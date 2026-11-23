//! REST request/response DTOs and the `/ws/events` protocol.
//!
//! Transport rules (Phase 0–2):
//! - REST under `/api/v1`, JSON bodies, `Authorization: Bearer <token>`.
//! - `/ws/events` carries JSON text frames of [`ClientMessage`] / [`ServerMessage`].
//!   Browsers cannot set headers on WebSocket upgrades, so the token may also be sent
//!   as the `token` query parameter.
//! - `/ws/pty/:session_id` (Phase 3) carries binary PTY frames and is out of scope here.
//!
//! Resume protocol: after `Welcome`, the client sends `Subscribe { last_seq }` per
//! session. The server replies `Subscribed { head_seq }`, then sends every event with
//! `seq > last_seq` in order, then live events. Events are never skipped; if the server
//! cannot keep up with a slow client it sends `Resync` and the client re-subscribes from
//! its last applied seq.

use crate::approval::ApprovalOptionKind;
use crate::approval::{ApprovalDecision, ApprovalRecord};
use crate::auth::{DeviceTokenRecord, MemberRecord, Role};
use crate::content::Prompt;
use crate::dispatch::Dispatch;
use crate::error::CoreError;
use crate::event::SessionEvent;
use crate::goal::Goal;
use crate::ids::{MemberId, SessionId, TurnId};
use crate::proxy::ProviderInfo;
use crate::queue::{QueuedPrompt, SubmitMode, SubmitOutcome};
use crate::schedule::{Schedule, ScheduleRun};
use crate::session::{SessionConfig, SessionSummary};
use crate::workspace::{WorkspaceSummary, WorktreeEntry};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `POST /api/v1/sessions`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateSessionRequest {
    pub config: SessionConfig,
}

/// `GET /api/v1/sessions`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionSummary>,
}

/// Query of `GET /api/v1/sessions/:id/events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListEventsQuery {
    /// Return events with `seq > after_seq`.
    #[serde(default)]
    #[ts(as = "Option<u32>", optional)]
    pub after_seq: u64,
    /// Page size; the server clamps it to [`MAX_EVENTS_PAGE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// Largest page `GET /events` returns.
pub const MAX_EVENTS_PAGE: u32 = 1000;

/// Response of `GET /api/v1/sessions/:id/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListEventsResponse {
    pub events: Vec<SessionEvent>,
    #[ts(type = "number")]
    pub head_seq: u64,
    pub has_more: bool,
}

/// `POST /api/v1/sessions/:id/prompts`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SubmitPromptRequest {
    pub prompt: Prompt,
    /// Queue & Steer mode (W2-3). Without it the session must be idle and the response
    /// is [`SubmitPromptResponse`]; with it the response is [`SendPromptResponse`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mode: Option<SubmitMode>,
}

/// Response of `POST /api/v1/sessions/:id/prompts` when a `mode` was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SendPromptResponse {
    pub outcome: SubmitOutcome,
}

/// Response of `POST /api/v1/sessions/:id/prompts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SubmitPromptResponse {
    pub turn_id: TurnId,
}

/// `GET /api/v1/sessions/:id/approvals`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListApprovalsResponse {
    pub approvals: Vec<ApprovalRecord>,
}

/// `POST /api/v1/approvals/:id/resolve`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ResolveApprovalRequest {
    pub decision: ApprovalDecision,
}

// ----------------------------------------------------------- W1-W4 REST DTOs
//
// Wire shapes for the endpoints of the W1-W4 server modules (review P1-15), so the
// REST layer, the web app and om-cli share one definition.

/// `GET /api/v1/sessions/:id/queue`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListQueueResponse {
    pub items: Vec<QueuedPrompt>,
}

/// `POST /api/v1/sessions/:id/queue/:queued_id/move`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MoveQueuedRequest {
    /// 0 = next; clamped to the end of the queue.
    pub position: u32,
}

/// One audit log entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AuditEntry {
    #[ts(type = "number")]
    pub id: i64,
    #[ts(type = "number")]
    pub ts: i64,
    /// Member that acted; absent for the server itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub actor: Option<MemberId>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub target: Option<String>,
    #[ts(type = "unknown")]
    pub detail: serde_json::Value,
}

/// `GET /api/v1/audit?target=&limit=`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListAuditResponse {
    pub entries: Vec<AuditEntry>,
}

/// A remembered "always" answer of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApprovalRule {
    pub rule_key: String,
    pub allow: bool,
    pub option_kind: ApprovalOptionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub created_by: Option<MemberId>,
    #[ts(type = "number")]
    pub created_at: i64,
}

/// `GET /api/v1/sessions/:id/approval-rules`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListApprovalRulesResponse {
    pub rules: Vec<ApprovalRule>,
}

/// `DELETE /api/v1/sessions/:id/approval-rules`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ClearApprovalRulesResponse {
    pub cleared: u32,
}

/// Key-proxy counters of one session's current token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProxyStats {
    #[ts(type = "number")]
    pub requests: u64,
    #[ts(type = "number")]
    pub upstream_errors: u64,
}

/// `GET /api/v1/providers`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListProvidersResponse {
    pub providers: Vec<ProviderInfo>,
}

