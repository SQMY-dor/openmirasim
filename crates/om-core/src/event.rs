//! The persisted, seq-ordered session event stream.
//!
//! Every observable change to a session is appended as a [`SessionEvent`] with a
//! per-session, strictly increasing `seq` starting at 1. The server persists an event
//! **before** broadcasting it, so any `seq` a client has seen can be replayed after a
//! reconnect or a server restart. Clients resume by sending their `last_seq`.

use crate::approval::{ApprovalDecision, ApprovalRecord};
use crate::content::{ContentBlock, Prompt};
use crate::harness::HarnessCapabilities;
use crate::ids::{ApprovalId, MemberId, QueuedPromptId, SessionId, TurnId};
use crate::queue::{QueueRemoval, QueuedPrompt};
use crate::session::{SessionConfig, SessionStatus, StopReason};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One persisted event in a session's stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionEvent {
    pub session_id: SessionId,
    /// Per-session sequence number, strictly increasing from 1 with no gaps.
    #[ts(type = "number")]
    pub seq: u64,
    /// Unix milliseconds when the server recorded the event.
    #[ts(type = "number")]
    pub ts: i64,
    pub payload: EventPayload,
}

/// What happened. Tagged by `type` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum EventPayload {
    /// First event of every session.
    SessionCreated {
        config: SessionConfig,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        created_by: Option<MemberId>,
    },
    /// The harness finished initializing.
    HarnessReady {
        capabilities: HarnessCapabilities,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        native_session_id: Option<String>,
    },
    StatusChanged {
        from: SessionStatus,
        to: SessionStatus,
    },
    /// A member's prompt started a turn.
    PromptSubmitted {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        member_id: Option<MemberId>,
        prompt: Prompt,
    },
    /// A streamed fragment of the agent's reply.
    AgentMessageChunk {
        turn_id: TurnId,
        content: ContentBlock,
    },
    /// A streamed fragment of the agent's reasoning.
    AgentThoughtChunk {
        turn_id: TurnId,
        content: ContentBlock,
    },
    ToolCall {
        turn_id: TurnId,
        tool_call: ToolCallInfo,
    },
    ToolCallUpdate {
        turn_id: TurnId,
        update: ToolCallUpdate,
    },
    /// The agent's current plan (replaces any earlier plan for the turn).
    Plan {
        turn_id: TurnId,
        entries: Vec<PlanEntry>,
    },
    ApprovalRequested {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        turn_id: Option<TurnId>,
        approval: ApprovalRecord,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        resolved_by: Option<MemberId>,
        /// Policy rule that resolved the request automatically, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        auto_rule: Option<String>,
    },
    TurnEnded {
        turn_id: TurnId,
        stop_reason: StopReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<String>,
    },
    /// A session-level error not tied to a turn (e.g. the harness process exited).
    Error { message: String },
    /// A prompt was added to the session's queue at `position` (0 = next).
    PromptQueued { queued: QueuedPrompt, position: u32 },
    /// A prompt left the queue.
    QueueItemRemoved {
        queued_id: QueuedPromptId,
        reason: QueueRemoval,
    },
    /// The queue's order changed; `order` is the whole queue, front first.
    QueueReordered { order: Vec<QueuedPromptId> },
    /// Queue draining stopped (after a cancel) or resumed.
    QueuePaused { paused: bool },
    /// A member's message was injected into the running turn (native steer).
    PromptSteered {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        member_id: Option<MemberId>,
        prompt: Prompt,
    },
}

impl EventPayload {
    /// The `type` tag of this payload, as stored in the `events.type` column.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::HarnessReady { .. } => "harness_ready",
            Self::StatusChanged { .. } => "status_changed",
            Self::PromptSubmitted { .. } => "prompt_submitted",
            Self::AgentMessageChunk { .. } => "agent_message_chunk",
            Self::AgentThoughtChunk { .. } => "agent_thought_chunk",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolCallUpdate { .. } => "tool_call_update",
            Self::Plan { .. } => "plan",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalResolved { .. } => "approval_resolved",
            Self::TurnEnded { .. } => "turn_ended",
            Self::Error { .. } => "error",
            Self::PromptQueued { .. } => "prompt_queued",
            Self::QueueItemRemoved { .. } => "queue_item_removed",
            Self::QueueReordered { .. } => "queue_reordered",
            Self::QueuePaused { .. } => "queue_paused",
            Self::PromptSteered { .. } => "prompt_steered",
        }
    }
}

/// Category of a tool call. Mirrors ACP `ToolKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    #[serde(other)]
    Other,
}

/// Execution status of a tool call. Mirrors ACP `ToolCallStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolCallStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// A tool call as first reported by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ToolCallInfo {
    pub tool_call_id: String,
    pub title: String,
    #[serde(default)]
    pub kind: ToolKind,
    #[serde(default)]
    pub status: ToolCallStatus,
    /// Raw tool input as sent by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub raw_input: Option<serde_json::Value>,
    /// File paths the call touches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<String>,
}

/// A partial update to an existing tool call; absent fields are unchanged.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub status: Option<ToolCallStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
    /// Output produced by the tool so far.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub raw_output: Option<serde_json::Value>,
}

/// Status of a plan entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PlanEntryStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

/// One step of the agent's plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PlanEntry {
    pub content: String,
    #[serde(default)]
    pub status: PlanEntryStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tag_matches_kind() {
        let p = EventPayload::TurnEnded {
            turn_id: TurnId::from("t1"),
            stop_reason: StopReason::EndTurn,
            error: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["type"], p.kind());
        assert_eq!(v["stop_reason"], "end_turn");
    }

    #[test]
    fn event_roundtrip() {
        let e = SessionEvent {
            session_id: SessionId::from("s1"),
            seq: 7,
            ts: 1,
            payload: EventPayload::AgentMessageChunk {
                turn_id: TurnId::from("t1"),
                content: ContentBlock::Text { text: "hi".into() },
            },
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: SessionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn unknown_tool_kind_falls_back_to_other() {
        let k: ToolKind = serde_json::from_str("\"teleport\"").unwrap();
        assert_eq!(k, ToolKind::Other);
    }
}
