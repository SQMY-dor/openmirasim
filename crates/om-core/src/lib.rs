//! Shared domain models, wire contracts and runtime traits for OpenMirasim.
//!
//! `om-core` is the single source of truth for every type that crosses a process
//! or network boundary (R2). Types that the Web workbench consumes derive [`ts_rs::TS`]
//! and are exported to `apps/web/src/generated/` by `cargo test -p om-core`; CI fails
//! when the committed bindings drift from the Rust definitions.
//!
//! Module map:
//! - [`ids`]: strongly typed identifiers.
//! - [`content`]: prompt / message content blocks.
//! - [`session`]: session status state machine and configuration.
//! - [`approval`]: approval options, decisions and records.
//! - [`event`]: the persisted, seq-ordered session event stream.
//! - [`api`]: REST request/response DTOs and the `/ws/events` protocol.
//! - [`harness`]: the `Harness` trait every agent adapter implements.
//! - [`sandbox`]: the `Sandbox` trait that wraps agent processes.
//! - [`queue`]: Queue & Steer submit modes (W2-3).
//! - [`schedule`]: schedules that send prompts on a timetable (W4-1).
//! - [`goal`]: goals that keep a session working until they are met (W4-1).
//! - [`dispatch`]: one session handing a prompt to another (W4-1).
//! - [`error`]: shared error type.

pub mod api;
pub mod approval;
pub mod auth;
pub mod content;
pub mod dispatch;
pub mod error;
pub mod event;
pub mod goal;
pub mod harness;
pub mod ids;
pub mod proxy;
pub mod queue;
pub mod sandbox;
pub mod schedule;
pub mod session;
pub mod workspace;

pub use error::{CoreError, CoreResult};

/// Version of the wire protocol spoken over REST and `/ws/events`.
///
/// Bump on any breaking change to [`event`] or [`api`] types.
pub const PROTOCOL_VERSION: u32 = 1;

/// Returns the crate version string for bootstrap diagnostics.
pub const fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Current wall-clock time as Unix milliseconds, the timestamp unit used on the wire.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_version_is_non_empty() {
        assert!(!crate_version().is_empty());
    }

    #[test]
    fn now_ms_is_after_2024() {
        assert!(now_ms() > 1_704_067_200_000);
    }
}
