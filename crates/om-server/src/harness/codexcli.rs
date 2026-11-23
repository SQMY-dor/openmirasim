//! Codex CLI (`codexcli`) `--json` headless adapter and probe (W1-4).
//!
//! Invokes `codex exec --json` (or `codex exec resume <thread_id> --json` on subsequent turns)
//! inside the session sandbox, parsing Codex NDJSON events into [`HarnessEvent`]s.

use super::probe::{CliProbe, apply_safe_base_env, env_binary_or, probe_cli_version};
use super::stream_cli::{NdjsonTurnParser, StreamCliAdapter, start_stream_cli};
use async_trait::async_trait;
use om_core::CoreResult;
use om_core::content::ContentBlock;
use om_core::event::{
    PlanEntry, PlanEntryStatus, ToolCallInfo, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use om_core::harness::{Harness, HarnessCapabilities, HarnessEvent, StartContext, StartedSession};
use om_core::sandbox::CommandSpec;
use om_core::session::StopReason;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Environment variable overriding the `codex` binary path.
pub const ENV_CODEX_BIN: &str = "OM_CODEX_BIN";

/// Provider env vars forwarded only when `KeyProxy` is disabled.
const FORWARDED_PROVIDER_ENV: &[&str] = &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_API_KEY"];

/// Configuration for launching Codex CLI (`codexcli`).
#[derive(Debug, Clone)]
pub struct CodexCliConfig {
    /// Executable name or absolute path (default `"codex"`).
    pub binary: String,
    /// Extra CLI arguments appended before the prompt.
    pub extra_args: Vec<String>,
    /// Explicit environment variables injected into the agent process.
    pub env: Vec<(String, String)>,
    /// Extra read-only host paths needed inside `BwrapSandbox`.
    pub readonly_paths: Vec<PathBuf>,
    /// Whether outbound network is allowed.
    pub network: bool,
}

impl Default for CodexCliConfig {
    fn default() -> Self {
        Self {
            binary: "codex".into(),
            extra_args: Vec::new(),
            env: Vec::new(),
            readonly_paths: Vec::new(),
            network: true,
        }
    }
}

impl CodexCliConfig {
    /// Builds configuration from host environment including OpenAI/Codex BYOK keys.
    pub fn from_env() -> Self {
        let mut cfg = Self::from_env_for_proxy();
        cfg.env = FORWARDED_PROVIDER_ENV
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|val| ((*key).to_string(), val)))
            .collect();
        cfg
    }

    /// Builds configuration without forwarding raw host API keys (for `KeyProxy` mode).
    pub fn from_env_for_proxy() -> Self {
        Self {
            binary: env_binary_or(ENV_CODEX_BIN, "codex"),
            ..Self::default()
        }
    }
}

/// [`Harness`] implementation for Codex CLI (`"codexcli"`).
pub struct CodexCliHarness {
    adapter: Arc<CodexCliAdapter>,
}

struct CodexCliAdapter {
    cfg: CodexCliConfig,
}

pub fn codexcli_harness(cfg: CodexCliConfig) -> CodexCliHarness {
    CodexCliHarness {
        adapter: Arc::new(CodexCliAdapter { cfg }),
    }
}

pub async fn probe_codexcli(binary: &str) -> CliProbe {
    probe_cli_version(binary, &["--version"]).await
}

#[async_trait]
impl Harness for CodexCliHarness {
    fn id(&self) -> &str {
        self.adapter.id()
    }

    fn capabilities(&self) -> HarnessCapabilities {
        self.adapter.capabilities()
    }

    async fn start(&self, ctx: StartContext) -> CoreResult<StartedSession> {
        start_stream_cli(Arc::clone(&self.adapter), ctx)
    }
}

impl StreamCliAdapter for CodexCliAdapter {
    fn id(&self) -> &str {
        "codexcli"
    }

    fn readonly_paths(&self) -> Vec<PathBuf> {
        self.cfg.readonly_paths.clone()
    }

    fn network(&self) -> bool {
        self.cfg.network
    }

    fn build_turn_command(
        &self,
        prompt_text: &str,
        cwd: &Path,
        model: Option<&str>,
        native_session_id: Option<&str>,
    ) -> CommandSpec {
        let mut spec = apply_safe_base_env(CommandSpec::new(&self.cfg.binary)).cwd(cwd);
        if let Some(thread_id) = native_session_id {
            spec = spec.args(["exec", "resume", thread_id, "--json"]);
        } else {
            spec = spec.args(["exec", "--json"]);
        }
        if let Some(m) = model {
            spec = spec.args(["--model", m]);
        }
        for arg in &self.cfg.extra_args {
            spec = spec.arg(arg);
        }
        spec = spec.arg(prompt_text);
        for (k, v) in &self.cfg.env {
            spec = spec.env(k, v);
        }
        spec
    }

    fn new_parser(&self) -> Box<dyn NdjsonTurnParser> {
        Box::new(CodexNdjsonParser::default())
    }
}

#[derive(Default)]
struct CodexNdjsonParser {
    turn_error: Option<String>,
}

