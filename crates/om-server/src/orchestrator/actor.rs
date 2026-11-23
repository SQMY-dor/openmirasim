//! The per-session actor: the only writer of a live session's event stream.
//!
//! One Tokio task per live session serializes every state transition: member commands
//! (prompt, cancel, approve, close) and harness events are handled one at a time, each
//! change is persisted through [`Db::append_events`] and only then broadcast.

use super::hub::EventHub;
use crate::db::{Db, StoredApproval};
use futures::StreamExt;
use om_core::approval::{ApprovalDecision, ApprovalRecord, ApprovalStatus};
use om_core::content::Prompt;
use om_core::event::{EventPayload, SessionEvent};
use om_core::harness::{Harness, HarnessEvent, HarnessEventStream, HarnessSession, StartContext};
use om_core::ids::{ApprovalId, MemberId, SessionId, TurnId};
use om_core::sandbox::Sandbox;
use om_core::session::{SessionConfig, SessionStatus, StopReason};
use om_core::{CoreError, CoreResult, now_ms};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(crate) enum Command {
    Prompt {
        member: Option<MemberId>,
        prompt: Prompt,
        reply: oneshot::Sender<CoreResult<TurnId>>,
    },
    Cancel {
        reply: oneshot::Sender<CoreResult<()>>,
    },
    ResolveApproval {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        member: Option<MemberId>,
        reply: oneshot::Sender<CoreResult<ApprovalRecord>>,
    },
    Close {
        reply: oneshot::Sender<CoreResult<()>>,
    },
}

pub(crate) struct ActorSpec {
    pub session_id: SessionId,
    pub config: SessionConfig,
    pub status: SessionStatus,
    pub resume_native_session_id: Option<String>,
    pub harness: Arc<dyn Harness>,
    pub sandbox: Arc<dyn Sandbox>,
}

struct Actor {
    db: Db,
    hub: Arc<EventHub>,
    id: SessionId,
    status: SessionStatus,
    turn: Option<TurnId>,
    pending: HashSet<ApprovalId>,
    session: Option<Box<dyn HarnessSession>>,
}

impl Actor {
    async fn append(&mut self, payloads: Vec<EventPayload>) -> CoreResult<Vec<SessionEvent>> {
        let events = self.db.append_events(&self.id, payloads).await?;
        for ev in &events {
            if let EventPayload::StatusChanged { to, .. } = ev.payload {
                self.status = to;
            }
        }
        self.hub.publish(&events);
        Ok(events)
    }

    fn status_change(&self, to: SessionStatus) -> Option<EventPayload> {
        (self.status != to).then_some(EventPayload::StatusChanged {
            from: self.status,
            to,
        })
    }

    /// Appends payloads, logging (not propagating) storage failures in background paths.
    async fn record(&mut self, payloads: Vec<EventPayload>) {
        if let Err(e) = self.append(payloads).await {
            tracing::error!(session = %self.id, error = %e, "failed to persist session event");
        }
    }

    async fn cancel_pending_approvals(&mut self) {
        match self.db.cancel_pending_approvals(&self.id).await {
            Ok(outcomes) => {
                let events: Vec<_> = outcomes.iter().map(|o| o.event.clone()).collect();
                self.hub.publish(&events);
                for o in outcomes {
                    self.pending.remove(&o.approval.record.id);
                    if let Some(s) = &self.session {
                        let _ = s
                            .respond_approval(
                                &o.approval.harness_request_id,
                                ApprovalDecision::Cancelled,
                            )
                            .await;
                    }
                }
            }
            Err(e) => tracing::error!(session = %self.id, error = %e, "failed to cancel approvals"),
        }
    }

