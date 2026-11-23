//! Shared per-turn NDJSON (`stream-json`) subprocess runner for headless CLI agents
//! such as Claude Code (`cc`) and Codex CLI (`codexcli`).
//!
//! Each turn spawns one sandbox-wrapped CLI invocation, parses newline-delimited JSON
//! lines from stdout with a strict line-length cap, extracts/persists the agent's native
//! session/thread id for `--resume`, and supports cooperative cancellation + clean shutdown.

use crate::stream::ReceiverStream;
use async_trait::async_trait;
use om_core::approval::ApprovalDecision;
use om_core::content::{ContentBlock, Prompt};
use om_core::harness::{
    HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::sandbox::{CommandSpec, Sandbox, SandboxSpec};
use om_core::session::StopReason;
use om_core::{CoreError, CoreResult};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Maximum bytes accepted on a single NDJSON stdout line from a CLI subprocess (4 MiB).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Stateful per-turn NDJSON parser implemented by each concrete CLI adapter (`cc`, `codexcli`).
pub trait NdjsonTurnParser: Send + 'static {
    /// Parses one JSON object emitted on stdout, returning zero or more [`HarnessEvent`]s.
    fn on_json(&mut self, value: &Value) -> Vec<HarnessEvent>;

    /// Optional final events + stop reason override once stdout closes and exit status is known.
    fn finish(
        &mut self,
        exit_ok: bool,
        stderr_tail: &str,
    ) -> (Vec<HarnessEvent>, StopReason, Option<String>) {
        if exit_ok {
            (Vec::new(), StopReason::EndTurn, None)
        } else {
            let msg = if stderr_tail.trim().is_empty() {
                "CLI process exited with non-zero status".to_string()
            } else {
                stderr_tail.trim().to_string()
            };
            (Vec::new(), StopReason::Failed, Some(msg))
        }
    }
}

/// Strategy describing how a CLI adapter builds each turn's command and parses its NDJSON stream.
pub trait StreamCliAdapter: Send + Sync + 'static {
    fn id(&self) -> &str;

    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            native_approval: false,
            steer: false,
            resume: true,
            structured_tools: true,
            pty_attach: false,
            remote_screen: false,
        }
    }

    fn readonly_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn network(&self) -> bool {
        true
    }

    /// Builds the raw [`CommandSpec`] (before sandbox wrapping) for one turn.
    fn build_turn_command(
        &self,
        prompt_text: &str,
        cwd: &Path,
        model: Option<&str>,
        native_session_id: Option<&str>,
    ) -> CommandSpec;

    /// Creates a fresh per-turn NDJSON parser.
    fn new_parser(&self) -> Box<dyn NdjsonTurnParser>;
}

