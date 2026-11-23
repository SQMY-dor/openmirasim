//! Process isolation for agents (W1-1).
//!
//! [`BwrapSandbox`] rewrites an agent [`CommandSpec`] into
//! `[systemd-run --scope -p ...] bwrap <args> -- <agent>` so that the agent:
//!
//! - sees only a minimal read-only system (`/usr`, `/lib*`, a curated `/etc` subset),
//!   the paths the harness lists as read-only (agent binary, toolchains), and its
//!   writable paths (the session worktree) — nothing else from the host, in
//!   particular no other home directories, `/root`, `/var`, or the server's database;
//! - runs in fresh user / pid / ipc / uts / cgroup (and, unless network is allowed,
//!   net) namespaces, in a new session (no `TIOCSTI` into the server's terminal), and
//!   dies with the server;
//! - gets an explicit environment allowlist instead of the server's environment, so
//!   host secrets (API keys, tokens) are never inherited by accident;
//! - optionally runs inside a transient systemd scope that enforces cgroups v2
//!   memory / CPU / pids limits.
//!
//! The wrapping itself is pure (unit-tested); [`BwrapSandbox::probe`] checks at server
//! start that unprivileged user namespaces actually work on this host.

use om_core::sandbox::{CommandSpec, ResourceLimits, Sandbox, SandboxMode, SandboxSpec};
use om_core::{CoreError, CoreResult};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// System paths bound read-only by default. Missing ones are skipped; top-level
/// symlinks (`/bin -> usr/bin` on merged-/usr systems) are recreated as symlinks.
pub const DEFAULT_SYSTEM_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc/alternatives",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/localtime",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/gitconfig",
    "/etc/mime.types",
];

/// Host environment variables passed through when set (values are copied at wrap time).
pub const DEFAULT_ENV_PASSTHROUGH: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];

/// How cgroups v2 limits are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CgroupMode {
    /// No resource limits (limits in [`SandboxSpec`] are rejected).
    #[default]
    Disabled,
    /// `systemd-run --user --scope` (needs a user systemd instance with delegation).
    SystemdUserScope,
    /// `systemd-run --scope` (server runs as root or has polkit rights).
    SystemdSystemScope,
}

/// Configuration for [`BwrapSandbox`].
#[derive(Debug, Clone)]
pub struct BwrapConfig {
    pub bwrap_path: PathBuf,
    pub systemd_run_path: PathBuf,
    pub system_paths: Vec<PathBuf>,
    pub env_passthrough: Vec<String>,
    pub cgroups: CgroupMode,
    /// Pass `--disable-userns` (bubblewrap >= 0.8) so the agent cannot create nested
    /// user namespaces. [`BwrapSandbox::probe`] turns this off if unsupported.
    pub disable_nested_userns: bool,
    /// Defaults applied when a [`SandboxSpec`] sets no limit of its own.
    pub default_limits: ResourceLimits,
    pub hostname: String,
}

impl Default for BwrapConfig {
    fn default() -> Self {
        Self {
            bwrap_path: PathBuf::from("bwrap"),
            systemd_run_path: PathBuf::from("systemd-run"),
            system_paths: DEFAULT_SYSTEM_PATHS.iter().map(PathBuf::from).collect(),
            env_passthrough: DEFAULT_ENV_PASSTHROUGH
                .iter()
                .map(|s| s.to_string())
                .collect(),
            cgroups: CgroupMode::Disabled,
            disable_nested_userns: true,
            default_limits: ResourceLimits::default(),
            hostname: "om-sandbox".into(),
        }
    }
}

/// `bubblewrap` (+ optional systemd/cgroups v2) sandbox.
#[derive(Debug, Clone, Default)]
pub struct BwrapSandbox {
    config: BwrapConfig,
}

/// HOME inside the sandbox when the spec gives no private home.
const SANDBOX_TMP_HOME: &str = "/tmp/home";

