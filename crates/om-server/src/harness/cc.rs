//! Claude Code (`cc`) `stream-json` headless CLI adapter and probe (W1-4).
//!
//! Invokes `claude -p --output-format stream-json --verbose [--resume <session_id>]`
//! per turn inside the session sandbox, parsing NDJSON events into [`HarnessEvent`]s.

use super::probe::{CliProbe, apply_safe_base_env, env_binary_or, probe_cli_version};
use super::stream_cli::{NdjsonTurnParser, StreamCliAdapter, start_stream_cli};
use async_trait::async_trait;
use om_core::CoreResult;
use om_core::content::ContentBlock;
use om_core::event::{ToolCallInfo, ToolCallStatus, ToolCallUpdate, ToolKind};
use om_core::harness::{Harness, HarnessCapabilities, HarnessEvent, StartContext, StartedSession};
use om_core::sandbox::CommandSpec;
use om_core::session::StopReason;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Environment variable overriding the `claude` binary path.
pub const ENV_CC_BIN: &str = "OM_CC_BIN";

/// Provider env vars forwarded only when `KeyProxy` is disabled.
const FORWARDED_PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

/// Configuration for launching Claude Code (`cc`).
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    /// Executable name or absolute path (default `"claude"`).
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

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            binary: "claude".into(),
            extra_args: Vec::new(),
            env: Vec::new(),
            readonly_paths: Vec::new(),
            network: true,
        }
    }
}

impl ClaudeCodeConfig {
    /// Builds configuration from host environment including Anthropic BYOK keys.
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
            binary: env_binary_or(ENV_CC_BIN, "claude"),
            ..Self::default()
        }
    }
}

/// [`Harness`] implementation for Claude Code (`"cc"`).
pub struct ClaudeCodeHarness {
    adapter: Arc<ClaudeCodeAdapter>,
}

struct ClaudeCodeAdapter {
    cfg: ClaudeCodeConfig,
}

pub fn cc_harness(cfg: ClaudeCodeConfig) -> ClaudeCodeHarness {
    ClaudeCodeHarness {
        adapter: Arc::new(ClaudeCodeAdapter { cfg }),
    }
}

pub async fn probe_cc(binary: &str) -> CliProbe {
    probe_cli_version(binary, &["--version"]).await
}

#[async_trait]
impl Harness for ClaudeCodeHarness {
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

impl StreamCliAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &str {
        "cc"
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
        let mut spec = apply_safe_base_env(CommandSpec::new(&self.cfg.binary))
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            .cwd(cwd);

        if let Some(id) = native_session_id {
            spec = spec.args(["--resume", id]);
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
        Box::new(ClaudeNdjsonParser::default())
    }
}

#[derive(Default)]
struct ClaudeNdjsonParser {
    emitted_text: bool,
    emitted_native_id: Option<String>,
    result_error: Option<String>,
}

fn infer_claude_tool_kind(name: &str) -> ToolKind {
    match name.to_ascii_lowercase().as_str() {
        "read" | "view" | "cat" => ToolKind::Read,
        "edit" | "write" | "multiedit" | "notebookedit" => ToolKind::Edit,
        "bash" | "shell" | "execute" => ToolKind::Execute,
        "glob" | "grep" | "search" | "ls" => ToolKind::Search,
        "webfetch" | "websearch" | "fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn extract_locations(input: Option<&Value>) -> Vec<String> {
    let Some(obj) = input.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut locs = Vec::new();
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(p) = obj.get(key).and_then(Value::as_str)
            && !p.is_empty()
        {
            locs.push(p.to_string());
        }
    }
    locs
}

fn extract_tool_result_text(content: Option<&Value>) -> String {
    let Some(v) = content else {
        return String::new();
    };
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    v.to_string()
}

impl NdjsonTurnParser for ClaudeNdjsonParser {
    fn on_json(&mut self, value: &Value) -> Vec<HarnessEvent> {
        let mut out = Vec::new();

        if let Some(sid) = value.get("session_id").and_then(Value::as_str)
            && !sid.is_empty()
            && self.emitted_native_id.as_deref() != Some(sid)
        {
            self.emitted_native_id = Some(sid.to_string());
            out.push(HarnessEvent::NativeSessionId(sid.to_string()));
        }

        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return out;
        };

        match kind {
            "assistant" => {
                if let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    self.emitted_text = true;
                                    out.push(HarnessEvent::MessageChunk(ContentBlock::Text {
                                        text: text.to_string(),
                                    }));
                                }
                            }
                            Some("thinking") => {
                                if let Some(text) = block
                                    .get("thinking")
                                    .or_else(|| block.get("text"))
                                    .and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    out.push(HarnessEvent::ThoughtChunk(ContentBlock::Text {
                                        text: text.to_string(),
                                    }));
                                }
                            }
                            Some("tool_use") => {
                                let id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string();
                                let name =
                                    block.get("name").and_then(Value::as_str).unwrap_or("tool");
                                let input = block.get("input").cloned();
                                let locations = extract_locations(input.as_ref());
                                out.push(HarnessEvent::ToolCall(ToolCallInfo {
                                    tool_call_id: id,
                                    title: name.to_string(),
                                    kind: infer_claude_tool_kind(name),
                                    status: ToolCallStatus::InProgress,
                                    raw_input: input,
                                    locations,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
            }
            "user" => {
                if let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) == Some("tool_result")
                            && let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                        {
                            let is_err = block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let text = extract_tool_result_text(block.get("content"));
                            out.push(HarnessEvent::ToolCallUpdate(ToolCallUpdate {
                                tool_call_id: id.to_string(),
                                status: Some(if is_err {
                                    ToolCallStatus::Failed
                                } else {
                                    ToolCallStatus::Completed
                                }),
                                content: if text.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![ContentBlock::Text { text }]
                                },
                                ..Default::default()
                            }));
                        }
                    }
                }
            }
            "stream_event" => {
                if let Some(delta) = value.pointer("/event/delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                self.emitted_text = true;
                                out.push(HarnessEvent::MessageChunk(ContentBlock::Text {
                                    text: text.to_string(),
                                }));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                out.push(HarnessEvent::ThoughtChunk(ContentBlock::Text {
                                    text: text.to_string(),
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                let is_error = value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some(result_text) = value.get("result").and_then(Value::as_str)
                    && !result_text.is_empty()
                {
                    if is_error {
                        self.result_error = Some(result_text.to_string());
                    } else if !self.emitted_text {
                        self.emitted_text = true;
                        out.push(HarnessEvent::MessageChunk(ContentBlock::Text {
                            text: result_text.to_string(),
                        }));
                    }
                } else if is_error {
                    self.result_error = Some("claude returned an error result".into());
                }
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
        if let Some(err) = self.result_error.take() {
            return (Vec::new(), StopReason::Failed, Some(err));
        }
        if exit_ok {
            (Vec::new(), StopReason::EndTurn, None)
        } else {
            let msg = if stderr_tail.trim().is_empty() {
                "claude process exited with non-zero status".to_string()
            } else {
                stderr_tail.trim().to_string()
            };
            (Vec::new(), StopReason::Failed, Some(msg))
        }
    }
}
