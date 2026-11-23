//! Hardened `git` invocations for server-side operations.
//!
//! The checkout and the per-worktree git admin dir are writable by the sandboxed agent,
//! so every server-side git call must assume they are hostile:
//!
//! - the environment is cleared (no inherited `GIT_*`, no global/system config);
//! - hooks and fsmonitor are disabled; diffs use `--no-ext-diff --no-textconv`;
//! - checkout operations pass an explicit `--git-dir` recorded at creation instead of
//!   following the agent-writable `.git` file in the checkout;
//! - the files git reads configuration or redirections from in the session's own
//!   repository (`config`, `hooks/`, `info/`, `commondir`, `objects/info/alternates`)
//!   are mounted read-only into the sandbox (see
//!   [`super::WorkspaceManager::sandbox_paths`]); the registered repository's `.git` is
//!   never mounted at all;
//! - output read from the session is size-capped.

use om_core::{CoreError, CoreResult};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Options every server-side git call starts with.
const HARDENING: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "protocol.allow=never",
    "-c",
    "core.quotePath=false",
    "-c",
    "advice.detachedHead=false",
];

/// A git command builder bound to a repository location.
#[derive(Debug, Clone)]
pub(crate) struct Git {
    git_bin: PathBuf,
    /// Explicit `--git-dir`, when known.
    git_dir: Option<PathBuf>,
    /// `--work-tree` (with `git_dir`) or `-C` directory.
    dir: PathBuf,
    /// Extra `-c key=value` options for this command only.
    config: Vec<String>,
}

pub(crate) fn git_err(what: &str, out: &Output) -> CoreError {
    CoreError::Workspace(format!(
        "git {what} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

impl Git {
    /// Runs in `dir`, letting git discover the repository (trusted locations only).
    pub(crate) fn at(git_bin: &Path, dir: impl Into<PathBuf>) -> Self {
        Self {
            git_bin: git_bin.to_path_buf(),
            git_dir: None,
            dir: dir.into(),
            config: Vec::new(),
        }
    }

    /// Runs against a checkout with an explicit admin dir; never reads `<dir>/.git`.
    pub(crate) fn worktree(
        git_bin: &Path,
        git_dir: impl Into<PathBuf>,
        dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            git_bin: git_bin.to_path_buf(),
            git_dir: Some(git_dir.into()),
            dir: dir.into(),
            config: Vec::new(),
        }
    }

    /// Adds `-c key=value` for this command.
    pub(crate) fn with_config(mut self, key: &str, value: &str) -> Self {
        self.config.push(format!("{key}={value}"));
        self
    }

    fn command<I, S>(&self, args: I) -> tokio::process::Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = tokio::process::Command::new(&self.git_bin);
        cmd.env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Required by `worktree add` / commits; never used for anything else.
            .env("GIT_AUTHOR_NAME", "OpenMirasim")
            .env("GIT_AUTHOR_EMAIL", "openmirasim@localhost")
            .env("GIT_COMMITTER_NAME", "OpenMirasim")
            .env("GIT_COMMITTER_EMAIL", "openmirasim@localhost")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .args(HARDENING);
        for kv in &self.config {
            cmd.arg("-c").arg(kv);
        }
        match &self.git_dir {
            Some(gd) => {
                cmd.arg("--git-dir")
                    .arg(gd)
                    .arg("--work-tree")
                    .arg(&self.dir);
                cmd.current_dir(&self.dir);
            }
            None => {
                cmd.arg("-C").arg(&self.dir);
            }
        }
        cmd.args(args);
        cmd
    }

    pub(crate) async fn output<I, S>(&self, args: I) -> CoreResult<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command(args);
        let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
            .await
            .map_err(|_| CoreError::Workspace("git timed out".into()))?
            .map_err(|e| CoreError::Workspace(format!("failed to run git: {e}")))?;
        Ok(out)
    }

    /// Runs and returns at most `max` bytes of stdout plus whether more was produced;
    /// git is stopped as soon as the cap is passed, so a huge diff cannot fill memory.
    pub(crate) async fn run_capped<I, S>(
        &self,
        what: &str,
        args: I,
        max: usize,
        ok_codes: &[i32],
    ) -> CoreResult<(Vec<u8>, bool)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        use tokio::io::AsyncReadExt;
        let mut cmd = self.command(args);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| CoreError::Workspace(format!("failed to run git: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        // Drain stderr concurrently so a chatty git never blocks on a full pipe.
        let err_task = tokio::spawn(async move {
            let mut err = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if err.len() < 64 * 1024 {
                    err.extend_from_slice(&buf[..n]);
                }
            }
            err
        });
        let work = async {
            let mut out = Vec::new();
            let mut limited = (&mut stdout).take(max as u64 + 1);
            limited
                .read_to_end(&mut out)
                .await
                .map_err(|e| CoreError::Workspace(format!("reading git output: {e}")))?;
            let truncated = out.len() > max;
            if truncated {
                out.truncate(max);
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok((out, true));
            }
            let status = child
                .wait()
                .await
                .map_err(|e| CoreError::Workspace(format!("waiting for git: {e}")))?;
            let err = err_task.await.unwrap_or_default();
            match status.code() {
                Some(c) if c == 0 || ok_codes.contains(&c) => Ok((out, false)),
                _ => Err(CoreError::Workspace(format!(
                    "git {what} failed: {}",
                    String::from_utf8_lossy(&err).trim()
                ))),
            }
        };
        tokio::time::timeout(GIT_TIMEOUT, work)
            .await
            .map_err(|_| CoreError::Workspace("git timed out".into()))?
    }

    /// Runs and requires success; returns stdout.
    pub(crate) async fn run<I, S>(&self, what: &str, args: I) -> CoreResult<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.output(args).await?;
        if !out.status.success() {
            return Err(git_err(what, &out));
        }
        Ok(out.stdout)
    }

    /// Runs and returns trimmed stdout as a string.
    pub(crate) async fn line<I, S>(&self, what: &str, args: I) -> CoreResult<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.run(what, args).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }
}

/// Splits NUL-separated git output.
pub(crate) fn split_z(out: &[u8]) -> Vec<String> {
    out.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}
