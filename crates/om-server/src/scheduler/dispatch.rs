//! Session-to-Session dispatch (W4-1). See [`om_core::dispatch`] for the contract.

use super::wait_ready;
use crate::db::Db;
use crate::orchestrator::Orchestrator;
use om_core::auth::Action;
use om_core::dispatch::{
    Dispatch, DispatchTarget, MAX_DISPATCH_DEPTH, MAX_DISPATCH_FANOUT, NewDispatch,
};
use om_core::event::EventPayload;
use om_core::ids::{DispatchId, MemberId, SessionId};
use om_core::queue::{SubmitMode, SubmitOutcome};
use om_core::session::SessionStatus;
use om_core::{CoreError, CoreResult, now_ms};
use std::time::Duration;

const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Most dispatches `list` returns.
pub const MAX_DISPATCH_PAGE: usize = 200;

/// Routes prompts between sessions. Cheap to clone.
#[derive(Clone)]
pub struct Dispatcher {
    db: Db,
    orch: Orchestrator,
    ready_timeout: Duration,
}

impl Dispatcher {
    pub fn new(orch: Orchestrator) -> Self {
        Self::with_ready_timeout(orch, DEFAULT_READY_TIMEOUT)
    }

    pub fn with_ready_timeout(orch: Orchestrator, ready_timeout: Duration) -> Self {
        Self {
            db: orch.db().clone(),
            orch,
            ready_timeout,
        }
    }

    /// A member dispatching by hand, acting as themself.
    pub async fn dispatch_as(&self, by: &MemberId, req: NewDispatch) -> CoreResult<Dispatch> {
        self.dispatch(Some(by.clone()), req).await
    }

    /// An agent dispatching from inside `req.from_session`: acts as the member whose
    /// prompt started that session's latest turn.
    pub async fn dispatch_from_agent(&self, req: NewDispatch) -> CoreResult<Dispatch> {
        self.dispatch(None, req).await
    }

    /// Dispatches sent from or received by a session, newest first.
    pub async fn list(&self, session_id: &SessionId, limit: usize) -> CoreResult<Vec<Dispatch>> {
        self.db
            .list_dispatches(session_id, limit.clamp(1, MAX_DISPATCH_PAGE))
            .await
    }