impl BwrapSandbox {
    pub fn new(config: BwrapConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &BwrapConfig {
        &self.config
    }

    /// Checks that bubblewrap can create its namespaces on this host, falling back to no
    /// `--disable-userns` on old bubblewrap. Call once at server start; returns an
    /// actionable error (e.g. for Ubuntu 24.04's AppArmor userns restriction).
    pub async fn probe(mut self) -> CoreResult<Self> {
        let attempt = |sb: &Self| {
            let spec = SandboxSpec {
                network: false,
                ..SandboxSpec::default()
            };
            sb.wrap(&spec, CommandSpec::new("true"))
        };
        let run = |cmd: CommandSpec| async move {
            let mut c = tokio::process::Command::new(&cmd.program);
            c.args(&cmd.args).env_clear().envs(&cmd.env);
            match c.output().await {
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
                Err(e) => Err(e.to_string()),
            }
        };
        let first = run(attempt(&self)?).await;
        let err = match first {
            Ok(()) => return Ok(self),
            Err(e) => e,
        };
        if self.config.disable_nested_userns && err.contains("disable-userns") {
            self.config.disable_nested_userns = false;
            if run(attempt(&self)?).await.is_ok() {
                tracing::warn!(
                    "bubblewrap lacks --disable-userns; nested user namespaces stay possible"
                );
                return Ok(self);
            }
        }
        Err(CoreError::Sandbox(probe_hint(&err)))
    }

    fn system_binds(&self, args: &mut Vec<String>) {
        for path in &self.config.system_paths {
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                continue;
            };
            let p = path.to_string_lossy().into_owned();
            // Recreate top-level symlinks (merged /usr); bind everything else.
            if meta.file_type().is_symlink() && path.parent() == Some(Path::new("/")) {
                if let Ok(target) = std::fs::read_link(path) {
                    args.extend(["--symlink".into(), target.to_string_lossy().into_owned(), p]);
                    continue;
                }
            }
            args.extend(["--ro-bind-try".into(), p.clone(), p]);
        }
    }

    /// Resolves the program to an absolute path on the host so it can be exposed.
    fn resolve_program(&self, program: &str, env_path: Option<&str>) -> CoreResult<PathBuf> {
        let p = Path::new(program);
        if p.is_absolute() {
            return Ok(p.to_path_buf());
        }
        if program.contains('/') {
            return Err(CoreError::InvalidInput(format!(
                "sandboxed program must be absolute or a bare name on PATH: {program}"
            )));
        }
        let path = env_path
            .map(str::to_owned)
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|c| c.is_file())
            .ok_or_else(|| CoreError::Sandbox(format!("`{program}` not found on PATH")))
    }

    fn is_covered(&self, path: &Path, extra: &[PathBuf]) -> bool {
        self.config
            .system_paths
            .iter()
            .chain(extra)
            .any(|root| path.starts_with(root))
    }

    fn cgroup_prefix(&self, limits: &ResourceLimits) -> CoreResult<Vec<String>> {
        let limits = ResourceLimits {
            memory_max_bytes: limits
                .memory_max_bytes
                .or(self.config.default_limits.memory_max_bytes),
            cpu_quota_percent: limits
                .cpu_quota_percent
                .or(self.config.default_limits.cpu_quota_percent),
            pids_max: limits.pids_max.or(self.config.default_limits.pids_max),
        };
        if limits == ResourceLimits::default() {
            return Ok(Vec::new());
        }
        let user = match self.config.cgroups {
            CgroupMode::Disabled => {
                return Err(CoreError::Sandbox(
                    "resource limits requested but cgroups are disabled for this sandbox".into(),
                ));
            }
            CgroupMode::SystemdUserScope => true,
            CgroupMode::SystemdSystemScope => false,
        };
        let mut a = vec![self.config.systemd_run_path.to_string_lossy().into_owned()];
        if user {
            a.push("--user".into());
        }
        a.extend(["--scope".into(), "--quiet".into(), "--collect".into()]);
        if let Some(m) = limits.memory_max_bytes {
            a.extend([
                "-p".into(),
                format!("MemoryMax={m}"),
                "-p".into(),
                "MemorySwapMax=0".into(),
            ]);
        }
        if let Some(c) = limits.cpu_quota_percent {
            a.extend(["-p".into(), format!("CPUQuota={c}%")]);
        }
        if let Some(p) = limits.pids_max {
            a.extend(["-p".into(), format!("TasksMax={p}")]);
        }
        a.push("--".into());
        Ok(a)
    }
}

fn absolute(p: &Path, what: &str) -> CoreResult<String> {
    if !p.is_absolute() {
        return Err(CoreError::InvalidInput(format!(
            "{what} must be absolute: {}",
            p.display()
        )));
    }
    Ok(p.to_string_lossy().into_owned())
}

fn probe_hint(stderr: &str) -> String {
    let mut msg = format!("bubblewrap cannot create a sandbox on this host: {stderr}");
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
    if restricted {
        msg.push_str(
            ". AppArmor restricts unprivileged user namespaces (Ubuntu 23.10+): install an \
             AppArmor profile granting `userns` to bwrap, or set \
             `kernel.apparmor_restrict_unprivileged_userns=0`",
        );
    } else if stderr.contains("No such file") || stderr.contains("not found") {
        msg.push_str(". Install the `bubblewrap` package");
    } else {
        msg.push_str(". Check that unprivileged user namespaces are enabled (`user.max_user_namespaces` > 0)");
    }
    msg
}

impl Sandbox for BwrapSandbox {
    fn mode(&self) -> SandboxMode {
        SandboxMode::Bwrap
    }