    async fn on_prompt(&mut self, member: Option<MemberId>, prompt: Prompt) -> CoreResult<TurnId> {
        if prompt.is_empty() {
            return Err(CoreError::InvalidInput("prompt is empty".into()));
        }
        if !self.status.accepts_prompt() {
            return Err(CoreError::InvalidTransition {
                from: self.status,
                to: SessionStatus::Running,
            });
        }
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| CoreError::Harness("harness not started".into()))?;
        // Ask the harness first so a refused prompt leaves no trace in the stream.
        session.prompt(prompt.clone()).await?;
        let turn_id = TurnId::new();
        self.turn = Some(turn_id.clone());
        let mut payloads = vec![EventPayload::PromptSubmitted {
            turn_id: turn_id.clone(),
            member_id: member,
            prompt,
        }];
        payloads.extend(self.status_change(SessionStatus::Running));
        self.append(payloads).await?;
        Ok(turn_id)
    }

    async fn on_cancel(&mut self) -> CoreResult<()> {
        if self.turn.is_none() {
            return Err(CoreError::InvalidTransition {
                from: self.status,
                to: SessionStatus::Idle,
            });
        }
        if let Some(s) = &self.session {
            s.cancel().await?;
        }
        self.cancel_pending_approvals().await;
        Ok(())
    }

    async fn on_resolve(
        &mut self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        member: Option<MemberId>,
    ) -> CoreResult<ApprovalRecord> {
        let outcome = self
            .db
            .resolve_approval(&approval_id, decision.clone(), member)
            .await?;
        self.hub.publish(std::slice::from_ref(&outcome.event));
        self.pending.remove(&approval_id);
        if let Some(s) = &self.session
            && let Err(e) = s
                .respond_approval(&outcome.approval.harness_request_id, decision)
                .await
        {
            tracing::warn!(session = %self.id, error = %e, "harness rejected approval answer");
        }
        if self.pending.is_empty()
            && self.status == SessionStatus::WaitingApproval
            && let Some(p) = self.status_change(SessionStatus::Running)
        {
            self.record(vec![p]).await;
        }
        Ok(outcome.approval.record)
    }

    async fn on_close(&mut self) -> CoreResult<()> {
        if self.turn.is_some()
            && let Some(s) = &self.session
        {
            let _ = s.cancel().await;
        }
        self.cancel_pending_approvals().await;
        if let Some(s) = self.session.take() {
            let _ = s.shutdown().await;
        }
        let mut payloads = Vec::new();
        if let Some(turn_id) = self.turn.take() {
            payloads.push(EventPayload::TurnEnded {
                turn_id,
                stop_reason: StopReason::Cancelled,
                error: Some("session closed".into()),
            });
        }
        payloads.extend(self.status_change(SessionStatus::Closed));
        self.append(payloads).await?;
        Ok(())
    }

    /// Handles one harness event. Returns `false` when the harness is gone.
    async fn on_harness_event(&mut self, ev: HarnessEvent) -> bool {
        let turn = self.turn.clone();
        let turn_payload = |f: &dyn Fn(TurnId) -> EventPayload| turn.clone().map(f);
        match ev {
            HarnessEvent::NativeSessionId(_) => {}
            HarnessEvent::MessageChunk(content) => {
                if let Some(p) = turn_payload(&|turn_id| EventPayload::AgentMessageChunk {
                    turn_id,
                    content: content.clone(),
                }) {
                    self.record(vec![p]).await;
                }
            }
            HarnessEvent::ThoughtChunk(content) => {
                if let Some(p) = turn_payload(&|turn_id| EventPayload::AgentThoughtChunk {
                    turn_id,
                    content: content.clone(),
                }) {
                    self.record(vec![p]).await;
                }
            }
            HarnessEvent::ToolCall(tool_call) => {
                if let Some(p) = turn_payload(&|turn_id| EventPayload::ToolCall {
                    turn_id,
                    tool_call: tool_call.clone(),
                }) {
                    self.record(vec![p]).await;
                }
            }
            HarnessEvent::ToolCallUpdate(update) => {
                if let Some(p) = turn_payload(&|turn_id| EventPayload::ToolCallUpdate {
                    turn_id,
                    update: update.clone(),
                }) {
                    self.record(vec![p]).await;
                }
            }
            HarnessEvent::Plan(entries) => {
                if let Some(p) = turn_payload(&|turn_id| EventPayload::Plan {
                    turn_id,
                    entries: entries.clone(),
                }) {
                    self.record(vec![p]).await;
                }
            }
            HarnessEvent::PermissionRequested {
                request_id,
                tool_call_id,
                title,
                options,
            } => {
                let record = ApprovalRecord {
                    id: ApprovalId::new(),
                    session_id: self.id.clone(),
                    tool_call_id,
                    title,
                    options,
                    status: ApprovalStatus::Pending,
                    decision: None,
                    resolved_by: None,
                    created_at: now_ms(),
                    resolved_at: None,
                };
                let approval_id = record.id.clone();
                let stored = StoredApproval {
                    record,
                    harness_request_id: request_id.clone(),
                };
                match self.db.create_approval(stored, self.turn.clone()).await {
                    Ok(event) => {
                        self.hub.publish(std::slice::from_ref(&event));
                        self.pending.insert(approval_id);
                        if self.status == SessionStatus::Running
                            && let Some(p) = self.status_change(SessionStatus::WaitingApproval)
                        {
                            self.record(vec![p]).await;
                        }
                    }
                    Err(e) => {
                        tracing::error!(session = %self.id, error = %e, "failed to store approval; rejecting");
                        if let Some(s) = &self.session {
                            let _ = s
                                .respond_approval(&request_id, ApprovalDecision::Cancelled)
                                .await;
                        }
                    }
                }
            }
            HarnessEvent::TurnEnded { stop_reason, error } => {
                self.cancel_pending_approvals().await;
                let mut payloads = Vec::new();
                if let Some(turn_id) = self.turn.take() {
                    payloads.push(EventPayload::TurnEnded {
                        turn_id,
                        stop_reason,
                        error,
                    });
                }
                if matches!(
                    self.status,
                    SessionStatus::Running | SessionStatus::WaitingApproval
                ) {
                    payloads.extend(self.status_change(SessionStatus::Idle));
                }
                self.record(payloads).await;
            }
            HarnessEvent::Exited { error } => {
                self.on_harness_gone(error).await;
                return false;
            }
        }
        true
    }

    async fn on_harness_gone(&mut self, error: Option<String>) {
        self.session = None;
        self.cancel_pending_approvals().await;
        let mut payloads = Vec::new();
        if let Some(turn_id) = self.turn.take() {
            payloads.push(EventPayload::TurnEnded {
                turn_id,
                stop_reason: StopReason::Failed,
                error: error.clone(),
            });
        }
        payloads.push(EventPayload::Error {
            message: error.unwrap_or_else(|| "agent process exited".into()),
        });
        if !self.status.is_terminal() {
            payloads.extend(self.status_change(SessionStatus::Failed));
        }
        self.record(payloads).await;
    }
}

