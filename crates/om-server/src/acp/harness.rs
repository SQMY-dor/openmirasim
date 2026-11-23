use super::ACP_PROTOCOL_VERSION;
use super::mapping;
use super::rpc::{Incoming, RpcConnection, RpcError};
use crate::stream::ReceiverStream;
use async_trait::async_trait;
use om_core::approval::ApprovalDecision;
use om_core::content::Prompt;
use om_core::harness::{
    Harness, HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::sandbox::{CommandSpec, SandboxSpec};
use om_core::session::StopReason;
use om_core::{CoreError, CoreResult};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Tunables for an [`AcpHarness`].
#[derive(Debug, Clone)]
pub struct AcpHarnessOptions {
    /// Timeout for `initialize` and `session/new` / `session/load`.
    pub init_timeout: Duration,
    /// Extra read-only paths for the sandbox (agent binary, toolchains, ...).
    pub readonly_paths: Vec<PathBuf>,
    /// Whether the sandbox should allow outbound network (agents call model APIs).
    pub network: bool,
}

impl Default for AcpHarnessOptions {
    fn default() -> Self {
        Self {
            init_timeout: Duration::from_secs(30),
            readonly_paths: Vec::new(),
            network: true,
        }
    }
}

/// Drives any ACP-speaking agent CLI.
pub struct AcpHarness {
    id: String,
    command: CommandSpec,
    options: AcpHarnessOptions,
}

impl AcpHarness {
    /// `command` is the agent invocation, e.g. `CommandSpec::new("opencode").arg("acp")`.
    pub fn new(id: impl Into<String>, command: CommandSpec) -> Self {
        Self::with_options(id, command, AcpHarnessOptions::default())
    }

    pub fn with_options(
        id: impl Into<String>,
        command: CommandSpec,
        options: AcpHarnessOptions,
    ) -> Self {
        Self {
            id: id.into(),
            command,
            options,
        }
    }
}

struct State {
    harness_id: String,
    conn: RpcConnection,
    native_id: Mutex<Option<String>>,
    turn_active: AtomicBool,
    turn_task: Mutex<Option<JoinHandle<()>>>,
    /// request_id -> JSON-RPC id of a pending `session/request_permission`.
    permissions: Mutex<HashMap<String, Value>>,
    /// tool_call_id -> title, to label permission requests that only carry an id.
    tool_titles: Mutex<HashMap<String, String>>,
    /// Suppresses replayed history while `session/load` runs.
    loading: AtomicBool,
    events: Mutex<Option<mpsc::UnboundedSender<HarnessEvent>>>,
    child: tokio::sync::Mutex<Child>,
}

impl State {
    fn emit(&self, ev: HarnessEvent) {
        if let Ok(guard) = self.events.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(ev);
        }
    }

    fn native_id(&self) -> Option<String> {
        self.native_id.lock().ok().and_then(|g| g.clone())
    }

    fn take_permissions(&self) -> Vec<Value> {
        self.permissions
            .lock()
            .map(|mut p| p.drain().map(|(_, id)| id).collect())
            .unwrap_or_default()
    }
}

fn cancelled_outcome() -> Value {
    json!({"outcome": {"outcome": "cancelled"}})
}

