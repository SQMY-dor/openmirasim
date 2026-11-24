//! The per-session actor: the only writer of a live session's event stream.
//!
//! One Tokio task per live session serializes every state transition: member commands
//! (prompt, cancel, approve, close) and harness events are handled one at a time, each
//! change is persisted through [`Db::append_events`] and only then broadcast.

use super::hub::EventHub;
use crate::approval::{self, ApprovalPolicy, PolicyRequest, Remembered, Verdict};
use crate::db::{ApprovalOutcome, ApprovalRule, Db, StoredApproval};
use futures::StreamExt;
use om_core::approval::{ApprovalDecision, ApprovalRecord, ApprovalStatus};
use om_core::content::Prompt;
use om_core::event::{EventPayload, SessionEvent, ToolKind};
use om_core::harness::HarnessCapabilities;
use om_core::harness::{Harness, HarnessEvent, HarnessSession, StartContext};
use om_core::ids::{ApprovalId, MemberId, QueuedPromptId, SessionId, TurnId};
use om_core::queue::{MAX_QUEUED_PROMPTS, QueueRemoval, QueuedPrompt, SubmitMode, SubmitOutcome};
use om_core::sandbox::Sandbox;
use om_core::session::{SessionConfig, SessionStatus, StopReason};
use om_core::{CoreError, CoreResult, now_ms};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

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
    /// Server shutdown: terminate the live harness process without permanently closing the session.
    Shutdown { reply: oneshot::Sender<()> },
    Send {
        member: Option<MemberId>,
        prompt: Prompt,
        mode: SubmitMode,
        reply: oneshot::Sender<CoreResult<SubmitOutcome>>,
    },
    ListQueue {
        reply: oneshot::Sender<CoreResult<Vec<QueuedPrompt>>>,
    },
    /// Appends an announcement (goal / dispatch event) in stream order.
    Announce {
        payload: EventPayload,
        reply: oneshot::Sender<CoreResult<()>>,
    },
    Withdraw {
        queued_id: QueuedPromptId,
        member: Option<MemberId>,
        reply: oneshot::Sender<CoreResult<()>>,
    },
    MoveQueued {
        queued_id: QueuedPromptId,
        position: usize,
        reply: oneshot::Sender<CoreResult<Vec<QueuedPrompt>>>,
    },
    ResumeQueue {
        reply: oneshot::Sender<CoreResult<()>>,
    },
}

pub(crate) struct ActorSpec {
    pub session_id: SessionId,
    pub config: SessionConfig,
    pub created_by: Option<MemberId>,
    pub status: SessionStatus,
    pub resume_native_session_id: Option<String>,
    pub harness: Arc<dyn Harness>,
    pub sandbox: Arc<dyn Sandbox>,
    pub policy: ApprovalPolicy,
}

