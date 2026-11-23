//! Goal loops (W4-1): keep a session working until its objective is met.
//!
//! See [`om_core::goal`] for the contract. One driver task per active goal follows the
//! session's event stream; it only counts turns that started after the goal was set,
//! reads the agent's reply text for each, and decides whether to stop or continue.

use crate::db::Db;
use crate::orchestrator::{Orchestrator, Subscription};
use om_core::auth::Action;
use om_core::content::{ContentBlock, Prompt};
use om_core::event::EventPayload;
use om_core::goal::{DEFAULT_DONE_MARKER, Goal, GoalStatus, NewGoal};
use om_core::ids::{GoalId, MemberId, QueuedPromptId, SessionId, TurnId};
use om_core::queue::{QueueRemoval, SubmitMode, SubmitOutcome};
use om_core::session::{SessionStatus, StopReason};
use om_core::{CoreError, CoreResult, now_ms};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Handle to the goal loops. Cheap to clone.
#[derive(Clone)]
pub struct GoalRunner {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    orch: Orchestrator,
    /// Wakes a driver whose goal was stopped from outside.
    stops: Mutex<HashMap<GoalId, Arc<Notify>>>,
}

/// What a driver does after a turn.
enum Next {
    /// A continuation was sent; follow it.
    Continue(SubmitOutcome),
    End(GoalStatus, &'static str),
}

/// The goal's own prompt the driver follows (review P1-13): only turns started by the
/// goal's prompts count, never prompts other members sent meanwhile.
#[derive(Debug, Clone, PartialEq)]
enum Expect {
    /// Waiting in the queue under this id.
    Queued(QueuedPromptId),
    /// Running (or ran) as this turn.
    Turn(TurnId),
}

impl Expect {
    fn from_outcome(o: &SubmitOutcome) -> Option<Self> {
        match o {
            SubmitOutcome::Started { turn_id } | SubmitOutcome::Steered { turn_id } => {
                Some(Self::Turn(turn_id.clone()))
            }
            SubmitOutcome::Queued { queued_id, .. } | SubmitOutcome::Interrupting { queued_id } => {
                Some(Self::Queued(queued_id.clone()))
            }
        }
    }
}

impl GoalRunner {
    pub fn new(orch: Orchestrator) -> Self {
        Self {
            inner: Arc::new(Inner {
                db: orch.db().clone(),
                orch,
                stops: Mutex::default(),
            }),
        }
    }

    fn db(&self) -> &Db {
        &self.inner.db
    }