async fn handle_incoming(state: Arc<State>, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
    while let Some(msg) = incoming.recv().await {
        match msg {
            Incoming::Notification { method, params } if method == "session/update" => {
                if state.loading.load(Ordering::SeqCst) {
                    continue;
                }
                let Some(update) = params.get("update") else {
                    continue;
                };
                if let Some(ev) = mapping::session_update_to_event(update) {
                    if let HarnessEvent::ToolCall(info) = &ev
                        && let Ok(mut titles) = state.tool_titles.lock()
                    {
                        titles.insert(info.tool_call_id.clone(), info.title.clone());
                    }
                    state.emit(ev);
                }
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(harness = %state.harness_id, %method, "ignoring ACP notification");
            }
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
                let tool_call_id = tool_call
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let title = tool_call
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        let titles = state.tool_titles.lock().ok()?;
                        titles.get(tool_call_id.as_deref()?).cloned()
                    })
                    .unwrap_or_else(|| "Permission requested".to_owned());
                let options = mapping::permission_options_from_acp(
                    params.get("options").unwrap_or(&Value::Null),
                );
                if options.is_empty() {
                    let _ = state.conn.respond(id, Ok(cancelled_outcome()));
                    continue;
                }
                let request_id = id.to_string();
                if let Ok(mut p) = state.permissions.lock() {
                    p.insert(request_id.clone(), id);
                }
                state.emit(HarnessEvent::PermissionRequested {
                    request_id,
                    tool_call_id,
                    title,
                    options,
                });
            }
            Incoming::Request { id, method, .. } => {
                // fs/* and terminal/* are not advertised; anything else is unknown.
                let _ = state.conn.respond(
                    id,
                    Err(RpcError::new(
                        RpcError::METHOD_NOT_FOUND,
                        format!("method `{method}` is not supported by OpenMirasim"),
                    )),
                );
            }
            Incoming::Closed => break,
        }
    }

    // The agent's stdout closed. Let an in-flight turn report its failure first so that
    // `TurnEnded` always precedes `Exited`.
    let turn = state.turn_task.lock().ok().and_then(|mut t| t.take());
    if let Some(turn) = turn {
        let _ = turn.await;
    }
    let status = {
        let mut child = state.child.lock().await;
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .ok()
            .and_then(Result::ok)
    };
    let error = match status {
        Some(s) if s.success() => None,
        Some(s) => Some(format!("agent exited with {s}")),
        None => Some("agent stopped responding".to_owned()),
    };
    state.emit(HarnessEvent::Exited { error });
    if let Ok(mut tx) = state.events.lock() {
        tx.take();
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> &str {
        &self.id
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
        let cwd = PathBuf::from(&ctx.config.cwd);
        if !cwd.is_absolute() {
            return Err(CoreError::InvalidInput(format!(
                "session cwd must be absolute: {}",
                ctx.config.cwd
            )));
        }
        let spec = SandboxSpec {
            writable_paths: vec![cwd.clone()],
            readonly_paths: self.options.readonly_paths.clone(),
            network: self.options.network,
            ..SandboxSpec::default()
        };
        let mut base = self.command.clone();
        base.cwd = Some(cwd.clone());
        let cmd = ctx.sandbox.wrap(&spec, base)?;

        let mut command = Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if cmd.clear_env {
            command.env_clear();
        }
        command.envs(&cmd.env);
        if let Some(dir) = &cmd.cwd {
            command.current_dir(dir);
        }
        let mut child = command
            .spawn()
            .map_err(|e| CoreError::Harness(format!("failed to spawn `{}`: {e}", cmd.program)))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        if let Some(stderr) = child.stderr.take() {
            let harness = self.id.clone();
            let session = ctx.session_id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(%harness, %session, "agent stderr: {line}");
                }
            });
        }

        let (conn, incoming) = RpcConnection::spawn(stdout, stdin);
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(State {
            harness_id: self.id.clone(),
            conn: conn.clone(),
            native_id: Mutex::new(None),
            turn_active: AtomicBool::new(false),
            turn_task: Mutex::new(None),
            permissions: Mutex::default(),
            tool_titles: Mutex::default(),
            loading: AtomicBool::new(false),
            events: Mutex::new(Some(tx)),
            child: tokio::sync::Mutex::new(child),
        });
        tokio::spawn(handle_incoming(state.clone(), incoming));

        match self.handshake(&state, &ctx, &cwd).await {
            Ok(native_id) => {
                if let Ok(mut n) = state.native_id.lock() {
                    *n = Some(native_id.clone());
                }
                state.emit(HarnessEvent::NativeSessionId(native_id));
                Ok(StartedSession {
                    session: Box::new(AcpSession { state }),
                    events: Box::pin(ReceiverStream::new(rx)),
                })
            }
            Err(e) => {
                let _ = state.child.lock().await.start_kill();
                Err(e)
            }
        }
    }
}

