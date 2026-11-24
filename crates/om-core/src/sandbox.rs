//! The `Sandbox` abstraction that wraps agent processes (R4).
//!
//! A sandbox does not spawn anything itself: it rewrites a [`CommandSpec`] so that the
//! harness spawns the agent inside the isolation boundary (for example by prefixing
//! `systemd-run ... bwrap ...`). This keeps harnesses independent of the isolation
//! mechanism and makes the wrapping logic unit-testable.

use crate::error::CoreResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use ts_rs::TS;

/// Execution isolation mode for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SandboxMode {
    /// No isolation. Phase 0 single-developer mode only: the server must refuse it
    /// unless explicitly enabled and bound to loopback.
    #[default]
    Direct,
    /// `bubblewrap` + `systemd-run`/cgroups v2 (Phase 1).
    Bwrap,
}

/// A process to spawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Environment variables to set.
    pub env: BTreeMap<String, String>,
    /// Start from an empty environment instead of inheriting the server's.
    pub clear_env: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

/// Resource limits applied through cgroups v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceLimits {
    pub memory_max_bytes: Option<u64>,
    /// CPU quota in percent of one core (e.g. 200 = two cores).
    pub cpu_quota_percent: Option<u32>,
    pub pids_max: Option<u32>,
}

/// What the sandboxed process may see and use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxSpec {
    /// Paths bind-mounted read-write (typically just the session's worktree).
    pub writable_paths: Vec<PathBuf>,
    /// Paths bind-mounted read-only (toolchains, the agent binary, ...).
    pub readonly_paths: Vec<PathBuf>,
    /// Private home directory for the agent (per member), when isolated.
    pub home: Option<PathBuf>,
    /// Whether outbound network is allowed.
    pub network: bool,
    /// Keep the child's controlling session (e.g. a dedicated fresh PTY slave) instead of
    /// detaching with `--new-session`. Must only be enabled when spawning on an isolated PTY.
    pub keep_session: bool,
    pub limits: ResourceLimits,
}

/// Wraps agent commands into an isolation boundary.
pub trait Sandbox: Send + Sync {
    fn mode(&self) -> SandboxMode;
    /// Returns the command to actually spawn.
    fn wrap(&self, spec: &SandboxSpec, cmd: CommandSpec) -> CoreResult<CommandSpec>;
}

/// The Phase 0 no-op sandbox: runs the agent directly on the host.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectSandbox;

impl Sandbox for DirectSandbox {
    fn mode(&self) -> SandboxMode {
        SandboxMode::Direct
    }

    fn wrap(&self, _spec: &SandboxSpec, cmd: CommandSpec) -> CoreResult<CommandSpec> {
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_sandbox_is_identity() {
        let cmd = CommandSpec::new("opencode").arg("acp").cwd("/tmp");
        let wrapped = DirectSandbox
            .wrap(&SandboxSpec::default(), cmd.clone())
            .unwrap();
        assert_eq!(wrapped, cmd);
        assert_eq!(DirectSandbox.mode(), SandboxMode::Direct);
    }
}