fn item_status(item: &Value, completed_event: bool) -> ToolCallStatus {
    match item.get("status").and_then(Value::as_str) {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed") | Some("error") => ToolCallStatus::Failed,
        Some("in_progress") => ToolCallStatus::InProgress,
        _ if completed_event => {
            if item.get("exit_code").and_then(Value::as_i64).unwrap_or(0) != 0 {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            }
        }
        _ => ToolCallStatus::InProgress,
    }
}

impl NdjsonTurnParser for CodexNdjsonParser {
    fn on_json(&mut self, value: &Value) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return out;
        };

        match kind {
            "thread.started" => {
                if let Some(tid) = value.get("thread_id").and_then(Value::as_str)
                    && !tid.is_empty()
                {
                    out.push(HarnessEvent::NativeSessionId(tid.to_string()));
                }
            }
            "item.started" | "item.updated" | "item.completed" => {
                let Some(item) = value.get("item") else {
                    return out;
                };
                let is_completed = kind == "item.completed";
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("item")
                    .to_string();
                match item.get("type").and_then(Value::as_str) {
                    Some("agent_message") if is_completed => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(HarnessEvent::MessageChunk(ContentBlock::Text {
                                text: text.to_string(),
                            }));
                        }
                    }
                    Some("reasoning") if is_completed => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(HarnessEvent::ThoughtChunk(ContentBlock::Text {
                                text: text.to_string(),
                            }));
                        }
                    }
                    Some("command_execution") => {
                        let cmd = item
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("command");
                        if kind == "item.started" {
                            out.push(HarnessEvent::ToolCall(ToolCallInfo {
                                tool_call_id: item_id,
                                title: cmd.to_string(),
                                kind: ToolKind::Execute,
                                status: ToolCallStatus::InProgress,
                                raw_input: Some(serde_json::json!({ "command": cmd })),
                                locations: Vec::new(),
                            }));
                        } else {
                            let output = item
                                .get("aggregated_output")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            out.push(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
                                tool_call_id: item_id,
                                status: Some(item_status(item, is_completed)),
                                content: if output.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![ContentBlock::Text {
                                        text: output.to_string(),
                                    }]
                                },
                                ..Default::default()
                            }));
                        }
                    }
                    Some("file_change") => {
                        let paths: Vec<String> = item
                            .get("changes")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|c| c.get("path").and_then(Value::as_str))
                                    .map(ToOwned::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        if kind == "item.started" {
                            out.push(HarnessEvent::ToolCall(ToolCallInfo {
                                tool_call_id: item_id,
                                title: "Edit files".into(),
                                kind: ToolKind::Edit,
                                status: ToolCallStatus::InProgress,
                                raw_input: item.get("changes").cloned(),
                                locations: paths,
                            }));
                        } else {
                            out.push(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
                                tool_call_id: item_id,
                                status: Some(item_status(item, is_completed)),
                                content: if paths.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![ContentBlock::Text {
                                        text: paths.join("\n"),
                                    }]
                                },
                                ..Default::default()
                            }));
                        }
                    }
                    Some("mcp_tool_call") => {
                        let tool_name = item
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("mcp_tool");
                        if kind == "item.started" {
                            out.push(HarnessEvent::ToolCall(ToolCallInfo {
                                tool_call_id: item_id,
                                title: tool_name.to_string(),
                                kind: ToolKind::Other,
                                status: ToolCallStatus::InProgress,
                                raw_input: item.get("arguments").cloned(),
                                locations: Vec::new(),
                            }));
                        } else {
                            out.push(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
                                tool_call_id: item_id,
                                status: Some(item_status(item, is_completed)),
                                raw_output: item.get("result").cloned(),
                                ..Default::default()
                            }));
                        }
                    }
                    Some("todo_list") => {
                        if let Some(items) = item.get("items").and_then(Value::as_array) {
                            let entries = items
                                .iter()
                                .filter_map(|t| {
                                    let text = t.get("text").and_then(Value::as_str)?;
                                    let done = t
                                        .get("completed")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    Some(PlanEntry {
                                        content: text.to_string(),
                                        status: if done {
                                            PlanEntryStatus::Completed
                                        } else {
                                            PlanEntryStatus::Pending
                                        },
                                    })
                                })
                                .collect();
                            out.push(HarnessEvent::Plan(entries));
                        }
                    }
                    _ => {}
                }
            }
            "turn.failed" | "error" => {
                let msg = value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("codex turn failed");
                self.turn_error = Some(msg.to_string());
            }
            _ => {}
        }

        out
    }

    fn finish(
        &mut self,
        exit_ok: bool,
        stderr_tail: &str,
    ) -> (Vec<HarnessEvent>, StopReason, Option<String>) {
        if let Some(err) = self.turn_error.take() {
            return (Vec::new(), StopReason::Failed, Some(err));
        }
        if exit_ok {
            (Vec::new(), StopReason::EndTurn, None)
        } else {
            let msg = if stderr_tail.trim().is_empty() {
                "codex process exited with non-zero status".to_string()
            } else {
                stderr_tail.trim().to_string()
            };
            (Vec::new(), StopReason::Failed, Some(msg))
        }
    }
}
