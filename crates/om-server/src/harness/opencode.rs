//! OpenCode ACP harness wrapper and binary/version probe (W0-6).
//!
//! Launches `opencode acp` via [`crate::acp::AcpHarness`] with explicit BYOK environment
//! allowlisting so server tokens are never leaked into agent child processes.

use super::probe::{apply_safe_base_env, env_binary_or, probe_cli_version};
use crate::acp::{AcpHarness, AcpHarnessOptions};
use om_core::sandbox::CommandSpec;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Environment variable overriding the `opencode` binary path.
pub const ENV_OPENCODE_BIN: &str = "OM_OPENCODE_BIN";

/// BYOK / model-provider env vars forwarded only when the server is NOT using [`crate::proxy::KeyProxy`].
const FORWARDED_PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
];

/// Configuration for launching `opencode acp`.
#[derive(Debug, Clone)]
pub struct OpenCodeConfig {
    /// Executable name or absolute path (default `"opencode"`).
    pub binary: String,
    /// Extra CLI arguments appended after `acp`.
    pub extra_args: Vec<String>,
    /// Explicit environment variables injected into the agent process.
    pub env: Vec<(String, String)>,
    /// Underlying ACP transport / sandbox options.
    pub acp_options: AcpHarnessOptions,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            binary: "opencode".into(),
            extra_args: Vec::new(),
            env: Vec::new(),
            acp_options: AcpHarnessOptions::default(),
        }
    }
}

impl OpenCodeConfig {
    /// Builds configuration from host environment variables (`OM_OPENCODE_BIN` plus
    /// allowlisted BYOK provider keys). Server auth tokens (`OM_*`) are never forwarded.
    pub fn from_env() -> Self {
        let mut cfg = Self::from_env_for_proxy();
        cfg.env = FORWARDED_PROVIDER_ENV
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|val| ((*key).to_string(), val)))
            .collect();
        cfg
    }

    /// Builds configuration from `OM_OPENCODE_BIN` **without** forwarding raw host API keys
    /// or `OPENCODE_CONFIG` / `OPENCODE_CONFIG_CONTENT` (which may embed upstream credentials).
    ///
    /// Use this when [`crate::proxy::KeyProxy`] or strict [`crate::sandbox::BwrapSandbox`]
    /// key isolation is active so raw upstream secrets are never placed in `CommandSpec::env`.
    pub fn from_env_for_proxy() -> Self {
        Self {
            binary: env_binary_or(ENV_OPENCODE_BIN, "opencode"),
            extra_args: Vec::new(),
            env: Vec::new(),
            acp_options: AcpHarnessOptions::default(),
        }
    }

    /// Builds the [`CommandSpec`] executed inside the session sandbox.
    ///
    /// Always sets `clear_env = true` and copies only [`super::probe::SAFE_BASE_ENV`] plus
    /// explicit `self.env` so `OM_TOKEN` and other server secrets never leak under `DirectSandbox`.
    pub fn command_spec(&self) -> CommandSpec {
        let mut spec = apply_safe_base_env(CommandSpec::new(&self.binary)).arg("acp");
        for arg in &self.extra_args {
            spec = spec.arg(arg);
        }
        for (k, v) in &self.env {
            spec = spec.env(k, v);
        }
        spec
    }
}

/// Constructs an [`AcpHarness`] registered under `"opencode"`.
pub fn opencode_harness(cfg: OpenCodeConfig) -> AcpHarness {
    let spec = cfg.command_spec();
    AcpHarness::with_options("opencode", spec, cfg.acp_options)
}

/// Result of probing an `opencode` installation on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCodeProbe {
    pub available: bool,
    pub binary: String,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// Checks whether the configured `opencode` binary exists and responds to `--version`.
pub async fn probe_opencode(binary: &str) -> OpenCodeProbe {
    let p = probe_cli_version(binary, &["--version"]).await;
    OpenCodeProbe {
        available: p.available,
        binary: p.binary,
        binary_path: p.binary_path,
        version: p.version,
        error: p.error,
    }
}
