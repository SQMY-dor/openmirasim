//! The `Harness` abstraction every agent adapter implements.
//!
//! A [`Harness`] is a factory registered under a stable id (`"opencode"`, `"mock"`, ...).
//! [`Harness::start`] spawns or attaches one agent session and returns a
//! [`HarnessSession`] for control calls plus a stream of [`HarnessEvent`]s.
//!
//! Harness events are *not* persisted directly: the session orchestrator assigns turn
//! ids and `seq` numbers, records approvals and converts them into
//! [`crate::event::EventPayload`]s. Harnesses therefore never need to know about storage,
//! members or broadcasting.
//!
//! Contract for implementors:
//! - [`HarnessSession::prompt`] returns once the prompt is accepted; the turn's outcome
//!   is reported later as exactly one [`HarnessEvent::TurnEnded`].
//! - Every [`HarnessEvent::PermissionRequested`] must eventually be answered through
//!   [`HarnessSession::respond_approval`] (the orchestrator answers `Cancelled` when a
//!   turn is cancelled or the session closes).
//! - When the agent process exits, emit [`HarnessEvent::Exited`] and end the stream.
//! - Capabilities a harness lacks return [`CoreError::Unsupported`]; the orchestrator
//!   consults [`HarnessCapabilities`] first and degrades (e.g. steer -> queue).

use crate::approval::{ApprovalDecision, ApprovalOption};
use crate::content::{ContentBlock, Prompt};
use crate::error::{CoreError, CoreResult};
use crate::event::{PlanEntry, ToolCallInfo, ToolCallUpdate, ToolKind};
use crate::ids::SessionId;
use crate::sandbox::Sandbox;
use crate::session::{SessionConfig, StopReason};
use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use ts_rs::TS;

/// What a harness can do. Reported once in `HarnessReady` and used for degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct HarnessCapabilities {
    /// The agent asks for permission before risky tool calls.
    pub native_approval: bool,
    /// Messages can be injected into a running turn.
    pub steer: bool,
    /// A previous native session can be resumed (`resume_native_session_id`).
    pub resume: bool,
    /// Tool calls are reported as structured events.
    pub structured_tools: bool,
    /// A raw PTY can be attached for a native terminal view (Phase 3).
    pub pty_attach: bool,
    /// A remote screen (e.g. noVNC) is available for GUI agents.
    pub remote_screen: bool,
}

/// An event emitted by a running harness session.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessEvent {
    /// The agent's native session id became known (or changed).
    NativeSessionId(String),
    MessageChunk(ContentBlock),
    ThoughtChunk(ContentBlock),
    ToolCall(ToolCallInfo),
    ToolCallUpdate(ToolCallUpdate),
    Plan(Vec<PlanEntry>),
    /// The agent is blocked until this request is answered.
    PermissionRequested {
        /// Harness-scoped id passed back to [`HarnessSession::respond_approval`].
        request_id: String,
        tool_call_id: Option<String>,
        title: String,
        /// Kind of the gated call, when the request itself carries it. The
        /// orchestrator falls back to the kind of the earlier `ToolCall` event.
        kind: Option<ToolKind>,
        /// Paths the gated call touches, when the request itself carries them.
        locations: Vec<String>,
        options: Vec<ApprovalOption>,
    },
    /// The current turn finished.
    TurnEnded {
        stop_reason: StopReason,
        error: Option<String>,
    },
    /// The agent process exited; no further events follow.
    Exited {
        error: Option<String>,
    },
}

/// A boxed stream of harness events.
pub type HarnessEventStream = Pin<Box<dyn Stream<Item = HarnessEvent> + Send + 'static>>;

/// Everything a harness needs to start a session.
#[derive(Clone)]
pub struct StartContext {
    pub session_id: SessionId,
    pub config: SessionConfig,
    /// Resume this native session instead of creating a new one.
    pub resume_native_session_id: Option<String>,
    /// Wraps the agent process according to `config.sandbox`.
    pub sandbox: Arc<dyn Sandbox>,
}

/// A started harness session.
pub struct StartedSession {
    pub session: Box<dyn HarnessSession>,
    pub events: HarnessEventStream,
}

/// Factory for agent sessions, registered under [`Harness::id`].
#[async_trait]
pub trait Harness: Send + Sync {
    /// Stable registry id, e.g. `"opencode"`.
    fn id(&self) -> &str;
    /// Static capabilities of sessions this harness creates.
    fn capabilities(&self) -> HarnessCapabilities;
    /// Spawns (or resumes) one agent session.
    async fn start(&self, ctx: StartContext) -> CoreResult<StartedSession>;
}

/// Control surface of one running agent session.
#[async_trait]
pub trait HarnessSession: Send + Sync {
    /// The agent's own session id, when known.
    fn native_session_id(&self) -> Option<String>;

    /// Starts a turn. Returns once accepted; completion arrives as `TurnEnded`.
    async fn prompt(&self, prompt: Prompt) -> CoreResult<()>;

    /// Injects a message into the running turn.
    async fn steer(&self, _prompt: Prompt) -> CoreResult<()> {
        Err(CoreError::Unsupported {
            harness: "unknown".into(),
            capability: "steer",
        })
    }

    /// Answers a `PermissionRequested` event.
    async fn respond_approval(
        &self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> CoreResult<()>;

    /// Cancels the running turn; the harness then emits `TurnEnded { Cancelled }`.
    async fn cancel(&self) -> CoreResult<()>;

    /// Terminates the agent process.
    async fn shutdown(&self) -> CoreResult<()>;
}