    /// Call once at server start: goals that were active when the server stopped are
    /// marked `stopped` (`server_restarted`). Returns how many.
    pub async fn recover(&self) -> CoreResult<usize> {
        let active = self.db().list_active_goals().await?;
        let mut n = 0;
        for g in active {
            if self
                .end(&g.id, GoalStatus::Stopped, "server_restarted", None)
                .await
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Sets a goal on a session and sends the opening prompt as `by`.
    pub async fn set_goal(
        &self,
        session_id: &SessionId,
        by: &MemberId,
        new: NewGoal,
    ) -> CoreResult<Goal> {
        let now = now_ms();
        let max_turns = new.validate(now)?;
        let session = self.db().get_session(session_id).await?;
        if matches!(
            session.status,
            SessionStatus::Failed | SessionStatus::Closed
        ) {
            return Err(CoreError::InvalidInput("the session has ended".into()));
        }
        let m = self
            .db()
            .get_member(by)
            .await
            .map_err(|_| CoreError::Forbidden("unknown member".into()))?;
        if !m.is_active() || m.space_id != session.space_id {
            return Err(CoreError::Forbidden(
                "member is not active in this space".into(),
            ));
        }
        if !m.role.can(Action::PromptSession) {
            return Err(CoreError::Forbidden(
                "role does not allow prompting sessions".into(),
            ));
        }
        let goal = self
            .db()
            .create_goal(Goal {
                id: GoalId::new(),
                space_id: session.space_id.clone(),
                session_id: session_id.clone(),
                objective: new.objective.trim().to_owned(),
                done_marker: DEFAULT_DONE_MARKER.to_owned(),
                max_turns,
                turns_used: 0,
                deadline_at: new.deadline_at,
                created_by: by.clone(),
                status: GoalStatus::Active,
                ended_reason: None,
                created_at: now,
                updated_at: now,
            })
            .await?;
        self.publish(&goal).await;
        self.audit(
            Some(by.clone()),
            "goal.set",
            &goal,
            serde_json::json!({
                "max_turns": goal.max_turns,
                "deadline_at": goal.deadline_at,
            }),
        )
        .await;

        // Subscribe before prompting so the driver sees the goal's first turn.
        let sub = match self
            .inner
            .orch
            .subscribe(session_id, session.head_seq)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.end(&goal.id, GoalStatus::Failed, "subscribe_failed", None)
                    .await;
                return Err(e);
            }
        };
        let stop = Arc::new(Notify::new());
        self.inner
            .stops
            .lock()
            .expect("goal stops lock")
            .insert(goal.id.clone(), stop.clone());
        // A stop that landed before the driver was registered must not be lost.
        if !self.db().get_goal(&goal.id).await?.status.is_active() {
            self.inner
                .stops
                .lock()
                .expect("goal stops lock")
                .remove(&goal.id);
            return self.db().get_goal(&goal.id).await;
        }
        // The driver sends the opening prompt itself, so it knows which queued prompt /
        // turn is the goal's before it reads any event of it.
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
        let this = self.clone();
        let driven = goal.clone();
        tokio::spawn(async move { this.drive(driven, sub, stop, opened_tx).await });
        match opened_rx.await {
            Ok(Ok(())) => Ok(goal),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(CoreError::Harness("goal driver stopped".into())),
        }
    }

    /// Stops an active goal on behalf of `by`, who must be an active member of the
    /// goal's space allowed to prompt sessions (the same right that lets them cancel a
    /// turn). The turn in progress, if any, keeps running; no further prompt is sent.
    pub async fn stop_goal(&self, id: &GoalId, by: &MemberId) -> CoreResult<Goal> {
        let goal = self.db().get_goal(id).await?;
        let m = self
            .db()
            .get_member(by)
            .await
            .map_err(|_| CoreError::Forbidden("unknown member".into()))?;
        if !m.is_active() || m.space_id != goal.space_id {
            // Do not reveal goals of other spaces.
            return Err(CoreError::NotFound(format!("goal {id}")));
        }
        if !m.role.can(Action::PromptSession) {
            return Err(CoreError::Forbidden(
                "role does not allow prompting sessions".into(),
            ));
        }
        if goal.status.is_active() {
            self.end(
                id,
                GoalStatus::Stopped,
                "stopped_by_member",
                Some(by.clone()),
            )
            .await;
        }
        self.db().get_goal(id).await
    }

    pub async fn get(&self, id: &GoalId) -> CoreResult<Goal> {
        self.db().get_goal(id).await
    }

    /// A session's goals, newest first.
    pub async fn list(&self, session_id: &SessionId) -> CoreResult<Vec<Goal>> {
        self.db().list_goals(session_id).await
    }

    /// Ends the goal on the server's behalf (first caller wins) and wakes its driver.
    async fn finish(&self, id: &GoalId, status: GoalStatus, reason: &'static str) {
        self.end(id, status, reason, None).await;
    }

