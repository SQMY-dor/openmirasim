//! Queue & Steer contract (W2-3).
//!
//! While a turn runs, members can still send prompts:
//!
//! - [`SubmitMode::Queue`] appends to the session's queue; queued prompts start one by
//!   one as turns end.
//! - [`SubmitMode::Next`] jumps the queue (inserted at the front).
//! - [`SubmitMode::Steer`] redirects the running turn now. If the harness supports
//!   native steering (`HarnessCapabilities::steer`) the message is injected into the
//!   turn; otherwise the orchestrator degrades to "cancel and resend": it cancels the
//!   turn and runs the prompt first once the turn has ended.
//!
//! On an idle session every mode simply starts a turn. The queue lives in the session's
//! actor and is mirrored in the event stream (`prompt_queued`, `queue_item_removed`,
//! `queue_reordered`, `queue_paused`), so clients can render it by replaying events.
//! Cancelling a turn pauses the queue (so "stop" does not immediately start the next
//! prompt); submitting again or resuming un-pauses it.

use crate::content::Prompt;
use crate::ids::{MemberId, QueuedPromptId, TurnId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Most prompts a session's queue holds.
pub const MAX_QUEUED_PROMPTS: usize = 32;

/// How a prompt sent to a session is handled when a turn is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SubmitMode {
    /// Run after everything already queued.
    #[default]
    Queue,
    /// Run right after the current turn, ahead of the queue.
    Next,
    /// Redirect the current turn now.
    Steer,
}

/// A prompt waiting in a session's queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct QueuedPrompt {
    pub id: QueuedPromptId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub member_id: Option<MemberId>,
    pub prompt: Prompt,
    pub mode: SubmitMode,
    #[ts(type = "number")]
    pub created_at: i64,
}

/// What happened to a submitted prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[ts(export)]
pub enum SubmitOutcome {
    /// The session was idle: a turn started.
    Started { turn_id: TurnId },
    /// Waiting in the queue at `position` (0 = next).
    Queued {
        queued_id: QueuedPromptId,
        position: u32,
    },
    /// Injected into the running turn (native steer).
    Steered { turn_id: TurnId },
    /// Steer degraded: the running turn is being cancelled and this prompt runs first
    /// once it has ended.
    Interrupting { queued_id: QueuedPromptId },
}

/// Why a prompt left the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[ts(export)]
pub enum QueueRemoval {
    /// It started a turn.
    Started { turn_id: TurnId },
    /// A member withdrew it.
    Withdrawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        by: Option<MemberId>,
    },
    /// The harness refused it when it was its turn.
    Failed { error: String },
    /// The session closed or its agent stopped before it ran.
    Dropped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        assert_eq!(
            serde_json::to_string(&SubmitMode::default()).unwrap(),
            "\"queue\""
        );
        let o = SubmitOutcome::Queued {
            queued_id: QueuedPromptId::from("q1"),
            position: 2,
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["outcome"], "queued");
        assert_eq!(v["position"], 2);
        let r = serde_json::to_value(QueueRemoval::Withdrawn { by: None }).unwrap();
        assert_eq!(r, serde_json::json!({"reason": "withdrawn"}));
    }
}
