//! OpenCode ACP harness wrapper and binary/version probe (W0-6).
//!
//! Launches `opencode acp` via [`crate::acp::AcpHarness`] with explicit BYOK environment
//! allowlisting so server tokens are never leaked into agent child processes.

use crate::acp::{AcpHarness, AcpHarnessOptions};
use om_core::sandbox::CommandSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Environment variable overriding the `opencode` binary path.
pub const ENV_OPENCODE_BIN: &str = "OM_OPENCODE_BIN";

/// BYOK / model-provider env vars safely forwarded when present in the server host env.
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
    /// allowlisted BYOK provider keys). Server auth tokens are intentionally excluded.
    pub fn from_env() -> Self {
        let binary = std::env::var(ENV_OPENCODE_BIN)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "opencode".into());

        let env = FORWARDED_PROVIDER_ENV
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|val| ((*key).to_string(), val)))
            .collect();

        Self {
            binary,
            extra_args: Vec::new(),
            env,
            acp_options: AcpHarnessOptions::default(),
        }
    }

    /// Builds the [`CommandSpec`] executed inside the session sandbox.
    pub fn command_spec(&self) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.binary).arg("acp");
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
    let resolved = resolve_executable(binary);
    let Some(path) = resolved else {
        return OpenCodeProbe {
            available: false,
            binary: binary.to_string(),
            binary_path: None,
            version: None,
            error: Some(format!("executable '{binary}' not found on PATH")),
        };
    };

    let mut cmd = Command::new(&path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let version = stdout
                .lines()
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            OpenCodeProbe {
                available: true,
                binary: binary.to_string(),
                binary_path: Some(path),
                version,
                error: None,
            }
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            OpenCodeProbe {
                available: false,
                binary: binary.to_string(),
                binary_path: Some(path),
                version: None,
                error: Some(if stderr.is_empty() {
                    format!("exited with status {}", out.status)
                } else {
                    stderr
                }),
            }
        }
        Ok(Err(e)) => OpenCodeProbe {
            available: false,
            binary: binary.to_string(),
            binary_path: Some(path),
            version: None,
            error: Some(e.to_string()),
        },
        Err(_) => OpenCodeProbe {
            available: false,
            binary: binary.to_string(),
            binary_path: Some(path),
            version: None,
            error: Some("timed out running --version".into()),
        },
    }
}

fn resolve_executable(bin: &str) -> Option<PathBuf> {
    let p = Path::new(bin);
    if p.components().count() > 1 {
        return is_executable_file(p).then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