    /// Ends the goal (first caller wins), wakes its driver, and — only for the caller
    /// that actually ended it — announces and audits the outcome. Returns whether this
    /// call ended it.
    async fn end(
        &self,
        id: &GoalId,
        status: GoalStatus,
        reason: &'static str,
        actor: Option<MemberId>,
    ) -> bool {
        let ended = match self.db().end_goal(id, status, reason).await {
            Ok(ended) => ended,
            Err(e) => {
                tracing::warn!(goal = %id, error = %e, "ending goal failed");
                false
            }
        };
        if let Some(n) = self.inner.stops.lock().expect("goal stops lock").remove(id) {
            n.notify_one();
        }
        if ended {
            tracing::info!(goal = %id, ?status, reason, "goal ended");
            if let Ok(goal) = self.db().get_goal(id).await {
                self.publish(&goal).await;
                self.audit(
                    actor,
                    "goal.ended",
                    &goal,
                    serde_json::json!({
                        "status": goal.status,
                        "reason": reason,
                        "turns_used": goal.turns_used,
                    }),
                )
                .await;
            }
        }
        ended
    }

    /// Appends a `goal_updated` event to the goal's session (best effort).
    async fn publish(&self, goal: &Goal) {
        let payload = EventPayload::GoalUpdated { goal: goal.clone() };
        if let Err(e) = self.inner.orch.announce(&goal.session_id, payload).await {
            tracing::warn!(goal = %goal.id, error = %e, "goal event not recorded");
        }
    }

    /// Writes an audit entry about `goal` (best effort).
    async fn audit(
        &self,
        actor: Option<MemberId>,
        action: &str,
        goal: &Goal,
        mut detail: serde_json::Value,
    ) {
        detail["session_id"] = serde_json::json!(goal.session_id);
        if let Err(e) = self
            .db()
            .audit(actor, action, goal.id.as_str(), detail)
            .await
        {
            tracing::warn!(goal = %goal.id, error = %e, "goal audit entry not written");
        }
    }

