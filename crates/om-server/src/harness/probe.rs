//! Shared CLI executable probing and non-secret environment helpers for harness adapters.

use om_core::sandbox::CommandSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Non-secret host environment variables preserved when `clear_env = true` under `DirectSandbox`.
/// Under `BwrapSandbox`, the sandbox itself enforces its own clean environment allowlist.
pub const SAFE_BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "USER",
    "TERM",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

/// Result of probing an external agent CLI binary on the server host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliProbe {
    pub available: bool,
    pub binary: String,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// Applies `clear_env = true` and copies only present [`SAFE_BASE_ENV`] variables,
/// ensuring server secrets (`OM_TOKEN`, etc.) never leak into agent subprocesses.
pub fn apply_safe_base_env(mut spec: CommandSpec) -> CommandSpec {
    spec.clear_env = true;
    for key in SAFE_BASE_ENV {
        if let Ok(val) = std::env::var(key) {
            spec = spec.env(*key, val);
        }
    }
    spec
}

/// Reads a trimmed, non-empty string override from `env_key`, falling back to `default`.
pub fn env_binary_or(env_key: &str, default: &str) -> String {
    std::env::var(env_key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Resolves `bin` (either a path containing separators or a bare executable name on `PATH`).
pub fn resolve_executable(bin: &str) -> Option<PathBuf> {
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

/// Probes whether `binary` exists on the host and responds cleanly to `version_args`.
pub async fn probe_cli_version(binary: &str, version_args: &[&str]) -> CliProbe {
    let Some(path) = resolve_executable(binary) else {
        return CliProbe {
            available: false,
            binary: binary.to_string(),
            binary_path: None,
            version: None,
            error: Some(format!("executable '{binary}' not found on PATH")),
        };
    };

    let mut cmd = Command::new(&path);
    cmd.args(version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let fallback_stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let raw = if stdout.is_empty() {
                fallback_stderr
            } else {
                stdout
            };
            let version = raw
                .lines()
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            CliProbe {
                available: true,
                binary: binary.to_string(),
                binary_path: Some(path),
                version,
                error: None,
            }
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            CliProbe {
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
        Ok(Err(e)) => CliProbe {
            available: false,
            binary: binary.to_string(),
            binary_path: Some(path),
            version: None,
            error: Some(e.to_string()),
        },
        Err(_) => CliProbe {
            available: false,
            binary: binary.to_string(),
            binary_path: Some(path),
            version: None,
            error: Some("timed out running version probe".into()),
        },
    }
}