    fn wrap(&self, spec: &SandboxSpec, cmd: CommandSpec) -> CoreResult<CommandSpec> {
        let mut args: Vec<String> = vec![
            "--die-with-parent".into(),
            "--new-session".into(),
            "--unshare-all".into(),
            "--unshare-user".into(),
        ];
        if self.config.disable_nested_userns {
            args.push("--disable-userns".into());
        }
        if spec.network {
            args.push("--share-net".into());
        }
        args.extend(["--hostname".into(), self.config.hostname.clone()]);
        args.extend([
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
        ]);
        args.extend([
            "--tmpfs".into(),
            "/tmp".into(),
            "--tmpfs".into(),
            "/run".into(),
        ]);
        self.system_binds(&mut args);

        // Environment: explicit allowlist, never the server's full environment.
        let mut env = std::collections::BTreeMap::new();
        for key in &self.config.env_passthrough {
            if let Ok(v) = std::env::var(key) {
                env.insert(key.clone(), v);
            }
        }
        env.extend(cmd.env.clone());

        let mut exposed: Vec<PathBuf> = Vec::new();
        let mut seen = BTreeSet::new();
        for p in &spec.writable_paths {
            let s = absolute(p, "writable path")?;
            if !p.exists() {
                return Err(CoreError::Sandbox(format!(
                    "writable path does not exist: {s}"
                )));
            }
            args.extend(["--bind".into(), s.clone(), s]);
            exposed.push(p.clone());
        }
        let home = match &spec.home {
            Some(h) => {
                let s = absolute(h, "home")?;
                if !h.exists() {
                    return Err(CoreError::Sandbox(format!("home does not exist: {s}")));
                }
                args.extend(["--bind".into(), s.clone(), s.clone()]);
                exposed.push(h.clone());
                s
            }
            None => {
                args.extend(["--dir".into(), SANDBOX_TMP_HOME.into()]);
                SANDBOX_TMP_HOME.into()
            }
        };
        // Read-only paths go last so they can overlay parts of writable binds
        // (e.g. `.git/config` inside a writable `.git`).
        for p in &spec.readonly_paths {
            let s = absolute(p, "read-only path")?;
            if seen.insert(s.clone()) {
                args.extend(["--ro-bind-try".into(), s.clone(), s]);
                exposed.push(p.clone());
            }
        }
        env.insert("HOME".into(), home);
        env.insert("TMPDIR".into(), "/tmp".into());

        // Make sure the agent binary itself is visible.
        let program = self.resolve_program(&cmd.program, env.get("PATH").map(String::as_str))?;
        if !self.is_covered(&program, &exposed) {
            let dir = program
                .parent()
                .ok_or_else(|| CoreError::Sandbox("program has no parent directory".into()))?;
            let d = dir.to_string_lossy().into_owned();
            args.extend(["--ro-bind".into(), d.clone(), d]);
        }

        if let Some(cwd) = &cmd.cwd {
            let s = absolute(cwd, "cwd")?;
            if !self.is_covered(cwd, &exposed) {
                return Err(CoreError::Sandbox(format!(
                    "cwd {s} is not inside any path exposed to the sandbox"
                )));
            }
            args.extend(["--chdir".into(), s]);
        }
        args.push("--".into());
        args.push(program.to_string_lossy().into_owned());
        args.extend(cmd.args);

        let mut full = self.cgroup_prefix(&spec.limits)?;
        let bwrap = self.config.bwrap_path.to_string_lossy().into_owned();
        let program = if full.is_empty() {
            bwrap
        } else {
            let p = full.remove(0);
            full.push(bwrap);
            p
        };
        full.extend(args);
        Ok(CommandSpec {
            program,
            args: full,
            // The host-side cwd only matters for spawning; the agent uses --chdir.
            cwd: None,
            env,
            clear_env: true,
        })
    }
}

/// Adds per-session paths (worktree, git dir overlays, private HOME) to every spec
/// before delegating to the real sandbox.
pub struct ScopedSandbox {
    inner: std::sync::Arc<dyn Sandbox>,
    writable: Vec<PathBuf>,
    readonly: Vec<PathBuf>,
    home: Option<PathBuf>,
}

impl ScopedSandbox {
    pub fn new(
        inner: std::sync::Arc<dyn Sandbox>,
        writable: Vec<PathBuf>,
        readonly: Vec<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            inner,
            writable,
            readonly,
            home,
        }
    }
}

impl Sandbox for ScopedSandbox {
    fn mode(&self) -> SandboxMode {
        self.inner.mode()
    }

    fn wrap(&self, spec: &SandboxSpec, cmd: CommandSpec) -> CoreResult<CommandSpec> {
        let mut spec = spec.clone();
        for p in &self.writable {
            if !spec.writable_paths.contains(p) {
                spec.writable_paths.push(p.clone());
            }
        }
        spec.readonly_paths.extend(self.readonly.iter().cloned());
        if spec.home.is_none() {
            spec.home = self.home.clone();
        }
        self.inner.wrap(&spec, cmd)
    }
}

#[cfg(test)]
mod tests;
