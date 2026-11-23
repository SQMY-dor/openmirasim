//! Session-to-Session dispatch contract (W4-1).
//!
//! A dispatch hands a prompt from one session to another: an existing session in the
//! same space (with Queue & Steer semantics) or a fresh one. It is how a coordinating
//! agent farms work out, and how a member forwards a result to another session.
//!
//! Permission routing: a dispatch acts as one member, never as "the agent".
//!
//! - A member dispatching by hand acts as themself.
//! - An agent dispatching from inside a session (through a tool the harness exposes)
//!   acts as the member whose prompt started the source session's current turn, and only
//!   while that turn runs. Every member who steered into the turn must be allowed too,
//!   and a turn with an instruction no member sent cannot dispatch.
//!
//! That member must be active in the space and allowed to prompt the target (and to
//! create sessions, for a new one). Chains are bounded: an agent's dispatch reaches depth
//! (depth of the turn it runs in) + 1, where a turn's depth is that of the dispatch that
//! delivered it (0 for a member's own turn), and depth may not pass
//! [`MAX_DISPATCH_DEPTH`]; one turn may dispatch at most [`MAX_DISPATCH_FANOUT`] times.
//! So agents cannot fan out wide or deep, or ping-pong without limit. A member
//! dispatching by hand starts a new chain at depth 1. Every dispatch leaves one
//! [`Dispatch`] row.

use crate::content::Prompt;
use crate::ids::{DispatchId, MemberId, SessionId, SpaceId};
use crate::queue::{SubmitMode, SubmitOutcome};
use crate::session::SessionConfig;
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Longest dispatch chain: a member's session (depth 0) may reach depth 3.
pub const MAX_DISPATCH_DEPTH: u32 = 3;

/// Most dispatches an agent may send during one turn of its session.
pub const MAX_DISPATCH_FANOUT: u32 = 8;

/// Where a dispatch goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum DispatchTarget {
    /// An existing session in the same space.
    Session {
        session_id: SessionId,
        #[serde(default)]
        #[ts(as = "Option<SubmitMode>", optional)]
        mode: SubmitMode,
    },
    /// A new session, given the prompt once its agent is ready.
    NewSession { config: SessionConfig },
}

/// Body for a dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewDispatch {
    pub from_session: SessionId,
    pub target: DispatchTarget,
    pub prompt: Prompt,
}

impl NewDispatch {
    pub fn validate(&self) -> CoreResult<()> {
        if self.prompt.is_empty() {
            return Err(CoreError::InvalidInput("dispatch prompt is empty".into()));
        }
        if let DispatchTarget::Session { session_id, .. } = &self.target
            && *session_id == self.from_session
        {
            return Err(CoreError::InvalidInput(
                "a session cannot dispatch to itself".into(),
            ));
        }
        Ok(())
    }
}

/// A dispatch that was delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Dispatch {
    pub id: DispatchId,
    pub space_id: SpaceId,
    pub from_session: SessionId,
    pub to_session: SessionId,
    /// The member the dispatch acted as.
    pub member_id: MemberId,
    /// Depth of `to_session` in the chain (1 for a member's own dispatch).
    pub depth: u32,
    /// Whether the target session was created by this dispatch.
    pub created_session: bool,
    pub submit: SubmitOutcome,
    #[ts(type = "number")]
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_and_wire() {
        let d = NewDispatch {
            from_session: SessionId::from("a"),
            target: DispatchTarget::Session {
                session_id: SessionId::from("a"),
                mode: SubmitMode::Queue,
            },
            prompt: Prompt::text("hi"),
        };
        assert!(d.validate().is_err());
        let d = NewDispatch {
            prompt: Prompt::text(""),
            target: DispatchTarget::Session {
                session_id: SessionId::from("b"),
                mode: SubmitMode::Queue,
            },
            ..d
        };
        assert!(d.validate().is_err());
        let t: DispatchTarget =
            serde_json::from_str(r#"{"kind":"session","session_id":"b"}"#).unwrap();
        assert_eq!(
            t,
            DispatchTarget::Session {
                session_id: SessionId::from("b"),
                mode: SubmitMode::default()
            }
        );
    }
}