/// `POST /api/v1/workspaces`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RegisterWorkspaceRequest {
    pub name: String,
    /// Absolute path of the repository's top level on the server.
    pub repo_path: String,
}

/// `GET /api/v1/workspaces`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListWorkspacesResponse {
    pub workspaces: Vec<WorkspaceSummary>,
}

/// `GET /api/v1/sessions/:id/worktree/files`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListWorktreeFilesResponse {
    pub entries: Vec<WorktreeEntry>,
}

/// `GET /api/v1/members`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListMembersResponse {
    pub members: Vec<MemberRecord>,
}

/// `POST /api/v1/members`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AddMemberRequest {
    pub display_name: String,
    pub role: Role,
}

/// `PATCH /api/v1/members/:id`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChangeRoleRequest {
    pub role: Role,
}

/// `POST /api/v1/members/:id/tokens`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IssueTokenRequest {
    pub label: String,
    /// Lifetime in seconds; absent = until revoked (never longer than the caller's own
    /// token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub ttl_secs: Option<u64>,
}

/// `GET /api/v1/members/:id/tokens`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListTokensResponse {
    pub tokens: Vec<DeviceTokenRecord>,
}

/// `GET /api/v1/schedules`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListSchedulesResponse {
    pub schedules: Vec<Schedule>,
}

/// `GET /api/v1/schedules/:id/runs`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListScheduleRunsResponse {
    pub runs: Vec<ScheduleRun>,
}

/// `POST /api/v1/schedules/:id/enabled`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SetScheduleEnabledRequest {
    pub enabled: bool,
}

/// `GET /api/v1/sessions/:id/goals`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListGoalsResponse {
    pub goals: Vec<Goal>,
}

/// `GET /api/v1/sessions/:id/dispatches`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ListDispatchesResponse {
    pub dispatches: Vec<Dispatch>,
}

/// Machine-readable error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ApiErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    /// State conflict: illegal transition, approval already resolved, session busy.
    Conflict,
    InvalidInput,
    Unsupported,
    /// A quota or rate limit was hit (queue length, fan-out, token count); retry later.
    LimitExceeded,
    Internal,
}

impl ApiErrorCode {
    /// Suggested HTTP status code.
    pub fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Conflict => 409,
            Self::InvalidInput => 400,
            Self::Unsupported => 422,
            Self::LimitExceeded => 429,
            Self::Internal => 500,
        }
    }
}

/// Error body of every non-2xx REST response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
}

impl From<&CoreError> for ApiError {
    fn from(err: &CoreError) -> Self {
        let code = match err {
            CoreError::NotFound(_) => ApiErrorCode::NotFound,
            CoreError::Unauthenticated(_) => ApiErrorCode::Unauthorized,
            CoreError::Forbidden(_) => ApiErrorCode::Forbidden,
            CoreError::InvalidTransition { .. } | CoreError::ApprovalAlreadyResolved(_) => {
                ApiErrorCode::Conflict
            }
            CoreError::LimitExceeded(_) => ApiErrorCode::LimitExceeded,
            CoreError::Unsupported { .. } => ApiErrorCode::Unsupported,
            CoreError::InvalidInput(_) => ApiErrorCode::InvalidInput,
            CoreError::Harness(_)
            | CoreError::Sandbox(_)
            | CoreError::Workspace(_)
            | CoreError::Storage(_)
            | CoreError::Io(_) => ApiErrorCode::Internal,
        };
        // Internal errors can carry paths, SQL or harness output: log them on the server
        // and give clients a generic message (review P2-8).
        let message = if code == ApiErrorCode::Internal {
            "internal server error".to_owned()
        } else {
            err.to_string()
        };
        Self { code, message }
    }
}

/// Client -> server frame on `/ws/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
    },
    Subscribe {
        session_id: SessionId,
        /// Last seq the client applied; 0 for a fresh subscription.
        #[ts(type = "number")]
        last_seq: u64,
    },
    Unsubscribe {
        session_id: SessionId,
    },
    Ping {
        #[ts(type = "number")]
        nonce: u64,
    },
}

/// Server -> client frame on `/ws/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ServerMessage {
    Welcome {
        protocol_version: u32,
        server_version: String,
    },
    Subscribed {
        session_id: SessionId,
        #[ts(type = "number")]
        head_seq: u64,
    },
    Event {
        event: Box<SessionEvent>,
    },
    /// The subscription fell too far behind; re-subscribe from the last applied seq.
    Resync {
        session_id: SessionId,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<SessionId>,
        error: ApiError,
    },
    Pong {
        #[ts(type = "number")]
        nonce: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_subscribe_wire_shape() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"type":"subscribe","session_id":"s1","last_seq":42}"#)
                .unwrap();
        assert_eq!(
            m,
            ClientMessage::Subscribe {
                session_id: SessionId::from("s1"),
                last_seq: 42
            }
        );
    }

    #[test]
    fn core_error_maps_to_conflict() {
        let e = CoreError::ApprovalAlreadyResolved("a1".into());
        let api = ApiError::from(&e);
        assert_eq!(api.code, ApiErrorCode::Conflict);
        assert_eq!(api.code.http_status(), 409);
    }
}