impl AcpHarness {
    async fn handshake(
        &self,
        state: &State,
        ctx: &StartContext,
        cwd: &std::path::Path,
    ) -> CoreResult<String> {
        let timeout = Some(self.options.init_timeout);
        let init = state
            .conn
            .request(
                "initialize",
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false
                    },
                    "clientInfo": {"name": "openmirasim", "version": om_core::crate_version()}
                }),
                timeout,
            )
            .await?;
        let version = init.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(ACP_PROTOCOL_VERSION) {
            return Err(CoreError::Harness(format!(
                "agent `{}` speaks ACP version {version:?}, expected {ACP_PROTOCOL_VERSION}",
                self.id
            )));
        }
        let can_load = init
            .pointer("/agentCapabilities/loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cwd = cwd.to_string_lossy();

        if let Some(native) = &ctx.resume_native_session_id {
            if !can_load {
                return Err(CoreError::Unsupported {
                    harness: self.id.clone(),
                    capability: "resume (session/load)",
                });
            }
            state.loading.store(true, Ordering::SeqCst);
            let result = state
                .conn
                .request(
                    "session/load",
                    json!({"sessionId": native, "cwd": cwd, "mcpServers": []}),
                    timeout,
                )
                .await;
            state.loading.store(false, Ordering::SeqCst);
            result?;
            Ok(native.clone())
        } else {
            let created = state
                .conn
                .request(
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": []}),
                    timeout,
                )
                .await?;
            created
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| CoreError::Harness("session/new returned no sessionId".into()))
        }
    }
}

struct AcpSession {
    state: Arc<State>,
}

impl AcpSession {
    fn native(&self) -> CoreResult<String> {
        self.state
            .native_id()
            .ok_or_else(|| CoreError::Harness("ACP session not initialized".into()))
    }
}

#[async_trait]
impl HarnessSession for AcpSession {
    fn native_session_id(&self) -> Option<String> {
        self.state.native_id()
    }

    async fn prompt(&self, prompt: Prompt) -> CoreResult<()> {
        let native = self.native()?;
        if self.state.turn_active.swap(true, Ordering::SeqCst) {
            return Err(CoreError::InvalidInput("a turn is already running".into()));
        }
        let state = self.state.clone();
        let params = json!({"sessionId": native, "prompt": mapping::prompt_to_acp(&prompt)});
        let task = tokio::spawn(async move {
            let result = state.conn.request("session/prompt", params, None).await;
            state.turn_active.store(false, Ordering::SeqCst);
            // Any permission still open belongs to the finished turn.
            for id in state.take_permissions() {
                let _ = state.conn.respond(id, Ok(cancelled_outcome()));
            }
            let ev = match result {
                Ok(v) => HarnessEvent::TurnEnded {
                    stop_reason: mapping::stop_reason_from_acp(&v),
                    error: None,
                },
                Err(e) => HarnessEvent::TurnEnded {
                    stop_reason: StopReason::Failed,
                    error: Some(e.to_string()),
                },
            };
            state.emit(ev);
        });
        if let Ok(mut t) = self.state.turn_task.lock() {
            *t = Some(task);
        }
        Ok(())
    }

    async fn respond_approval(
        &self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> CoreResult<()> {
        let id = self
            .state
            .permissions
            .lock()
            .ok()
            .and_then(|mut p| p.remove(request_id));
        let Some(id) = id else {
            // Already answered (e.g. auto-cancelled with its turn): cancelling is idempotent.
            return match decision {
                ApprovalDecision::Cancelled => Ok(()),
                ApprovalDecision::Selected { .. } => Err(CoreError::NotFound(format!(
                    "permission request {request_id}"
                ))),
            };
        };
        let outcome = match decision {
            ApprovalDecision::Selected { option_id } => {
                json!({"outcome": {"outcome": "selected", "optionId": option_id}})
            }
            ApprovalDecision::Cancelled => cancelled_outcome(),
        };
        self.state.conn.respond(id, Ok(outcome))
    }

    async fn cancel(&self) -> CoreResult<()> {
        let native = self.native()?;
        self.state
            .conn
            .notify("session/cancel", json!({"sessionId": native}))?;
        // ACP: the client MUST answer pending permission requests with `cancelled`.
        for id in self.state.take_permissions() {
            let _ = self.state.conn.respond(id, Ok(cancelled_outcome()));
        }
        Ok(())
    }

    async fn shutdown(&self) -> CoreResult<()> {
        let mut child = self.state.child.lock().await;
        let _ = child.start_kill();
        Ok(())
    }
}
