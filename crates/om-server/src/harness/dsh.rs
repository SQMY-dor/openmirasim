//! DeepSeek Harness (`dsh`) ACP wrapper and binary/version probe (W1-4).
//!
//! Launches `dsh acp` (configurable via `OM_DSH_BIN` / `OM_DSH_ACP_ARG`) through
//! [`crate::acp::AcpHarness`] with explicit `clear_env = true` isolation.

use super::probe::{CliProbe, apply_safe_base_env, env_binary_or, probe_cli_version};
use crate::acp::{AcpHarness, AcpHarnessOptions};
use om_core::sandbox::CommandSpec;

/// Environment variable overriding the `dsh` binary path.
pub const ENV_DSH_BIN: &str = "OM_DSH_BIN";

/// Environment variable overriding the ACP subcommand passed to `dsh` (default `"acp"`).
pub const ENV_DSH_ACP_ARG: &str = "OM_DSH_ACP_ARG";

/// Provider env vars forwarded only when `KeyProxy` is disabled.
const FORWARDED_PROVIDER_ENV: &[&str] = &[
    "DEEPSEEK_API_KEY",
    "DEEPSEEK_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
];

/// Configuration for launching DeepSeek Harness (`dsh`).
#[derive(Debug, Clone)]
pub struct DshConfig {
    /// Executable name or absolute path (default `"dsh"`).
    pub binary: String,
    /// ACP mode subcommand (default `"acp"`).
    pub acp_arg: String,
    /// Extra CLI arguments appended after `acp_arg`.
    pub extra_args: Vec<String>,
    /// Explicit environment variables injected into the agent process.
    pub env: Vec<(String, String)>,
    /// Underlying ACP transport / sandbox options.
    pub acp_options: AcpHarnessOptions,
}

impl Default for DshConfig {
    fn default() -> Self {
        Self {
            binary: "dsh".into(),
            acp_arg: "acp".into(),
            extra_args: Vec::new(),
            env: Vec::new(),
            acp_options: AcpHarnessOptions::default(),
        }
    }
}

impl DshConfig {
    /// Builds configuration from host environment variables including BYOK provider keys.
    pub fn from_env() -> Self {
        let mut cfg = Self::from_env_for_proxy();
        cfg.env = FORWARDED_PROVIDER_ENV
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|val| ((*key).to_string(), val)))
            .collect();
        cfg
    }

    /// Builds configuration without forwarding raw host API keys (use when `KeyProxy` is active).
    pub fn from_env_for_proxy() -> Self {
        let binary = env_binary_or(ENV_DSH_BIN, "dsh");
        let acp_arg = env_binary_or(ENV_DSH_ACP_ARG, "acp");
        Self {
            binary,
            acp_arg,
            extra_args: Vec::new(),
            env: Vec::new(),
            acp_options: AcpHarnessOptions::default(),
        }
    }

    /// Builds the sandboxed [`CommandSpec`] with `clear_env = true`.
    pub fn command_spec(&self) -> CommandSpec {
        let mut spec = apply_safe_base_env(CommandSpec::new(&self.binary));
        if !self.acp_arg.is_empty() {
            spec = spec.arg(&self.acp_arg);
        }
        for arg in &self.extra_args {
            spec = spec.arg(arg);
        }
        for (k, v) in &self.env {
            spec = spec.env(k, v);
        }
        spec
    }
}

/// Constructs an [`AcpHarness`] registered under `"dsh"`.
pub fn dsh_harness(cfg: DshConfig) -> AcpHarness {
    let spec = cfg.command_spec();
    AcpHarness::with_options("dsh", spec, cfg.acp_options)
}

/// Probes whether `dsh` exists on the host and responds to `--version`.
pub async fn probe_dsh(binary: &str) -> CliProbe {
    probe_cli_version(binary, &["--version"]).await
}
