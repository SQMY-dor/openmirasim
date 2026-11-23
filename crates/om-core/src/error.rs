//! Shared error type.

use crate::session::SessionStatus;
use thiserror::Error;

/// Result alias for fallible `om-core` operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Errors shared across server, harnesses and clients.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid session transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: SessionStatus,
        to: SessionStatus,
    },
    #[error("approval already resolved: {0}")]
    ApprovalAlreadyResolved(String),
    #[error("capability not supported by harness `{harness}`: {capability}")]
    Unsupported {
        harness: String,
        capability: &'static str,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("harness error: {0}")]
    Harness(String),
    #[error("sandbox error: {0}")]
    Sandbox(String),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
