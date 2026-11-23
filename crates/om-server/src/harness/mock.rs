//! Deterministic in-process [`MockHarness`] implementing [`om_core::harness::Harness`].
//!
//! Supports:
//! - Standard streaming turns (`Plan` -> `ThoughtChunk` -> `ToolCall` -> `ToolCallUpdate` -> `MessageChunk` -> `TurnEnded`)
//! - Approval-gated turns when the prompt contains `approve`, `审批`, or `deploy`
//! - Slow streaming turns (`slow`) for deterministic cancellation testing
//! - Explicit failure turns (`fail`) for error-path testing
//! - Native session id preservation across `resume_native_session_id`

use crate::stream::ReceiverStream;
use async_trait::async_trait;
use om_core::approval::{ApprovalDecision, ApprovalOption, ApprovalOptionKind};
use om_core::content::{ContentBlock, Prompt};
use om_core::event::{
    PlanEntry, PlanEntryStatus, ToolCallInfo, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use om_core::harness::{
    Harness, HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::session::StopReason;
use om_core::{CoreError, CoreResult};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Configuration for [`MockHarness`].
#[derive(Debug, Clone)]
pub struct MockHarnessConfig {
    /// Artificial delay between emitted chunks (use `Duration::ZERO` in unit tests).
    pub chunk_delay: Duration,
    /// Delay per chunk for prompts containing `"slow"`.
    pub slow_chunk_delay: Duration,
}

impl Default for MockHarnessConfig {
    fn default() -> Self {
        Self {
            chunk_delay: Duration::from_millis(25),
            slow_chunk_delay: Duration::from_millis(40),
        }
    }
}

/// Fast, deterministic in-process harness registered under `"mock"`.
pub struct MockHarness {
    config: MockHarnessConfig,
    seq: AtomicU64,
}

impl Default for MockHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl MockHarness {
    pub fn new() -> Self {
        Self::with_config(MockHarnessConfig::default())
    }

    pub fn instant() -> Self {
        Self::with_config(MockHarnessConfig {
            chunk_delay: Duration::ZERO,
            slow_chunk_delay: Duration::from_millis(25),
        })
    }

    pub fn with_config(config: MockHarnessConfig) -> Self {
        Self {
            config,
            seq: AtomicU64::new(1),
        }
    }
}

#[async_trait]
impl Harness for MockHarness {
    fn id(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            native_approval: true,
            steer: false,
            resume: true,
            structured_tools: true,
            pty_attach: false,
            remote_screen: false,
        }
    }

    async fn start(&self, ctx: StartContext) -> CoreResult<StartedSession> {
        let native_id = ctx.resume_native_session_id.unwrap_or_else(|| {
            let n = self.seq.fetch_add(1, Ordering::Relaxed);
            format!("mock-session-{n}")
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(HarnessEvent::NativeSessionId(native_id.clone()));

        let state = Arc::new(MockSessionState {
            native_id,
            cwd: ctx.config.cwd,
            config: self.config.clone(),
            counter: AtomicU64::new(1),
            turn_active: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            events: Mutex::new(Some(tx)),
            pending_approvals: Mutex::new(HashMap::new()),
            turn_task: tokio::sync::Mutex::new(None),
        });

        Ok(StartedSession {
            session: Box::new(MockSession { state }),
            events: Box::pin(ReceiverStream::new(rx)),
        })
    }
}

struct MockSessionState {
    native_id: String,
    cwd: String,
    config: MockHarnessConfig,
    counter: AtomicU64,
    turn_active: AtomicBool,
    cancelled: AtomicBool,
    closed: AtomicBool,
    events: Mutex<Option<mpsc::UnboundedSender<HarnessEvent>>>,
    pending_approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    turn_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl MockSessionState {
    fn emit(&self, ev: HarnessEvent) {
        if let Ok(guard) = self.events.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(ev);
        }
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{n}")
    }

    fn cancel_pending_approvals(&self) {
        let senders: Vec<_> = self
            .pending_approvals
            .lock()
            .map(|mut map| map.drain().map(|(_, tx)| tx).collect())
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.send(ApprovalDecision::Cancelled);
        }
    }
}

struct MockSession {
    state: Arc<MockSessionState>,
}

fn prompt_to_plain_text(prompt: &Prompt) -> String {
    prompt
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::ResourceLink { uri, .. } => uri.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn maybe_sleep(delay: Duration) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

async fn run_mock_turn(state: Arc<MockSessionState>, prompt_text: String) {
    let lower = prompt_text.to_lowercase();
    let wants_fail = lower.contains("fail") || lower.contains("触发失败");
    let wants_slow = lower.contains("slow") || lower.contains("慢速");
    let wants_approval =
        lower.contains("approve") || lower.contains("deploy") || prompt_text.contains("审批");

    if wants_fail {
        maybe_sleep(state.config.chunk_delay).await;
        state.turn_active.store(false, Ordering::SeqCst);
        state.emit(HarnessEvent::TurnEnded {
            stop_reason: StopReason::Failed,
            error: Some("mock harness simulated failure".into()),
        });
        return;
    }

    if wants_slow {
        for idx in 1..=20 {
            if state.cancelled.load(Ordering::SeqCst) {
                state.turn_active.store(false, Ordering::SeqCst);
                state.emit(HarnessEvent::TurnEnded {
                    stop_reason: StopReason::Cancelled,
                    error: None,
                });
                return;
            }
            state.emit(HarnessEvent::MessageChunk(ContentBlock::Text {
                text: format!("chunk-{idx} "),
            }));
            maybe_sleep(state.config.slow_chunk_delay).await;
        }
        state.turn_active.store(false, Ordering::SeqCst);
        state.emit(HarnessEvent::TurnEnded {
            stop_reason: StopReason::EndTurn,
            error: None,
        });
        return;
    }

    let call_id = state.next_id("mock-call");
    state.emit(HarnessEvent::Plan(vec![
        PlanEntry {
            content: "分析指令与工作区上下文".into(),
            status: PlanEntryStatus::InProgress,
        },
        PlanEntry {
            content: if wants_approval {
                "发起高危命令审批并等待确认".into()
            } else {
                "整理回复并结束本轮".into()
            },
            status: PlanEntryStatus::Pending,
        },
    ]));

    maybe_sleep(state.config.chunk_delay).await;
    if state.cancelled.load(Ordering::SeqCst) {
        return finish_cancelled(&state);
    }

    state.emit(HarnessEvent::ThoughtChunk(ContentBlock::Text {
        text: format!("Inspecting workspace {} for prompt.", state.cwd),
    }));

    if wants_approval {
        state.emit(HarnessEvent::ToolCall(ToolCallInfo {
            tool_call_id: call_id.clone(),
            title: "Execute systemctl reload openmirasim-server".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            raw_input: Some(json!({
                "cmd": "systemctl reload openmirasim-server",
                "cwd": state.cwd,
            })),
            locations: vec![state.cwd.clone()],
        }));

        let req_id = state.next_id("mock-perm");
        let (resp_tx, resp_rx) = oneshot::channel();
        if let Ok(mut map) = state.pending_approvals.lock() {
            map.insert(req_id.clone(), resp_tx);
        }

        state.emit(HarnessEvent::PermissionRequested {
            request_id: req_id.clone(),
            tool_call_id: Some(call_id.clone()),
            title: "执行受控命令 `systemctl reload openmirasim-server`".into(),
            options: vec![
                ApprovalOption {
                    option_id: "allow_once".into(),
                    name: "允许一次".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    option_id: "allow_always".into(),
                    name: "始终允许".into(),
                    kind: ApprovalOptionKind::AllowAlways,
                },
                ApprovalOption {
                    option_id: "reject_once".into(),
                    name: "拒绝".into(),
                    kind: ApprovalOptionKind::RejectOnce,
                },
            ],
        });

        let decision = resp_rx.await.unwrap_or(ApprovalDecision::Cancelled);
        if state.cancelled.load(Ordering::SeqCst) || matches!(decision, ApprovalDecision::Cancelled)
        {
            state.emit(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: call_id,
                status: Some(ToolCallStatus::Failed),
                content: vec![ContentBlock::Text {
                    text: "cancelled".into(),
                }],
                ..Default::default()
            }));
            return finish_cancelled(&state);
        }

        let allowed = matches!(
            &decision,
            ApprovalDecision::Selected { option_id }
                if option_id == "allow_once" || option_id == "allow_always"
        );

        state.emit(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: call_id,
            status: Some(if allowed {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            }),
            content: vec![ContentBlock::Text {
                text: if allowed {
                    "Reloaded openmirasim-server.service (exit 0)".into()
                } else {
                    "Operation rejected by member".into()
                },
            }],
            ..Default::default()
        }));

        state.emit(HarnessEvent::Plan(vec![
            PlanEntry {
                content: "分析指令与工作区上下文".into(),
                status: PlanEntryStatus::Completed,
            },
            PlanEntry {
                content: "发起高危命令审批并等待确认".into(),
                status: PlanEntryStatus::Completed,
            },
        ]));

        let reply = if allowed {
            format!("审批已通过并执行完成（工作区：{}）。", state.cwd)
        } else {
            "审批已拒绝，未执行受控命令。".into()
        };
        state.emit(HarnessEvent::MessageChunk(ContentBlock::Text {
            text: reply,
        }));
    } else {
        state.emit(HarnessEvent::ToolCall(ToolCallInfo {
            tool_call_id: call_id.clone(),
            title: "Read workspace status".into(),
            kind: ToolKind::Search,
            status: ToolCallStatus::InProgress,
            raw_input: Some(json!({ "cwd": state.cwd })),
            locations: vec![state.cwd.clone()],
        }));

        maybe_sleep(state.config.chunk_delay).await;
        if state.cancelled.load(Ordering::SeqCst) {
            return finish_cancelled(&state);
        }

        state.emit(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: call_id,
            status: Some(ToolCallStatus::Completed),
            content: vec![ContentBlock::Text {
                text: "Workspace ready".into(),
            }],
            ..Default::default()
        }));

        state.emit(HarnessEvent::Plan(vec![
            PlanEntry {
                content: "分析指令与工作区上下文".into(),
                status: PlanEntryStatus::Completed,
            },
            PlanEntry {
                content: "整理回复并结束本轮".into(),
                status: PlanEntryStatus::Completed,
            },
        ]));

        let chunks = [
            format!("mock echo: {prompt_text}"),
            format!(" (cwd={})", state.cwd),
        ];
        for chunk in chunks {
            maybe_sleep(state.config.chunk_delay).await;
            if state.cancelled.load(Ordering::SeqCst) {
                return finish_cancelled(&state);
            }
            state.emit(HarnessEvent::MessageChunk(ContentBlock::Text {
                text: chunk,
            }));
        }
    }

    state.turn_active.store(false, Ordering::SeqCst);
    state.emit(HarnessEvent::TurnEnded {
        stop_reason: StopReason::EndTurn,
        error: None,
    });
}

