//! Approval options, decisions and records.
//!
//! Option kinds mirror ACP `PermissionOptionKind` so harnesses can map a member's
//! decision back to the agent's own option id.

use crate::ids::{ApprovalId, MemberId, SessionId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The semantic kind of an approval option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl ApprovalOptionKind {
    /// Whether this option lets the tool call proceed.
    pub fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

/// One choice offered to members for an approval request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApprovalOption {
    /// Harness-native option id (ACP `optionId`), echoed back on resolution.
    pub option_id: String,
    /// Label shown to members.
    pub name: String,
    pub kind: ApprovalOptionKind,
}

/// A member's (or the server's) answer to an approval request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[ts(export)]
pub enum ApprovalDecision {
    /// One of the offered options was chosen.
    Selected { option_id: String },
    /// The request was withdrawn (turn cancelled, session closed or timed out).
    Cancelled,
}

/// Persisted state of an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ApprovalStatus {
    Pending,
    Resolved,
    Cancelled,
}

/// An approval request as stored and listed by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApprovalRecord {
    pub id: ApprovalId,
    pub session_id: SessionId,
    /// The tool call this approval gates, when the harness reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tool_call_id: Option<String>,
    pub title: String,
    pub options: Vec<ApprovalOption>,
    pub status: ApprovalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub decision: Option<ApprovalDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub resolved_by: Option<MemberId>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub resolved_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_wire_shape() {
        let d = ApprovalDecision::Selected {
            option_id: "allow".into(),
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            serde_json::json!({"outcome": "selected", "option_id": "allow"})
        );
        assert_eq!(
            serde_json::to_value(ApprovalDecision::Cancelled).unwrap(),
            serde_json::json!({"outcome": "cancelled"})
        );
    }

    #[test]
    fn allow_kinds() {
        assert!(ApprovalOptionKind::AllowOnce.is_allow());
        assert!(!ApprovalOptionKind::RejectAlways.is_allow());
    }
}