    async fn drive(
        &self,
        goal: Goal,
        mut sub: Subscription,
        stop: Arc<Notify>,
        opened: tokio::sync::oneshot::Sender<CoreResult<()>>,
    ) {
        let opening = self
            .inner
            .orch
            .send_prompt(
                &goal.session_id,
                Some(goal.created_by.clone()),
                Prompt::text(goal.opening_prompt()),
                SubmitMode::Queue,
            )
            .await;
        let mut expect = match opening {
            Ok(outcome) => {
                let _ = opened.send(Ok(()));
                Expect::from_outcome(&outcome)
            }
            Err(e) => {
                self.finish(&goal.id, GoalStatus::Failed, "prompt_rejected")
                    .await;
                let _ = opened.send(Err(e));
                return;
            }
        };
        // The deadline ends the goal even while a turn runs (review P1-13).
        let deadline = goal.deadline_at.map(|d| {
            let left = (d - now_ms()).max(0) as u64;
            tokio::time::Instant::now() + std::time::Duration::from_millis(left)
        });
        let mut reply = String::new();
        loop {
            let ev = tokio::select! {
                _ = stop.notified() => return,
                _ = async {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.finish(&goal.id, GoalStatus::Expired, "deadline").await;
                    return;
                }
                ev = sub.next() => ev,
            };
            let ev = match ev {
                Ok(Some(ev)) => ev,
                Ok(None) | Err(_) => {
                    self.finish(&goal.id, GoalStatus::Failed, "session_ended")
                        .await;
                    return;
                }
            };
            match ev.payload {
                EventPayload::QueueItemRemoved { queued_id, reason }
                    if expect == Some(Expect::Queued(queued_id.clone())) =>
                {
                    match reason {
                        QueueRemoval::Started { turn_id } => {
                            expect = Some(Expect::Turn(turn_id));
                            reply.clear();
                        }
                        QueueRemoval::Failed { .. } => {
                            self.finish(&goal.id, GoalStatus::Failed, "prompt_rejected")
                                .await;
                            return;
                        }
                        _ => {
                            self.finish(&goal.id, GoalStatus::Stopped, "prompt_withdrawn")
                                .await;
                            return;
                        }
                    }
                }
                EventPayload::PromptSubmitted { turn_id, .. }
                    if expect == Some(Expect::Turn(turn_id.clone())) =>
                {
                    reply.clear();
                }
                EventPayload::AgentMessageChunk {
                    turn_id,
                    content: ContentBlock::Text { text },
                } if expect == Some(Expect::Turn(turn_id.clone())) => reply.push_str(&text),
                EventPayload::StatusChanged {
                    to: SessionStatus::Failed | SessionStatus::Closed,
                    ..
                } => {
                    self.finish(&goal.id, GoalStatus::Failed, "session_ended")
                        .await;
                    return;
                }
                EventPayload::TurnEnded {
                    turn_id,
                    stop_reason,
                    ..
                } if expect == Some(Expect::Turn(turn_id.clone())) => {
                    match self.after_turn(&goal.id, stop_reason, &reply).await {
                        Ok(Next::Continue(outcome)) => {
                            expect = Expect::from_outcome(&outcome);
                            reply.clear();
                        }
                        Ok(Next::End(status, reason)) => {
                            self.finish(&goal.id, status, reason).await;
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(goal = %goal.id, error = %e, "goal step failed");
                            self.finish(&goal.id, GoalStatus::Failed, "internal_error")
                                .await;
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Decides what follows a finished turn; sends the continuation when the loop goes on.
    async fn after_turn(
        &self,
        id: &GoalId,
        stop_reason: StopReason,
        reply: &str,
    ) -> CoreResult<Next> {
        let goal = self.db().get_goal(id).await?;
        if !goal.status.is_active() {
            return Ok(Next::End(goal.status, "already_ended"));
        }
        let turns_used = goal.turns_used + 1;
        if !self.db().bump_goal_turns(id, turns_used).await? {
            return Ok(Next::End(GoalStatus::Stopped, "already_ended"));
        }
        if goal.is_met_by(reply) {
            return Ok(Next::End(GoalStatus::Achieved, "done_marker"));
        }
        match stop_reason {
            StopReason::Cancelled => return Ok(Next::End(GoalStatus::Stopped, "turn_cancelled")),
            StopReason::Refusal => return Ok(Next::End(GoalStatus::Stopped, "agent_refused")),
            StopReason::Failed => return Ok(Next::End(GoalStatus::Failed, "turn_failed")),
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests => {}
        }
        if turns_used >= goal.max_turns {
            return Ok(Next::End(GoalStatus::Exhausted, "max_turns"));
        }
        if goal.deadline_at.is_some_and(|d| now_ms() >= d) {
            return Ok(Next::End(GoalStatus::Expired, "deadline"));
        }
        // Re-check the owner before acting for them again.
        match self.db().get_member(&goal.created_by).await {
            Ok(m) if !m.is_active() || m.space_id != goal.space_id => {
                return Ok(Next::End(GoalStatus::Stopped, "owner_disabled"));
            }
            Ok(m) if !m.role.can(Action::PromptSession) => {
                return Ok(Next::End(GoalStatus::Stopped, "owner_not_allowed"));
            }
            Ok(_) => {}
            Err(CoreError::NotFound(_)) => {
                return Ok(Next::End(GoalStatus::Stopped, "owner_disabled"));
            }
            Err(e) => return Err(e),
        }
        let next = Goal {
            turns_used,
            updated_at: now_ms(),
            ..goal
        };
        self.publish(&next).await;
        match self
            .inner
            .orch
            .send_prompt(
                &next.session_id,
                Some(next.created_by.clone()),
                Prompt::text(next.continuation_prompt()),
                SubmitMode::Queue,
            )
            .await
        {
            Ok(outcome) => Ok(Next::Continue(outcome)),
            Err(e) => {
                tracing::warn!(goal = %id, error = %e, "continuation rejected");
                Ok(Next::End(GoalStatus::Failed, "prompt_rejected"))
            }
        }
    }
}