pub fn prompt_to_plain_text(prompt: &Prompt) -> String {
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

struct SessionState<A: StreamCliAdapter> {
    adapter: Arc<A>,
    cwd: PathBuf,
    model: Option<String>,
    sandbox: Arc<dyn Sandbox>,
    native_id: Mutex<Option<String>>,
    turn_active: AtomicBool,
    cancelled: AtomicBool,
    closed: AtomicBool,
    events: Mutex<Option<mpsc::UnboundedSender<HarnessEvent>>>,
    active_child: tokio::sync::Mutex<Option<Child>>,
    turn_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl<A: StreamCliAdapter> SessionState<A> {
    fn emit(&self, ev: HarnessEvent) {
        if let HarnessEvent::NativeSessionId(ref id) = ev
            && let Ok(mut guard) = self.native_id.lock()
        {
            *guard = Some(id.clone());
        }
        if let Ok(guard) = self.events.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(ev);
        }
    }

    fn native_id(&self) -> Option<String> {
        self.native_id.lock().ok().and_then(|g| g.clone())
    }
}

/// Starts a [`StreamCliSession`] for any [`StreamCliAdapter`].
pub fn start_stream_cli<A: StreamCliAdapter>(
    adapter: Arc<A>,
    ctx: StartContext,
) -> CoreResult<StartedSession> {
    let cwd = PathBuf::from(&ctx.config.cwd);
    if !cwd.is_absolute() {
        return Err(CoreError::InvalidInput(format!(
            "session cwd must be absolute: {}",
            ctx.config.cwd
        )));
    }

    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(ref native) = ctx.resume_native_session_id {
        let _ = tx.send(HarnessEvent::NativeSessionId(native.clone()));
    }

    let state = Arc::new(SessionState {
        adapter,
        cwd,
        model: ctx.config.model,
        sandbox: ctx.sandbox,
        native_id: Mutex::new(ctx.resume_native_session_id),
        turn_active: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        events: Mutex::new(Some(tx)),
        active_child: tokio::sync::Mutex::new(None),
        turn_task: tokio::sync::Mutex::new(None),
    });

    Ok(StartedSession {
        session: Box::new(StreamCliSession { state }),
        events: Box::pin(ReceiverStream::new(rx)),
    })
}

struct StreamCliSession<A: StreamCliAdapter> {
    state: Arc<SessionState<A>>,
}

async fn run_cli_turn<A: StreamCliAdapter>(state: Arc<SessionState<A>>, prompt_text: String) {
    let native = state.native_id();
    let base_cmd = state.adapter.build_turn_command(
        &prompt_text,
        &state.cwd,
        state.model.as_deref(),
        native.as_deref(),
    );

    let spec = SandboxSpec {
        writable_paths: vec![state.cwd.clone()],
        readonly_paths: state.adapter.readonly_paths(),
        network: state.adapter.network(),
        ..SandboxSpec::default()
    };

    let wrapped = match state.sandbox.wrap(&spec, base_cmd) {
        Ok(cmd) => cmd,
        Err(e) => {
            state.turn_active.store(false, Ordering::SeqCst);
            state.emit(HarnessEvent::TurnEnded {
                stop_reason: StopReason::Failed,
                error: Some(e.to_string()),
            });
            return;
        }
    };

    let mut command = Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if wrapped.clear_env {
        command.env_clear();
    }
    command.envs(&wrapped.env);
    if let Some(dir) = &wrapped.cwd {
        command.current_dir(dir);
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            state.turn_active.store(false, Ordering::SeqCst);
            state.emit(HarnessEvent::TurnEnded {
                stop_reason: StopReason::Failed,
                error: Some(format!("failed to spawn `{}`: {e}", wrapped.program)),
            });
            return;
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take();
    *state.active_child.lock().await = Some(child);

    // Collect up to 16 KiB of stderr for diagnostics without unbounded RAM growth.
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(err) = stderr {
            let mut limited = err.take(16 * 1024);
            let _ = limited.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut parser = state.adapter.new_parser();
    let mut reader = BufReader::new(stdout);
    let mut line_buf = Vec::new();
    let mut line_overflow = false;

    loop {
        line_buf.clear();
        let read_res = (&mut reader)
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line_buf)
            .await;
        match read_res {
            Ok(0) => break,
            Ok(_) => {
                if line_buf.len() > MAX_LINE_BYTES {
                    line_overflow = true;
                    tracing::warn!(
                        harness = %state.adapter.id(),
                        limit = MAX_LINE_BYTES,
                        "dropping oversized NDJSON line from CLI agent"
                    );
                    // Drain remaining bytes until the next newline or EOF.
                    if !line_buf.ends_with(b"\n") {
                        let mut discard = Vec::new();
                        loop {
                            discard.clear();
                            match (&mut reader)
                                .take(64 * 1024)
                                .read_until(b'\n', &mut discard)
                                .await
                            {
                                Ok(0) | Err(_) => break,
                                Ok(_) if discard.ends_with(b"\n") => break,
                                Ok(_) => {}
                            }
                        }
                    }
                    continue;
                }
                let line = String::from_utf8_lossy(&line_buf);
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(val) => {
                        for ev in parser.on_json(&val) {
                            state.emit(ev);
                        }
                    }
                    Err(_) => {
                        tracing::debug!(
                            harness = %state.adapter.id(),
                            "ignoring non-JSON stdout line from CLI agent"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::debug!(harness = %state.adapter.id(), error = %e, "stdout read ended");
                break;
            }
        }
    }

    let exit_status = {
        let mut child_guard = state.active_child.lock().await;
        if let Some(mut child) = child_guard.take() {
            child.wait().await.ok()
        } else {
            None
        }
    };
    let stderr_tail = stderr_task.await.unwrap_or_default();

    state.turn_active.store(false, Ordering::SeqCst);

    if state.cancelled.load(Ordering::SeqCst) {
        state.emit(HarnessEvent::TurnEnded {
            stop_reason: StopReason::Cancelled,
            error: None,
        });
        return;
    }

    let exit_ok = exit_status.is_some_and(|s| s.success()) && !line_overflow;
    let (tail_events, stop_reason, error) = parser.finish(exit_ok, &stderr_tail);
    for ev in tail_events {
        state.emit(ev);
    }
    let error = if line_overflow && error.is_none() {
        Some("agent emitted an oversized output line".into())
    } else {
        error
    };
    state.emit(HarnessEvent::TurnEnded { stop_reason, error });
}

#[async_trait]
impl<A: StreamCliAdapter> HarnessSession for StreamCliSession<A> {
    fn native_session_id(&self) -> Option<String> {
        self.state.native_id()
    }

    async fn prompt(&self, prompt: Prompt) -> CoreResult<()> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(CoreError::Harness(format!(
                "harness '{}' session is closed",
                self.state.adapter.id()
            )));
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
            return Err(CoreError::InvalidInput("a turn is already running".into()));
        }
        self.state.cancelled.store(false, Ordering::SeqCst);
        let text = prompt_to_plain_text(&prompt);
        let state = Arc::clone(&self.state);
        let handle = tokio::spawn(run_cli_turn(state, text));
        *self.state.turn_task.lock().await = Some(handle);
        Ok(())
    }

    async fn respond_approval(
        &self,
        _request_id: &str,
        decision: ApprovalDecision,
    ) -> CoreResult<()> {
        if matches!(decision, ApprovalDecision::Cancelled) {
            return Ok(());
        }
        Err(CoreError::Unsupported {
            harness: self.state.adapter.id().to_string(),
            capability: "native_approval",
        })
    }

    async fn cancel(&self) -> CoreResult<()> {
        self.state.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.state.active_child.lock().await.as_mut() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    async fn shutdown(&self) -> CoreResult<()> {
        if self.state.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.state.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.state.active_child.lock().await.as_mut() {
            let _ = child.start_kill();
        }
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
