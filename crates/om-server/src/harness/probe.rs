//! Shared CLI executable probing and non-secret environment helpers for harness adapters.

use om_core::sandbox::CommandSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Non-secret host environment variables preserved when `clear_env = true` under `DirectSandbox`.
/// Under `BwrapSandbox`, the sandbox itself enforces its own clean environment allowlist.
///
/// `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` are intentionally excluded so host
/// user credential caches are never implicitly exposed into agent subprocesses.
pub const SAFE_BASE_ENV: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "USER", "TERM",
];

/// Minimal non-secret environment variables passed to `--version` binary probes.
const PROBE_BASE_ENV: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TERM"];

/// Maximum stdout/stderr bytes captured from a version probe subprocess (64 KiB).
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;

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

/// Validates a CLI option value (`--model`, `--resume`, thread ID) so untrusted inputs or
/// compromised agent outputs cannot inject leading `-` flags or whitespace/control characters.
pub(crate) fn is_safe_cli_arg_value(val: &str) -> bool {
    !val.is_empty()
        && val.len() <= 128
        && !val.starts_with('-')
        && val.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
        })
}

/// Validates a single URL path segment (e.g. a bridge session ID) against traversal or query injection.
pub(crate) fn is_safe_path_segment(val: &str) -> bool {
    !val.is_empty()
        && val.len() <= 128
        && !val.starts_with('-')
        && val != "."
        && val != ".."
        && val
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Resolves `bin` (either a path containing separators or a bare executable name on `PATH`).
/// Relative entries in `PATH` (such as `.`) are skipped.
pub fn resolve_executable(bin: &str) -> Option<PathBuf> {
    let p = Path::new(bin);
    if p.components().count() > 1 {
        return is_executable_file(p).then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if !dir.is_absolute() {
            continue;
        }
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

async fn read_capped_and_drain<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < cap {
                    let take = (cap - out.len()).min(n);
                    out.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    out
}

/// Probes whether `binary` exists on the host and responds cleanly to `version_args`.
/// Runs with a cleared minimal environment and bounded stdout/stderr capture.
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
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in PROBE_BASE_ENV {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CliProbe {
                available: false,
                binary: binary.to_string(),
                binary_path: Some(path),
                version: None,
                error: Some(e.to_string()),
            };
        }
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let probe_fut = async {
        let out_fut = async {
            match stdout_pipe {
                Some(r) => read_capped_and_drain(r, MAX_PROBE_OUTPUT_BYTES).await,
                None => Vec::new(),
            }
        };
        let err_fut = async {
            match stderr_pipe {
                Some(r) => read_capped_and_drain(r, MAX_PROBE_OUTPUT_BYTES).await,
                None => Vec::new(),
            }
        };
        let (stdout_bytes, stderr_bytes, status) = tokio::join!(out_fut, err_fut, child.wait());
        (stdout_bytes, stderr_bytes, status)
    };

    match tokio::time::timeout(Duration::from_secs(5), probe_fut).await {
        Ok((stdout_bytes, stderr_bytes, Ok(status))) if status.success() => {
            let stdout = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
            let fallback_stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
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
        Ok((_, stderr_bytes, Ok(status))) => {
            let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
            CliProbe {
                available: false,
                binary: binary.to_string(),
                binary_path: Some(path),
                version: None,
                error: Some(if stderr.is_empty() {
                    format!("exited with status {status}")
                } else {
                    stderr
                }),
            }
        }
        Ok((_, _, Err(e))) => CliProbe {
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
