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

use crate::approval::{ApprovalDecision, ApprovalRecord};
use crate::content::Prompt;
use crate::error::CoreError;
use crate::event::SessionEvent;
use crate::ids::{SessionId, TurnId};
use crate::session::{SessionConfig, SessionSummary};
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
    #[ts(type = "number")]
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
            CoreError::InvalidTransition { .. } | CoreError::ApprovalAlreadyResolved(_) => {
                ApiErrorCode::Conflict
            }
            CoreError::Unsupported { .. } => ApiErrorCode::Unsupported,
            CoreError::InvalidInput(_) => ApiErrorCode::InvalidInput,
            CoreError::Harness(_)
            | CoreError::Sandbox(_)
            | CoreError::Storage(_)
            | CoreError::Io(_) => ApiErrorCode::Internal,
        };
        Self {
            code,
            message: err.to_string(),
        }
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
