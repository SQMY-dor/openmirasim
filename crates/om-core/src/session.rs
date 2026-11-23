//! Session status state machine and configuration.
//!
//! The architecture doc lists `Idle -> Running -> WaitingApproval -> Settled / Failed /
//! Cancelled`. Sessions are multi-turn, so this crate splits that into two concepts:
//!
//! - [`SessionStatus`]: what the session as a whole is doing right now;
//! - [`StopReason`]: how a single turn ended (`EndTurn` is "settled", `Cancelled` is
//!   "cancelled"). After a turn ends the session returns to [`SessionStatus::Idle`].

use crate::error::{CoreError, CoreResult};
use crate::ids::{SessionId, SpaceId, WorkspaceId};
use crate::sandbox::SandboxMode;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Lifecycle status of a session. Owned and mutated only by the session's actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SessionStatus {
    /// The harness process is being spawned and initialized.
    Starting,
    /// Ready to accept a prompt.
    Idle,
    /// A turn is in progress.
    Running,
    /// A turn is paused until a member resolves an approval request.
    WaitingApproval,
    /// The harness crashed or failed to start; may be resumed.
    Failed,
    /// Closed by a member or the server; terminal.
    Closed,
}

impl SessionStatus {
    /// Returns whether moving from `self` to `to` is a legal transition.
    pub fn can_transition_to(self, to: SessionStatus) -> bool {
        use SessionStatus::*;
        if self == Closed {
            return false;
        }
        if to == Closed {
            return true;
        }
        matches!(
            (self, to),
            (Starting, Idle)
                | (Starting, Failed)
                | (Idle, Running)
                | (Running, WaitingApproval)
                | (Running, Idle)
                | (Running, Failed)
                | (WaitingApproval, Running)
                | (WaitingApproval, Idle)
                | (WaitingApproval, Failed)
                | (Failed, Starting)
                | (Idle, Starting)
        )
    }

    /// Validates a transition, returning [`CoreError::InvalidTransition`] when illegal.
    pub fn transition(self, to: SessionStatus) -> CoreResult<SessionStatus> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(CoreError::InvalidTransition { from: self, to })
        }
    }

    /// Whether a new prompt may start a turn immediately.
    pub fn accepts_prompt(self) -> bool {
        self == SessionStatus::Idle
    }

    /// Whether the session is in a terminal state.
    pub fn is_terminal(self) -> bool {
        self == SessionStatus::Closed
    }
}

/// Why a turn ended. Mirrors ACP `StopReason`, plus `Failed` for harness errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum StopReason {
    /// The agent finished the turn normally ("settled").
    EndTurn,
    /// Token limit reached.
    MaxTokens,
    /// Per-turn request limit reached.
    MaxTurnRequests,
    /// The agent refused to continue.
    Refusal,
    /// A member cancelled the turn.
    Cancelled,
    /// The harness reported an error or exited mid-turn.
    Failed,
}

/// How a session is configured at creation time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionConfig {
    /// Registry id of the harness, e.g. `"opencode"`, `"mock"`.
    pub harness_id: String,
    /// Absolute working directory on the server (workspace root or worktree).
    pub cwd: String,
    /// Workspace the session belongs to, when registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_id: Option<WorkspaceId>,
    /// Model override passed to the harness, when it supports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model: Option<String>,
    /// Execution isolation mode.
    #[serde(default)]
    pub sandbox: SandboxMode,
    /// Human-readable title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
}

/// A session as listed by the REST API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionSummary {
    pub id: SessionId,
    pub space_id: SpaceId,
    pub config: SessionConfig,
    pub status: SessionStatus,
    /// Highest persisted event seq (0 when the session has no events).
    #[ts(type = "number")]
    pub head_seq: u64,
    /// The harness' own session id, used for `resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub native_session_id: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::SessionStatus::*;

    #[test]
    fn happy_path_turn() {
        let s = Starting;
        let s = s.transition(Idle).unwrap();
        let s = s.transition(Running).unwrap();
        let s = s.transition(WaitingApproval).unwrap();
        let s = s.transition(Running).unwrap();
        let s = s.transition(Idle).unwrap();
        assert!(s.accepts_prompt());
    }

    #[test]
    fn closed_is_terminal() {
        assert!(Closed.is_terminal());
        for to in [Starting, Idle, Running, WaitingApproval, Failed, Closed] {
            assert!(!Closed.can_transition_to(to));
        }
    }

    #[test]
    fn any_live_state_can_close() {
        for from in [Starting, Idle, Running, WaitingApproval, Failed] {
            assert!(from.can_transition_to(Closed));
        }
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert!(Idle.transition(WaitingApproval).is_err());
        assert!(Starting.transition(Running).is_err());
        assert!(Failed.transition(Running).is_err());
    }

    #[test]
    fn failed_session_can_be_resumed() {
        assert!(Failed.transition(Starting).is_ok());
    }
}