struct Actor {
    db: Db,
    hub: Arc<EventHub>,
    id: SessionId,
    status: SessionStatus,
    turn: Option<TurnId>,
    pending: HashSet<ApprovalId>,
    session: Option<Box<dyn HarnessSession>>,
    config: SessionConfig,
    policy: ApprovalPolicy,
    /// Remembered "always" answers, by [`approval::rule_key`].
    remembered: HashMap<String, Remembered>,
    /// Kind and paths of tool calls seen this session, to classify permission
    /// requests that only reference a `tool_call_id`.
    tool_calls: HashMap<String, (ToolKind, Vec<String>)>,
    /// When each pending approval times out.
    deadlines: HashMap<ApprovalId, Instant>,
    capabilities: HarnessCapabilities,
    /// Prompts waiting for the current turn to end, front first (W2-3).
    queue: VecDeque<QueuedPrompt>,
    /// Set by a cancel: queued prompts wait until a member resumes or submits again.
    paused: bool,
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
                    self.deadlines.remove(&o.approval.record.id);
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
        if !self.queue.is_empty() {
            return Err(CoreError::InvalidInput(
                "prompts are queued; send with a submit mode instead".into(),
            ));
        }
        let turn = self.start_turn(member, prompt, None).await?;
        // An empty queue has nothing left to hold back.
        self.set_paused(false).await;
        Ok(turn)
    }

    /// Starts a turn. `from_queue` records which queued prompt it came from.
    async fn start_turn(
        &mut self,
        member: Option<MemberId>,
        prompt: Prompt,
        from_queue: Option<QueuedPromptId>,
    ) -> CoreResult<TurnId> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| CoreError::Harness("harness not started".into()))?;
        // Ask the harness first so a refused prompt leaves no trace in the stream.
        session.prompt(prompt.clone()).await?;
        let turn_id = TurnId::new();
        self.turn = Some(turn_id.clone());
        let mut payloads = Vec::new();
        if let Some(queued_id) = from_queue {
            payloads.push(EventPayload::QueueItemRemoved {
                queued_id,
                reason: QueueRemoval::Started {
                    turn_id: turn_id.clone(),
                },
            });
        }
        payloads.push(EventPayload::PromptSubmitted {
            turn_id: turn_id.clone(),
            member_id: member,
            prompt,
        });
        payloads.extend(self.status_change(SessionStatus::Running));
        self.append(payloads).await?;
        Ok(turn_id)
    }

    async fn set_paused(&mut self, paused: bool) {
        if self.paused != paused {
            self.paused = paused;
            self.record(vec![EventPayload::QueuePaused { paused }])
                .await;
        }
    }

    /// Adds a prompt to the queue (front or back) and returns its position.
    async fn enqueue(
        &mut self,
        member: Option<MemberId>,
        prompt: Prompt,
        mode: SubmitMode,
        front: bool,
    ) -> CoreResult<(QueuedPromptId, u32)> {
        if self.queue.len() >= MAX_QUEUED_PROMPTS {
            return Err(CoreError::LimitExceeded(format!(
                "a session queues at most {MAX_QUEUED_PROMPTS} prompts"
            )));
        }
        let queued = QueuedPrompt {
            id: QueuedPromptId::new(),
            member_id: member,
            prompt,
            mode,
            created_at: now_ms(),
        };
        let id = queued.id.clone();
        let position = if front { 0 } else { self.queue.len() };
        self.append(vec![EventPayload::PromptQueued {
            queued: queued.clone(),
            position: position as u32,
        }])
        .await?;
        self.queue.insert(position, queued);
        Ok((id, position as u32))
    }

    fn position_of(&self, id: &QueuedPromptId) -> Option<usize> {
        self.queue.iter().position(|q| &q.id == id)
    }

    /// Starts queued prompts while the session is idle and the queue is not paused. A
    /// prompt the harness refuses is dropped with `Failed` and the next one is tried.
    /// Returns what happened to each prompt it took off the queue.
    async fn drain(&mut self) -> Vec<(QueuedPromptId, Result<TurnId, String>)> {
        let mut out = Vec::new();
        while !self.paused
            && self.turn.is_none()
            && self.status == SessionStatus::Idle
            && let Some(next) = self.queue.pop_front()
        {
            let id = next.id.clone();
            match self
                .start_turn(next.member_id, next.prompt, Some(id.clone()))
                .await
            {
                Ok(turn) => out.push((id, Ok(turn))),
                Err(e) => {
                    self.record(vec![EventPayload::QueueItemRemoved {
                        queued_id: id.clone(),
                        reason: QueueRemoval::Failed {
                            error: e.to_string(),
                        },
                    }])
                    .await;
                    out.push((id, Err(e.to_string())));
                }
            }
        }
        out
    }

    /// Drops every queued prompt (session closing or agent gone).
    fn drop_queue_payloads(&mut self) -> Vec<EventPayload> {
        self.queue
            .drain(..)
            .map(|q| EventPayload::QueueItemRemoved {
                queued_id: q.id,
                reason: QueueRemoval::Dropped,
            })
            .collect()
    }

    async fn on_send(
        &mut self,
        member: Option<MemberId>,
        prompt: Prompt,
        mode: SubmitMode,
    ) -> CoreResult<SubmitOutcome> {
        if prompt.is_empty() {
            return Err(CoreError::InvalidInput("prompt is empty".into()));
        }
        match self.status {
            SessionStatus::Idle if self.turn.is_none() => {
                // Sending again resumes a paused queue. A plain `Queue` send still waits
                // behind what is already queued; `Next` / `Steer` go first. Nothing is
                // unpaused until the prompt is accepted (review P1-9).
                if mode == SubmitMode::Queue && !self.queue.is_empty() {
                    let (queued_id, _) = self.enqueue(member, prompt, mode, false).await?;
                    self.set_paused(false).await;
                    let drained = self.drain().await;
                    if let Some(position) = self.position_of(&queued_id) {
                        return Ok(SubmitOutcome::Queued {
                            queued_id,
                            position: position as u32,
                        });
                    }
                    // The queue ahead failed to start and ours was taken off too.
                    return match drained.into_iter().find(|(id, _)| id == &queued_id) {
                        Some((_, Ok(turn_id))) => Ok(SubmitOutcome::Started { turn_id }),
                        Some((_, Err(e))) => Err(CoreError::Harness(e)),
                        None => Err(CoreError::Harness("queued prompt was dropped".into())),
                    };
                }
                let turn_id = self.start_turn(member, prompt, None).await?;
                self.set_paused(false).await;
                Ok(SubmitOutcome::Started { turn_id })
            }
            SessionStatus::Running | SessionStatus::WaitingApproval if self.turn.is_some() => {
                match mode {
                    SubmitMode::Queue | SubmitMode::Next => {
                        let (queued_id, position) = self
                            .enqueue(member, prompt, mode, mode == SubmitMode::Next)
                            .await?;
                        Ok(SubmitOutcome::Queued {
                            queued_id,
                            position,
                        })
                    }
                    SubmitMode::Steer => self.steer(member, prompt).await,
                }
            }
            from => Err(CoreError::InvalidTransition {
                from,
                to: SessionStatus::Running,
            }),
        }
    }

    /// Steers the running turn: natively when the harness can, otherwise by cancelling
    /// the turn and running this prompt first once it has ended.
    async fn steer(
        &mut self,
        member: Option<MemberId>,
        prompt: Prompt,
    ) -> CoreResult<SubmitOutcome> {
        let turn_id = self.turn.clone().expect("steer requires a running turn");
        if self.capabilities.steer
            && let Some(s) = &self.session
        {
            match s.steer(prompt.clone()).await {
                Ok(()) => {
                    self.append(vec![EventPayload::PromptSteered {
                        turn_id: turn_id.clone(),
                        member_id: member,
                        prompt,
                    }])
                    .await?;
                    return Ok(SubmitOutcome::Steered { turn_id });
                }
                Err(CoreError::Unsupported { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        let (queued_id, _) = self
            .enqueue(member, prompt, SubmitMode::Steer, true)
            .await?;
        self.set_paused(false).await;
        if let Some(s) = &self.session
            && let Err(e) = s.cancel().await
        {
            tracing::warn!(session = %self.id, error = %e, "cancel for steer failed");
        }
        self.cancel_pending_approvals().await;
        Ok(SubmitOutcome::Interrupting { queued_id })
    }

    async fn on_withdraw(
        &mut self,
        queued_id: QueuedPromptId,
        member: Option<MemberId>,
    ) -> CoreResult<()> {
        let i = self
            .position_of(&queued_id)
            .ok_or_else(|| CoreError::NotFound(format!("queued prompt {queued_id}")))?;
        self.append(vec![EventPayload::QueueItemRemoved {
            queued_id,
            reason: QueueRemoval::Withdrawn { by: member },
        }])
        .await?;
        self.queue.remove(i);
        if self.queue.is_empty() {
            self.set_paused(false).await;
        }
        Ok(())
    }

    async fn on_move(
        &mut self,
        queued_id: QueuedPromptId,
        position: usize,
    ) -> CoreResult<Vec<QueuedPrompt>> {
        let i = self
            .position_of(&queued_id)
            .ok_or_else(|| CoreError::NotFound(format!("queued prompt {queued_id}")))?;
        let to = position.min(self.queue.len() - 1);
        if i != to {
            let mut next_queue = self.queue.clone();
            let item = next_queue.remove(i).expect("index checked");
            next_queue.insert(to, item);
            let order = next_queue.iter().map(|q| q.id.clone()).collect();
            self.append(vec![EventPayload::QueueReordered { order }])
                .await?;
            self.queue = next_queue;
        }
        Ok(self.queue.iter().cloned().collect())
    }

    async fn on_resume(&mut self) -> CoreResult<()> {
        self.set_paused(false).await;
        self.drain().await;
        Ok(())
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
        // "Stop" must not immediately start the next queued prompt.
        if !self.queue.is_empty() {
            self.set_paused(true).await;
        }
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
            .resolve_approval(&approval_id, decision.clone(), member.clone())
            .await?;
        self.remember(&outcome.approval.record, &decision, member)
            .await;
        Ok(self.finish_approval(outcome, decision).await)
    }

    /// Publishes a resolution, answers the agent and leaves `WaitingApproval` when
    /// nothing else is pending.
    async fn finish_approval(
        &mut self,
        outcome: ApprovalOutcome,
        decision: ApprovalDecision,
    ) -> ApprovalRecord {
        let approval_id = outcome.approval.record.id.clone();
        let for_agent = agent_decision(&outcome.approval.record.options, &decision);
        self.hub.publish(std::slice::from_ref(&outcome.event));
        self.pending.remove(&approval_id);
        self.deadlines.remove(&approval_id);
        if let Some(s) = &self.session
            && let Err(e) = s
                .respond_approval(&outcome.approval.harness_request_id, for_agent)
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
        outcome.approval.record
    }

    /// Stores a member's "allow always" / "reject always" answer for this session.
    async fn remember(
        &mut self,
        record: &ApprovalRecord,
        decision: &ApprovalDecision,
        member: Option<MemberId>,
    ) {
        if !self.policy.remember_always {
            return;
        }
        let ApprovalDecision::Selected { option_id } = decision else {
            return;
        };
        let Some(option) = record.options.iter().find(|o| &o.option_id == option_id) else {
            return;
        };
        let (Some(r), Some(key)) = (
            approval::remembers(option.kind),
            approval::rule_key(record.tool_kind, &record.title),
        ) else {
            return;
        };
        let rule = ApprovalRule {
            rule_key: key.clone(),
            allow: r.allow,
            option_kind: option.kind,
            created_by: member,
            created_at: now_ms(),
        };
        match self.db.remember_approval_rule(&self.id, rule).await {
            Ok(()) => {
                self.remembered.insert(key, r);
            }
            Err(e) => {
                tracing::error!(session = %self.id, error = %e, "failed to remember approval rule")
            }
        }
    }

    /// Rejects (or cancels) every pending approval whose deadline has passed.
    async fn expire_due(&mut self) {
        let now = Instant::now();
        let due: Vec<ApprovalId> = self
            .deadlines
            .iter()
            .filter(|(_, d)| **d <= now)
            .map(|(id, _)| id.clone())
            .collect();
        // A storage hiccup must not leave the approval waiting forever: retry shortly.
        let retry_at = now + std::time::Duration::from_secs(1);
        for id in due {
            self.deadlines.remove(&id);
            let decision = match self.db.get_approval(&id).await {
                Ok(a) => match ApprovalPolicy::timeout_option(&a.record.options) {
                    Some(option_id) => ApprovalDecision::Selected { option_id },
                    None => ApprovalDecision::Cancelled,
                },
                Err(CoreError::NotFound(_)) => {
                    self.pending.remove(&id);
                    continue;
                }
                Err(e) => {
                    tracing::error!(session = %self.id, error = %e, "failed to load approval");
                    self.deadlines.insert(id, retry_at);
                    continue;
                }
            };
            match self
                .db
                .resolve_approval_by_policy(&id, decision.clone(), "timeout".into())
                .await
            {
                Ok(outcome) => {
                    self.finish_approval(outcome, decision).await;
                }
                Err(CoreError::ApprovalAlreadyResolved(_)) => {
                    self.pending.remove(&id);
                }
                Err(e) => {
                    tracing::error!(session = %self.id, error = %e, "failed to expire approval");
                    self.deadlines.insert(id, retry_at);
                }
            }
        }
    }

    async fn load_rules(&mut self) {
        match self.db.approval_rules(&self.id).await {
            Ok(rules) => {
                self.remembered = rules
                    .into_iter()
                    .map(|r| (r.rule_key, Remembered { allow: r.allow }))
                    .collect();
            }
            Err(e) => {
                // Fail closed: without a fresh read, keep only remembered rejections so
                // a revoked "allow always" can never keep answering.
                self.remembered.retain(|_, r| !r.allow);
                tracing::error!(session = %self.id, error = %e, "failed to load approval rules")
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.values().min().copied()
    }

    async fn on_permission(
        &mut self,
        request_id: String,
        tool_call_id: Option<String>,
        title: String,
        kind: Option<ToolKind>,
        locations: Vec<String>,
        options: Vec<om_core::approval::ApprovalOption>,
    ) {
        let known = tool_call_id.as_ref().and_then(|id| self.tool_calls.get(id));
        let kind = kind.or_else(|| known.map(|(k, _)| *k));
        let locations = if locations.is_empty() {
            known.map(|(_, l)| l.clone()).unwrap_or_default()
        } else {
            locations
        };
        // Re-read remembered rules so a member revoking them takes effect at once.
        if self.policy.remember_always {
            self.load_rules().await;
        }
        let verdict = self.policy.evaluate(
            &PolicyRequest {
                sandbox: self.config.sandbox,
                cwd: &self.config.cwd,
                kind,
                locations: &locations,
                title: &title,
                options: &options,
            },
            &self.remembered,
        );
        let record = ApprovalRecord {
            id: ApprovalId::new(),
            session_id: self.id.clone(),
            tool_call_id,
            title,
            tool_kind: kind,
            locations,
            options,
            status: ApprovalStatus::Pending,
            decision: None,
            resolved_by: None,
            created_at: now_ms(),
            resolved_at: None,
            auto_rule: None,
        };
        let approval_id = record.id.clone();
        let stored = StoredApproval {
            record,
            harness_request_id: request_id.clone(),
        };
        let event = match self.db.create_approval(stored, self.turn.clone()).await {
            Ok(event) => event,
            Err(e) => {
                tracing::error!(session = %self.id, error = %e, "failed to store approval; rejecting");
                if let Some(s) = &self.session {
                    let _ = s
                        .respond_approval(&request_id, ApprovalDecision::Cancelled)
                        .await;
                }
                return;
            }
        };
        self.hub.publish(std::slice::from_ref(&event));
        self.pending.insert(approval_id.clone());

        if let Verdict::Auto { option_id, rule } = verdict {
            let decision = ApprovalDecision::Selected { option_id };
            match self
                .db
                .resolve_approval_by_policy(&approval_id, decision.clone(), rule)
                .await
            {
                Ok(outcome) => {
                    self.finish_approval(outcome, decision).await;
                    return;
                }
                Err(e) => {
                    tracing::error!(session = %self.id, error = %e, "auto-resolve failed; asking members");
                }
            }
        }

        if let Some(t) = self.policy.timeout {
            self.deadlines.insert(approval_id, Instant::now() + t);
        }
        if self.status == SessionStatus::Running
            && let Some(p) = self.status_change(SessionStatus::WaitingApproval)
        {
            self.record(vec![p]).await;
        }
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
        payloads.extend(self.drop_queue_payloads());
        if self.paused {
            self.paused = false;
            payloads.push(EventPayload::QueuePaused { paused: false });
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
            HarnessEvent::NativeSessionId(native) => {
                if let Err(e) = self.db.set_native_session_id(&self.id, native).await {
                    tracing::warn!(session = %self.id, error = %e, "native session id not saved");
                }
            }
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
                if self.tool_calls.len() < 1024 {
                    self.tool_calls.insert(
                        tool_call.tool_call_id.clone(),
                        (tool_call.kind, tool_call.locations.clone()),
                    );
                }
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
                kind,
                locations,
                options,
            } => {
                self.on_permission(request_id, tool_call_id, title, kind, locations, options)
                    .await;
            }
            HarnessEvent::TurnEnded { stop_reason, error } => {
                self.cancel_pending_approvals().await;
                self.tool_calls.clear();
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
                self.drain().await;
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
        self.tool_calls.clear();
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
        payloads.extend(self.drop_queue_payloads());
        if self.paused {
            self.paused = false;
            payloads.push(EventPayload::QueuePaused { paused: false });
        }
        if !self.status.is_terminal() {
            payloads.extend(self.status_change(SessionStatus::Failed));
        }
        self.record(payloads).await;
    }
}

struct ExitGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for ExitGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
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
    // Always revoke KeyProxy grant and remove live slot, even on panic or cancellation.
    let _exit_guard = ExitGuard(Some(on_exit));
    let mut actor = Actor {
        db,
        hub,
        id: spec.session_id.clone(),
        status: spec.status,
        turn: None,
        pending: HashSet::new(),
        session: None,
        config: spec.config.clone(),
        policy: spec.policy.clone(),
        remembered: HashMap::new(),
        tool_calls: HashMap::new(),
        deadlines: HashMap::new(),
        capabilities: spec.harness.capabilities(),
        queue: VecDeque::new(),
        paused: false,
    };

    // Move non-Starting sessions into Starting before racing `harness.start` against
    // `Command::Close`, so abandoning startup never leaves DB status out of sync with
    // `actor.status`.
    if actor.status != SessionStatus::Starting {
        let from = actor.status;
        if let Err(e) = actor
            .append(vec![EventPayload::StatusChanged {
                from,
                to: SessionStatus::Starting,
            }])
            .await
        {
            tracing::warn!(session = %actor.id, error = %e, "could not transition to Starting");
            commands.close();
            while let Some(cmd) = commands.recv().await {
                reject(cmd, CoreError::Harness("harness failed to start".into()));
            }
            return;
        }
    }

    // Starting can take a while (handshake timeouts): keep answering `Close` meanwhile
    // (review P2-5). Other commands wait (bounded) until the agent is up.
    const MAX_STARTUP_BUFFERED: usize = 64;
    let mut buffered = Vec::new();
    let mut close_reply = None;
    let mut shutdown_reply = None;
    let start_ctx = StartContext {
        session_id: spec.session_id.clone(),
        config: spec.config.clone(),
        created_by: spec.created_by.clone(),
        resume_native_session_id: spec.resume_native_session_id.clone(),
        sandbox: spec.sandbox.clone(),
    };
    let start_result = {
        let start = spec.harness.start(start_ctx);
        tokio::pin!(start);
        loop {
            tokio::select! {
                r = &mut start => break Some(r),
                cmd = commands.recv() => match cmd {
                    Some(Command::Close { reply }) => {
                        close_reply = Some(reply);
                        break None;
                    }
                    Some(Command::Shutdown { reply }) => {
                        shutdown_reply = Some(reply);
                        break None;
                    }
                    Some(other) => {
                        if buffered.len() < MAX_STARTUP_BUFFERED {
                            buffered.push(other);
                        } else {
                            reject(
                                other,
                                CoreError::LimitExceeded(
                                    "too many commands queued while session is starting".into(),
                                ),
                            );
                        }
                    }
                    None => break None,
                },
            }
        }
    };
    let mut events = match start_result {
        Some(Ok(started)) => {
            let native = started.session.native_session_id();
            actor.session = Some(started.session);
            if let Err(e) = actor
                .append(vec![
                    EventPayload::HarnessReady {
                        capabilities: spec.harness.capabilities(),
                        native_session_id: native,
                    },
                    EventPayload::StatusChanged {
                        from: SessionStatus::Starting,
                        to: SessionStatus::Idle,
                    },
                ])
                .await
            {
                tracing::error!(session = %actor.id, error = %e, "failed to persist HarnessReady");
                if let Some(s) = actor.session.take() {
                    let _ = s.shutdown().await;
                }
                commands.close();
                for cmd in buffered {
                    reject(cmd, CoreError::Harness("harness failed to start".into()));
                }
                while let Some(cmd) = commands.recv().await {
                    reject(cmd, CoreError::Harness("harness failed to start".into()));
                }
                return;
            }
            started.events
        }
        Some(Err(e)) => {
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
            commands.close();
            for cmd in buffered {
                reject(cmd, CoreError::Harness("harness failed to start".into()));
            }
            while let Some(cmd) = commands.recv().await {
                reject(cmd, CoreError::Harness("harness failed to start".into()));
            }
            return;
        }
        None => {
            if let Some(reply) = close_reply {
                // Refresh from DB defensively in case a write already committed.
                if let Ok(s) = actor.db.get_session(&actor.id).await {
                    actor.status = s.status;
                }
                let _ = reply.send(actor.on_close().await);
            } else {
                if let Some(s) = actor.session.take() {
                    let _ = s.shutdown().await;
                }
                if let Some(reply) = shutdown_reply {
                    let _ = reply.send(());
                }
            }
            commands.close();
            for cmd in buffered {
                reject(cmd, CoreError::Harness("harness failed to start".into()));
            }
            while let Some(cmd) = commands.recv().await {
                reject(cmd, CoreError::Harness("harness failed to start".into()));
            }
            return;
        }
    };
    for cmd in buffered {
        if handle_command(&mut actor, cmd).await {
            if let Some(s) = actor.session.take() {
                let _ = s.shutdown().await;
            }
            commands.close();
            while let Ok(cmd) = commands.try_recv() {
                reject(
                    cmd,
                    CoreError::InvalidInput("session is no longer running".into()),
                );
            }
            return;
        }
    }

    loop {
        let deadline = actor.next_deadline();
        tokio::select! {
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                actor.expire_due().await;
            }
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
        Command::Shutdown { reply } => {
            if let Some(s) = actor.session.take() {
                let _ = s.shutdown().await;
            }
            let _ = reply.send(());
            true
        }
        Command::Send {
            member,
            prompt,
            mode,
            reply,
        } => {
            let _ = reply.send(actor.on_send(member, prompt, mode).await);
            false
        }
        Command::ListQueue { reply } => {
            let _ = reply.send(Ok(actor.queue.iter().cloned().collect()));
            false
        }
        Command::Withdraw {
            queued_id,
            member,
            reply,
        } => {
            let _ = reply.send(actor.on_withdraw(queued_id, member).await);
            false
        }
        Command::MoveQueued {
            queued_id,
            position,
            reply,
        } => {
            let _ = reply.send(actor.on_move(queued_id, position).await);
            false
        }
        Command::ResumeQueue { reply } => {
            let _ = reply.send(actor.on_resume().await);
            false
        }
        Command::Announce { payload, reply } => {
            let _ = reply.send(actor.append(vec![payload]).await.map(|_| ()));
            false
        }
    }
}

fn reject(cmd: Command, err: CoreError) {
    let msg = err.to_string();
    match cmd {
        Command::Prompt { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::Cancel { reply }
        | Command::Close { reply }
        | Command::ResumeQueue { reply }
        | Command::Announce { reply, .. }
        | Command::Withdraw { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::Send { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::ListQueue { reply } | Command::MoveQueued { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::ResolveApproval { reply, .. } => {
            let _ = reply.send(Err(CoreError::InvalidInput(msg)));
        }
        Command::Shutdown { reply } => {
            let _ = reply.send(());
        }
    }
}

/// The answer sent to the agent for a member's decision. "Always" choices are kept by
/// the server (so members can revoke them); the agent gets the matching one-off option
/// when it offered one, so it never caches a rule of its own.
fn agent_decision(
    options: &[om_core::approval::ApprovalOption],
    decision: &ApprovalDecision,
) -> ApprovalDecision {
    use om_core::approval::ApprovalOptionKind as K;
    let ApprovalDecision::Selected { option_id } = decision else {
        return decision.clone();
    };
    let Some(chosen) = options.iter().find(|o| &o.option_id == option_id) else {
        return decision.clone();
    };
    let once = match chosen.kind {
        K::AllowAlways => K::AllowOnce,
        K::RejectAlways => K::RejectOnce,
        _ => return decision.clone(),
    };
    options
        .iter()
        .find(|o| o.kind == once)
        .map(|o| ApprovalDecision::Selected {
            option_id: o.option_id.clone(),
        })
        .unwrap_or_else(|| decision.clone())
}

#[cfg(test)]
mod agent_decision_tests {
    use super::*;
    use om_core::approval::{ApprovalOption, ApprovalOptionKind as K};

    #[test]
    fn always_answers_reach_the_agent_as_one_offs() {
        let opts: Vec<_> = [
            ("a1", K::AllowOnce),
            ("aa", K::AllowAlways),
            ("ra", K::RejectAlways),
        ]
        .into_iter()
        .map(|(id, kind)| ApprovalOption {
            option_id: id.into(),
            name: id.into(),
            kind,
        })
        .collect();
        let sel = |id: &str| ApprovalDecision::Selected {
            option_id: id.into(),
        };
        assert_eq!(agent_decision(&opts, &sel("aa")), sel("a1"));
        assert_eq!(agent_decision(&opts, &sel("a1")), sel("a1"));
        // No one-off reject offered: the member's choice goes through unchanged.
        assert_eq!(agent_decision(&opts, &sel("ra")), sel("ra"));
        assert_eq!(
            agent_decision(&opts, &ApprovalDecision::Cancelled),
            ApprovalDecision::Cancelled
        );
    }
}