    async fn dispatch(&self, by: Option<MemberId>, req: NewDispatch) -> CoreResult<Dispatch> {
        req.validate()?;
        let from = self.db.get_session(&req.from_session).await?;
        let by_member = by.is_some();
        let (member_id, turn_members, depth, fanout) = match by {
            // A member dispatching by hand starts a new chain.
            Some(m) => {
                self.check_member(&m, &from.space_id).await?;
                (m.clone(), vec![m], 1, None)
            }
            // An agent extends the chain of the turn it runs in, and only while it runs.
            None => {
                if !matches!(
                    from.status,
                    SessionStatus::Running | SessionStatus::WaitingApproval
                ) {
                    return Err(CoreError::InvalidInput(
                        "an agent can only dispatch while its turn runs".into(),
                    ));
                }
                let turn = self.db.turn_context(&from.id).await?.ok_or_else(|| {
                    CoreError::Forbidden("the source session has no turn to act for".into())
                })?;
                // Everyone who instructed this turn must be allowed; act as its starter.
                let mut acting = None;
                let mut members = Vec::with_capacity(turn.members.len());
                for m in &turn.members {
                    let m = m.clone().ok_or_else(|| {
                        CoreError::Forbidden(
                            "an instruction in this turn came from no member, so it cannot dispatch"
                                .into(),
                        )
                    })?;
                    self.check_member(&m, &from.space_id).await?;
                    acting.get_or_insert(m.clone());
                    if !members.contains(&m) {
                        members.push(m);
                    }
                }
                let acting = acting.expect("a turn has a starter");
                (
                    acting,
                    members,
                    turn.depth + 1,
                    Some((turn.turn_id, MAX_DISPATCH_FANOUT)),
                )
            }
        };
        if depth > MAX_DISPATCH_DEPTH {
            return Err(CoreError::LimitExceeded(format!(
                "dispatch chains are limited to {MAX_DISPATCH_DEPTH} hops"
            )));
        }

        let (to_session, created_session, mode) = match &req.target {
            DispatchTarget::Session { session_id, mode } => {
                let to = self.db.get_session(session_id).await?;
                if to.space_id != from.space_id {
                    return Err(CoreError::NotFound(format!("session {session_id}")));
                }
                if matches!(to.status, SessionStatus::Failed | SessionStatus::Closed) {
                    return Err(CoreError::InvalidInput(
                        "the target session has ended".into(),
                    ));
                }
                (session_id.clone(), false, *mode)
            }
            DispatchTarget::NewSession { config } => {
                // Every participant who instructed/steered the turn must hold CreateSession.
                for participant in &turn_members {
                    let pm = self.db.get_member(participant).await?;
                    if !pm.is_active() || !pm.role.can(Action::CreateSession) {
                        return Err(CoreError::Forbidden(
                            "role does not allow creating sessions".into(),
                        ));
                    }
                }
                let session_owner = if turn_members.len() == 1 {
                    Some(member_id.clone())
                } else {
                    None
                };
                let created = self
                    .orch
                    .create_session(from.space_id.clone(), config.clone(), session_owner)
                    .await?;
                (created.id, true, SubmitMode::Queue)
            }
        };
        // Guard against HTTP caller cancellation or panic mid-delivery so neither the
        // `submit = 'null'` reservation nor a newly created session is leaked.
        struct DispatchGuard {
            db: Db,
            orch: Orchestrator,
            dispatch_id: Option<DispatchId>,
            created_session: Option<SessionId>,
        }
        impl Drop for DispatchGuard {
            fn drop(&mut self) {
                let dispatch_id = self.dispatch_id.take();
                let created_session = self.created_session.take();
                if dispatch_id.is_some() || created_session.is_some() {
                    let db = self.db.clone();
                    let orch = self.orch.clone();
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        handle.spawn(async move {
                            if let Some(id) = dispatch_id {
                                let _ = db.delete_dispatch(&id).await;
                            }
                            if let Some(sid) = created_session {
                                let _ = orch.close_session(&sid).await;
                            }
                        });
                    }
                }
            }
        }
        let mut guard = DispatchGuard {
            db: self.db.clone(),
            orch: self.orch.clone(),
            dispatch_id: None,
            created_session: created_session.then(|| to_session.clone()),
        };
        // Reserve the row before anything reaches the target, so the depth and fan-out
        // limits see this dispatch at once (review P1-14).
        let reserved = self
            .db
            .reserve_dispatch(
                Dispatch {
                    id: DispatchId::new(),
                    space_id: from.space_id.clone(),
                    from_session: from.id.clone(),
                    to_session: to_session.clone(),
                    member_id: member_id.clone(),
                    depth,
                    created_session,
                    submit: SubmitOutcome::Queued {
                        queued_id: om_core::ids::QueuedPromptId::new(),
                        position: 0,
                    },
                    created_at: now_ms(),
                },
                fanout,
            )
            .await;
        let reserved = match reserved {
            Ok(d) => d,
            Err(e) => {
                guard.created_session.take();
                self.abandon(created_session.then_some(&to_session)).await;
                return Err(e);
            }
        };
        guard.dispatch_id = Some(reserved.id.clone());
        let delivered = async {
            if created_session {
                wait_ready(&self.orch, &to_session, self.ready_timeout).await?;
            }
            self.orch
                .send_prompt(&to_session, Some(member_id.clone()), req.prompt, mode)
                .await
        }
        .await;
        let submit = match delivered {
            Ok(s) => s,
            Err(e) => {
                guard.dispatch_id.take();
                guard.created_session.take();
                let _ = self.db.delete_dispatch(&reserved.id).await;
                self.abandon(created_session.then_some(&to_session)).await;
                return Err(e);
            }
        };
        let d = self
            .db
            .complete_dispatch(Dispatch { submit, ..reserved })
            .await?;
        guard.dispatch_id.take();
        guard.created_session.take();
        self.record(&d, !by_member).await;
        Ok(d)
    }

    /// The member must be active in `space` and allowed to prompt sessions.
    async fn check_member(&self, id: &MemberId, space: &om_core::ids::SpaceId) -> CoreResult<()> {
        let m = self
            .db
            .get_member(id)
            .await
            .map_err(|_| CoreError::Forbidden("unknown member".into()))?;
        if !m.is_active() || &m.space_id != space {
            return Err(CoreError::Forbidden(
                "member is not active in this space".into(),
            ));
        }
        if !m.role.can(Action::PromptSession) {
            return Err(CoreError::Forbidden(
                "role does not allow prompting sessions".into(),
            ));
        }
        Ok(())
    }

    /// Closes a session this dispatch created but could not deliver to.
    async fn abandon(&self, created: Option<&SessionId>) {
        if let Some(id) = created
            && let Err(e) = self.orch.close_session(id).await
        {
            tracing::warn!(session = %id, error = %e, "closing undelivered dispatch session");
        }
    }

    /// Announces the dispatch on both sessions' streams and writes the audit entry.
    /// Best effort: the prompt has already been delivered.
    async fn record(&self, d: &Dispatch, from_agent: bool) {
        for (session, payload) in [
            (
                &d.from_session,
                EventPayload::DispatchSent {
                    dispatch: d.clone(),
                },
            ),
            (
                &d.to_session,
                EventPayload::DispatchReceived {
                    dispatch: d.clone(),
                },
            ),
        ] {
            if let Err(e) = self.orch.announce(session, payload).await {
                tracing::warn!(dispatch = %d.id, %session, error = %e, "dispatch event not recorded");
            }
        }
        let detail = serde_json::json!({
            "from_session": d.from_session,
            "to_session": d.to_session,
            "depth": d.depth,
            "created_session": d.created_session,
            "from_agent": from_agent,
        });
        if let Err(e) = self
            .db
            .audit(
                Some(d.member_id.clone()),
                "session.dispatch",
                d.id.as_str(),
                detail,
            )
            .await
        {
            tracing::warn!(dispatch = %d.id, error = %e, "dispatch audit entry not written");
        }
    }
}