fn finish_cancelled(state: &MockSessionState) {
    state.turn_active.store(false, Ordering::SeqCst);
    state.emit(HarnessEvent::TurnEnded {
        stop_reason: StopReason::Cancelled,
        error: None,
    });
}

#[async_trait]
impl HarnessSession for MockSession {
    fn native_session_id(&self) -> Option<String> {
        Some(self.state.native_id.clone())
    }

    async fn prompt(&self, prompt: Prompt) -> CoreResult<()> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(CoreError::Harness("mock session is closed".into()));
        }
        if prompt.is_empty() {
            return Err(CoreError::InvalidInput("prompt must not be empty".into()));
        }
        if self
            .state
            .turn_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(CoreError::InvalidInput(
                "mock session already has an active turn".into(),
            ));
        }

        self.state.cancelled.store(false, Ordering::SeqCst);
        let text = prompt_to_plain_text(&prompt);
        let state = Arc::clone(&self.state);
        let handle = tokio::spawn(run_mock_turn(state, text));
        *self.state.turn_task.lock().await = Some(handle);
        Ok(())
    }

    async fn respond_approval(
        &self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> CoreResult<()> {
        let sender = self
            .state
            .pending_approvals
            .lock()
            .ok()
            .and_then(|mut map| map.remove(request_id));
        match sender {
            Some(tx) => {
                let _ = tx.send(decision);
                Ok(())
            }
            None => {
                if matches!(decision, ApprovalDecision::Cancelled) {
                    Ok(())
                } else {
                    Err(CoreError::NotFound(format!(
                        "pending approval request '{request_id}' not found"
                    )))
                }
            }
        }
    }

    async fn cancel(&self) -> CoreResult<()> {
        self.state.cancelled.store(true, Ordering::SeqCst);
        self.state.cancel_pending_approvals();
        Ok(())
    }

    async fn shutdown(&self) -> CoreResult<()> {
        if self.state.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.state.cancelled.store(true, Ordering::SeqCst);
        self.state.cancel_pending_approvals();
        if let Some(handle) = self.state.turn_task.lock().await.take() {
            let _ = handle.await;
        }
        self.state.emit(HarnessEvent::Exited { error: None });
        if let Ok(mut guard) = self.state.events.lock() {
            guard.take();
        }
        Ok(())
    }
}