/// Starts the harness and runs the actor loop until the session closes or fails.
pub(crate) async fn run(
    db: Db,
    hub: Arc<EventHub>,
    spec: ActorSpec,
    mut commands: mpsc::Receiver<Command>,
    on_exit: impl FnOnce() + Send + 'static,
) {
    let mut actor = Actor {
        db,
        hub,
        id: spec.session_id.clone(),
        status: spec.status,
        turn: None,
        pending: HashSet::new(),
        session: None,
    };

    let events = start_harness(&mut actor, &spec).await;
    let Some(mut events) = events else {
        // Fail queued commands so callers do not hang.
        commands.close();
        while let Some(cmd) = commands.recv().await {
            reject(cmd, CoreError::Harness("harness failed to start".into()));
        }
        on_exit();
        return;
    };

    loop {
        tokio::select! {
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    // Orchestrator dropped: shut the agent down.
                    if let Some(s) = actor.session.take() {
                        let _ = s.shutdown().await;
                    }
                    break;
                };
                if handle_command(&mut actor, cmd).await {
                    break;
                }
            }
            ev = events.next() => {
                let alive = match ev {
                    Some(ev) => actor.on_harness_event(ev).await,
                    None => {
                        actor.on_harness_gone(Some("agent event stream ended".into())).await;
                        false
                    }
                };
                if !alive {
                    break;
                }
            }
        }
    }
    commands.close();
    while let Ok(cmd) = commands.try_recv() {
        reject(
            cmd,
            CoreError::InvalidInput("session is no longer running".into()),
        );
    }
    on_exit();
}

async fn start_harness(actor: &mut Actor, spec: &ActorSpec) -> Option<HarnessEventStream> {
    if actor.status == SessionStatus::Failed {
        actor
            .record(vec![EventPayload::StatusChanged {
                from: SessionStatus::Failed,
                to: SessionStatus::Starting,
            }])
            .await;
    }
    let ctx = StartContext {
        session_id: spec.session_id.clone(),
        config: spec.config.clone(),
        resume_native_session_id: spec.resume_native_session_id.clone(),
        sandbox: spec.sandbox.clone(),
    };
    match spec.harness.start(ctx).await {
        Ok(started) => {
            let native = started.session.native_session_id();
            actor.session = Some(started.session);
            actor
                .record(vec![
                    EventPayload::HarnessReady {
                        capabilities: spec.harness.capabilities(),
                        native_session_id: native,
                    },
                    EventPayload::StatusChanged {
                        from: SessionStatus::Starting,
                        to: SessionStatus::Idle,
                    },
                ])
                .await;
            Some(started.events)
        }
        Err(e) => {
            actor
                .record(vec![
                    EventPayload::Error {
                        message: format!("failed to start harness `{}`: {e}", spec.harness.id()),
                    },
                    EventPayload::StatusChanged {
                        from: SessionStatus::Starting,
                        to: SessionStatus::Failed,
                    },
                ])
                .await;
            None
        }
    }
}

/// Returns `true` when the actor should stop.
async fn handle_command(actor: &mut Actor, cmd: Command) -> bool {
    match cmd {
        Command::Prompt {
            member,
            prompt,
            reply,
        } => {
            let _ = reply.send(actor.on_prompt(member, prompt).await);
            false
        }
        Command::Cancel { reply } => {
            let _ = reply.send(actor.on_cancel().await);
            false
        }
        Command::ResolveApproval {
            approval_id,
            decision,
            member,
            reply,
        } => {
            let _ = reply.send(actor.on_resolve(approval_id, decision, member).await);
            false
        }
        Command::Close { reply } => {
            let _ = reply.send(actor.on_close().await);
            true
        }
    }
}

fn reject(cmd: Command, err: CoreError) {
    let msg = err.to_string();
    match cmd {
        Command::Prompt { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::Cancel { reply } | Command::Close { reply } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::ResolveApproval { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
    }
}
