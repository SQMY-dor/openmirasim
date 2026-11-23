//! Session-to-Session dispatch (W4-1). See [`om_core::dispatch`] for the contract.

use super::wait_ready;
use crate::db::Db;
use crate::orchestrator::Orchestrator;
use om_core::auth::Action;
use om_core::dispatch::{Dispatch, DispatchTarget, MAX_DISPATCH_DEPTH, NewDispatch};
use om_core::event::EventPayload;
use om_core::ids::{DispatchId, MemberId, SessionId};
use om_core::queue::SubmitMode;
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
        let member_id = match by {
            Some(m) => m,
            None => self.db.last_turn_member(&from.id).await?.ok_or_else(|| {
                CoreError::Forbidden(
                    "no member started the source session's turn, so it cannot dispatch".into(),
                )
            })?,
        };
        let m = self
            .db
            .get_member(&member_id)
            .await
            .map_err(|_| CoreError::Forbidden("unknown member".into()))?;
        if !m.is_active() || m.space_id != from.space_id {
            return Err(CoreError::Forbidden(
                "member is not active in this space".into(),
            ));
        }
        if !m.role.can(Action::PromptSession) {
            return Err(CoreError::Forbidden(
                "role does not allow prompting sessions".into(),
            ));
        }
        // A member dispatching by hand starts a new chain; an agent extends its own.
        let depth = if by_member {
            1
        } else {
            self.db.dispatch_depth(&from.id).await? + 1
        };
        if depth > MAX_DISPATCH_DEPTH {
            return Err(CoreError::LimitExceeded(format!(
                "dispatch chains are limited to {MAX_DISPATCH_DEPTH} hops"
            )));
        }

        let (to_session, created_session, mode) = match req.target {
            DispatchTarget::Session { session_id, mode } => {
                let to = self.db.get_session(&session_id).await?;
                if to.space_id != from.space_id {
                    return Err(CoreError::NotFound(format!("session {session_id}")));
                }
                if matches!(to.status, SessionStatus::Failed | SessionStatus::Closed) {
                    return Err(CoreError::InvalidInput(
                        "the target session has ended".into(),
                    ));
                }
                (session_id, false, mode)
            }
            DispatchTarget::NewSession { config } => {
                if !m.role.can(Action::CreateSession) {
                    return Err(CoreError::Forbidden(
                        "role does not allow creating sessions".into(),
                    ));
                }
                let created = self
                    .orch
                    .create_session(from.space_id.clone(), config, Some(member_id.clone()))
                    .await?;
                wait_ready(&self.orch, &created.id, self.ready_timeout).await?;
                (created.id, true, SubmitMode::Queue)
            }
        };
        let submit = self
            .orch
            .send_prompt(&to_session, Some(member_id.clone()), req.prompt, mode)
            .await?;
        let d = self
            .db
            .insert_dispatch(Dispatch {
                id: DispatchId::new(),
                space_id: from.space_id,
                from_session: from.id,
                to_session,
                member_id,
                depth,
                created_session,
                submit,
                created_at: now_ms(),
            })
            .await?;
        self.record(&d, !by_member).await;
        Ok(d)
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
