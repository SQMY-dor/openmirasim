//! Isolated external Antigravity GUI bridge (`om-gui-bridge`, W2-5).
//!
//! Runs out-of-process from `om-server` so Linux Xvfb/GUI crashes never affect core
//! orchestration. Enforces:
//! - **1 member = 1 isolated profile directory** (`0700` under `profile_root/<member_id>`)
//! - **Per-host concurrency cap** (`max_sessions`, rejecting excess with HTTP `429`)
//! - **Idle session reaper** reclaiming Xvfb/GUI/noVNC processes and display slots
//! - **Human takeover via noVNC / remote screen**: never attempts to bypass login or
//!   anti-bot/CAPTCHA gates; surfaces an isolated per-session takeover URL instead.

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// Maximum transcript lines retained per GUI session.
const MAX_TRANSCRIPT_LINES: usize = 200;

/// Maximum bytes accepted for a single manual takeover input.
const MAX_MANUAL_INPUT_BYTES: usize = 8 * 1024;

/// Maximum duration for a single `xdotool` injection command.
const XDOTOOL_TIMEOUT: Duration = Duration::from_secs(15);

/// Minimal non-secret environment variables forwarded to X11/GUI helper children.
const SAFE_CHILD_ENV: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];

/// Execution backend for GUI sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    /// Automatically uses `x11` when `Xvfb` and `gui_binary` exist on host, else `simulated`.
    Auto,
    /// Real Linux `Xvfb` + Antigravity GUI process + `x11vnc`/`websockify` + `xdotool`.
    X11,
    /// Deterministic headless driver with built-in HTML5 remote-screen takeover view.
    Simulated,
}

impl DriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::X11 => "x11",
            Self::Simulated => "simulated",
        }
    }
}

/// Configuration for [`GuiBridge`].
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Root directory holding per-member isolated profile directories (`0700`).
    pub profile_root: PathBuf,
    /// Maximum concurrent GUI sessions allowed on this host.
    pub max_sessions: usize,
    /// Idle duration after which a GUI session is automatically torn down.
    pub idle_timeout: Duration,
    /// How often the background idle reaper runs.
    pub reap_interval: Duration,
    /// Driver selection (`auto`, `x11`, or `simulated`).
    pub driver: DriverKind,
    /// Antigravity GUI executable (used when `x11` driver is active).
    pub gui_binary: String,
    /// Shared Bearer token protecting `/v1/*` bridge control endpoints.
    /// Required for any non-loopback bind and for `x11` driver deployments.
    pub bridge_token: Option<String>,
    /// Public base URL advertised for takeover links (defaults to `http://<bind_addr>`).
    pub public_base_url: Option<String>,
}

impl BridgeConfig {
    pub fn new(profile_root: impl Into<PathBuf>) -> Self {
        Self {
            profile_root: profile_root.into(),
            max_sessions: 4,
            idle_timeout: Duration::from_secs(30 * 60),
            reap_interval: Duration::from_secs(15),
            driver: DriverKind::Auto,
            gui_binary: std::env::var("OM_ANTIGRAVITY_GUI_BIN")
                .unwrap_or_else(|_| "antigravity".into()),
            bridge_token: None,
            public_base_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeHealth {
    pub status: String,
    pub driver: String,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub xvfb_available: bool,
    pub gui_binary_available: bool,
    pub novnc_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateGuiSessionRequest {
    pub om_session_id: String,
    pub member_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiSessionInfo {
    pub bridge_session_id: String,
    pub om_session_id: String,
    pub member_id: String,
    pub profile_dir: String,
    pub display: String,
    pub driver: String,
    pub takeover_url: String,
    pub status: String,
    pub last_active_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListGuiSessionsResponse {
    pub sessions: Vec<GuiSessionInfo>,
    pub max_sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiPromptRequest {
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiPromptResponse {
    pub reply: String,
    pub takeover_required: bool,
    pub takeover_url: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BridgeError {
    code: String,
    message: String,
}

fn json_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(BridgeError {
            code: code.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn random_hex(bytes_len: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes_len];
    getrandom::fill(&mut buf).map_err(|e| format!("CSPRNG entropy failed: {e}"))?;
    let mut out = String::with_capacity(bytes_len * 2);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn is_executable_file(path: &FsPath) -> bool {
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

fn binary_on_path(bin: &str) -> bool {
    let p = FsPath::new(bin);
    if p.components().count() > 1 {
        return is_executable_file(p);
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.is_absolute() && is_executable_file(&dir.join(bin)))
}

fn apply_minimal_child_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in SAFE_CHILD_ENV {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    bool::from(ha.as_slice().ct_eq(hb.as_slice()))
}

/// Validates and sanitizes a member id so it can safely name an isolated profile directory.
pub fn validate_member_id(raw: &str) -> Result<&str, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err("member_id must be 1..=64 characters".into());
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("member_id may only contain ASCII letters, digits, '-' and '_'".into());
    }
    Ok(trimmed)
}

fn create_private_dir(path: &FsPath) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!("refusing to use symlink path: {}", path.display()));
        }
        if !meta.is_dir() {
            return Err(format!("expected directory at {}", path.display()));
        }
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| format!("failed to create directory {}: {e}", path.display()))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(path)
                .map_err(|e| format!("failed to create directory {}: {e}", path.display()))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("failed to chmod {} to 0700: {e}", path.display()))?;
    }
    Ok(())
}

fn ensure_isolated_profile(profile_root: &FsPath, member_id: &str) -> Result<PathBuf, String> {
    let safe_id = validate_member_id(member_id)?;
    create_private_dir(profile_root)?;
    let dir = profile_root.join(safe_id);
    create_private_dir(&dir)?;
    for sub in [".config", ".local/share", ".cache", ".run"] {
        create_private_dir(&dir.join(sub))?;
    }
    Ok(dir)
}

/// Writes a `0600` file exclusively (`create_new`), failing if a symlink or preexisting file exists.
fn write_secret_file(path: &FsPath, bytes: &[u8]) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("failed to create secret file {}: {e}", path.display()))?;
    use std::io::Write;
    f.write_all(bytes)
        .map_err(|e| format!("failed to write secret file {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("failed to sync secret file {}: {e}", path.display()))?;
    Ok(())
}

/// Encodes a binary Xauthority file containing MIT-MAGIC-COOKIE-1 for `display_num`
/// (`FamilyWild = 0xffff` so both host `Xvfb` and `bwrap` namespaces authenticate cleanly).
fn write_xauth_file(path: &FsPath, display_num: u16) -> Result<(), String> {
    let mut cookie = [0u8; 16];
    getrandom::fill(&mut cookie).map_err(|e| format!("xauth entropy failed: {e}"))?;
    let disp = display_num.to_string();
    let name = b"MIT-MAGIC-COOKIE-1";

    let mut data = Vec::with_capacity(64);
    // FamilyWild (0xffff), empty address
    data.extend_from_slice(&0xffffu16.to_be_bytes());
    data.extend_from_slice(&0u16.to_be_bytes());
    data.extend_from_slice(&(disp.len() as u16).to_be_bytes());
    data.extend_from_slice(disp.as_bytes());
    data.extend_from_slice(&(name.len() as u16).to_be_bytes());
    data.extend_from_slice(name);
    data.extend_from_slice(&(cookie.len() as u16).to_be_bytes());
    data.extend_from_slice(&cookie);

    write_secret_file(path, &data)
}

struct X11Children {
    xvfb: Option<Child>,
    gui: Option<Child>,
    vnc: Option<Child>,
    websockify: Option<Child>,
    xauth_path: Option<PathBuf>,
    vnc_passwd_path: Option<PathBuf>,
}

impl X11Children {
    fn kill_all_now(&mut self) {
        for child in [
            &mut self.websockify,
            &mut self.vnc,
            &mut self.gui,
            &mut self.xvfb,
        ] {
            if let Some(c) = child.as_mut() {
                let _ = c.start_kill();
            }
        }
    }

    fn cleanup_secret_files(&mut self) {
        if let Some(p) = self.xauth_path.take() {
            let _ = std::fs::remove_file(p);
        }
        if let Some(p) = self.vnc_passwd_path.take() {
            let _ = std::fs::remove_file(p);
        }
    }

    async fn terminate_and_reap(mut self) {
        #[cfg(unix)]
        for child in [
            self.websockify.as_ref(),
            self.vnc.as_ref(),
            self.gui.as_ref(),
            self.xvfb.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        for slot in [
            &mut self.websockify,
            &mut self.vnc,
            &mut self.gui,
            &mut self.xvfb,
        ] {
            if let Some(mut c) = slot.take() {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() || tokio::time::timeout(remaining, c.wait()).await.is_err() {
                    let _ = c.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(1), c.wait()).await;
                }
            }
        }
        self.cleanup_secret_files();
    }
}

impl Drop for X11Children {
    fn drop(&mut self) {
        self.kill_all_now();
        self.cleanup_secret_files();
    }
}

#[derive(Debug, Clone)]
struct TranscriptLine {
    role: String,
    text: String,
    ts: i64,
}

struct ActiveSession {
    bridge_session_id: String,
    om_session_id: String,
    member_id: String,
    profile_dir: PathBuf,
    display_num: u16,
    driver: &'static str,
    takeover_token: String,
    takeover_url: String,
    novnc_port: Option<u16>,
    xauth_path: Option<PathBuf>,
    status: String,
    last_active: Instant,
    last_active_ms: i64,
    transcript: VecDeque<TranscriptLine>,
    prompt_lock: Arc<tokio::sync::Mutex<()>>,
    children: Option<X11Children>,
}

impl ActiveSession {
    fn push_transcript(&mut self, line: TranscriptLine) {
        while self.transcript.len() >= MAX_TRANSCRIPT_LINES {
            self.transcript.pop_front();
        }
        self.transcript.push_back(line);
    }

    fn to_info(&self) -> GuiSessionInfo {
        GuiSessionInfo {
            bridge_session_id: self.bridge_session_id.clone(),
            om_session_id: self.om_session_id.clone(),
            member_id: self.member_id.clone(),
            profile_dir: self.profile_dir.to_string_lossy().into_owned(),
            display: format!(":{}", self.display_num),
            driver: self.driver.to_string(),
            takeover_url: self.takeover_url.clone(),
            status: self.status.clone(),
            last_active_ms: self.last_active_ms,
        }
    }
}

struct ManagerInner {
    config: BridgeConfig,
    resolved_driver: &'static str,
    xvfb_available: bool,
    gui_binary_available: bool,
    novnc_available: bool,
    base_url: String,
    sessions: HashMap<String, ActiveSession>,
    reserved_slots: usize,
    used_displays: HashSet<u16>,
}

/// Shared handle to the running GUI bridge server.
#[derive(Clone)]
pub struct GuiBridge {
    inner: Arc<Mutex<ManagerInner>>,
    addr: SocketAddr,
}

impl GuiBridge {
    /// Starts the GUI bridge HTTP server and background idle reaper.
    pub async fn start(
        config: BridgeConfig,
        bind: SocketAddr,
    ) -> Result<(Self, JoinHandle<()>), String> {
        if config.max_sessions == 0 {
            return Err("max_sessions must be at least 1".into());
        }
        let has_token = config
            .bridge_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
        if !bind.ip().is_loopback() && !has_token {
            return Err(
                "refusing to bind om-gui-bridge on non-loopback address without --token / OM_GUI_BRIDGE_TOKEN"
                    .into(),
            );
        }

        create_private_dir(&config.profile_root)?;

        let xvfb_available = binary_on_path("Xvfb");
        let gui_binary_available = binary_on_path(&config.gui_binary);
        let novnc_available = binary_on_path("x11vnc") && binary_on_path("websockify");

        let resolved_driver = match config.driver {
            DriverKind::Simulated => "simulated",
            DriverKind::X11 => {
                if !xvfb_available {
                    return Err("driver=x11 requested but `Xvfb` is not installed on PATH".into());
                }
                "x11"
            }
            DriverKind::Auto => {
                if xvfb_available && gui_binary_available {
                    "x11"
                } else {
                    "simulated"
                }
            }
        };

        if resolved_driver == "x11" && !has_token {
            return Err(
                "refusing to run X11 GUI driver without --token / OM_GUI_BRIDGE_TOKEN".into(),
            );
        }
        if !has_token {
            tracing::warn!(
                addr = %bind,
                driver = resolved_driver,
                "om-gui-bridge running without Bearer token on loopback simulated driver"
            );
        }

        let listener = TcpListener::bind(bind)
            .await
            .map_err(|e| format!("failed to bind {bind}: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("failed to read local_addr: {e}"))?;
        let base_url = config
            .public_base_url
            .clone()
            .unwrap_or_else(|| format!("http://{addr}"))
            .trim_end_matches('/')
            .to_string();

        let inner = Arc::new(Mutex::new(ManagerInner {
            config: config.clone(),
            resolved_driver,
            xvfb_available,
            gui_binary_available,
            novnc_available,
            base_url,
            sessions: HashMap::new(),
            reserved_slots: 0,
            used_displays: HashSet::new(),
        }));

        let bridge = Self {
            inner: inner.clone(),
            addr,
        };

        // Background idle reaper
        let reaper_inner = inner.clone();
        let reap_interval = config.reap_interval.max(Duration::from_millis(50));
        let reaper_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(reap_interval);
            loop {
                ticker.tick().await;
                let reaped = reap_idle_sessions(&reaper_inner).await;
                if reaped > 0 {
                    tracing::info!(reaped, "reaped idle GUI bridge sessions");
                }
            }
        });

        let router = build_bridge_router(bridge.clone());
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
            reaper_handle.abort();
        });

        Ok((bridge, server_handle))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .base_url
            .clone()
    }

    pub fn active_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sessions
            .len()
    }

    /// Gracefully terminates and reaps every active GUI session (e.g. on SIGTERM/SIGINT).
    pub async fn shutdown_all(&self) {
        let removed: Vec<(u16, Option<X11Children>)> = {
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            guard
                .sessions
                .drain()
                .map(|(_, s)| (s.display_num, s.children))
                .collect()
        };
        for (display_num, children) in removed {
            if let Some(c) = children {
                c.terminate_and_reap().await;
            }
            self.inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .used_displays
                .remove(&display_num);
        }
    }
}

async fn reap_idle_sessions(inner: &Arc<Mutex<ManagerInner>>) -> usize {
    let removed: Vec<(u16, Option<X11Children>)> = {
        let mut guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
        let timeout = guard.config.idle_timeout;
        let now = Instant::now();
        let stale: Vec<String> = guard
            .sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_active) >= timeout)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::with_capacity(stale.len());
        for id in stale {
            if let Some(sess) = guard.sessions.remove(&id) {
                out.push((sess.display_num, sess.children));
            }
        }
        out
    };
    let count = removed.len();
    for (display_num, children) in removed {
        if let Some(c) = children {
            c.terminate_and_reap().await;
        }
        inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .used_displays
            .remove(&display_num);
    }
    count
}

fn allocate_display(used: &mut HashSet<u16>) -> Option<u16> {
    (100..200).find(|&d| used.insert(d))
}

async fn spawn_x11_session(
    bridge_session_id: &str,
    display_num: u16,
    profile_dir: &FsPath,
    gui_binary: &str,
    cwd: Option<&str>,
) -> Result<(X11Children, Option<u16>, PathBuf), String> {
    let display = format!(":{display_num}");
    let run_dir = profile_dir.join(".run");
    let xauth_path = run_dir.join(format!("xauth-{bridge_session_id}"));
    write_xauth_file(&xauth_path, display_num)?;

    let mut xvfb_cmd = Command::new("Xvfb");
    apply_minimal_child_env(&mut xvfb_cmd);
    xvfb_cmd
        .args([
            &display,
            "-screen",
            "0",
            "1440x900x24",
            "-nolisten",
            "tcp",
            "-auth",
        ])
        .arg(&xauth_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut xvfb = xvfb_cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_file(&xauth_path);
        format!("failed to spawn Xvfb: {e}")
    })?;

    // Wait up to 3s for the Xvfb Unix socket to become ready before launching GUI / x11vnc.
    let x11_sock = PathBuf::from(format!("/tmp/.X11-unix/X{display_num}"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if x11_sock.exists() {
            break;
        }
        if let Ok(Some(status)) = xvfb.try_wait() {
            let _ = std::fs::remove_file(&xauth_path);
            return Err(format!("Xvfb exited prematurely with status {status}"));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = xvfb.start_kill();
            let _ = xvfb.wait().await;
            let _ = std::fs::remove_file(&xauth_path);
            return Err(format!(
                "timed out waiting for Xvfb socket {}",
                x11_sock.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let valid_cwd = cwd
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir());

    let mut gui_cmd = if binary_on_path("bwrap") {
        let mut cmd = Command::new("bwrap");
        apply_minimal_child_env(&mut cmd);
        cmd.args([
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--bind",
        ])
        .arg(profile_dir)
        .arg(profile_dir)
        .arg("--ro-bind")
        .arg(&x11_sock)
        .arg(&x11_sock);
        if let Some(ref dir) = valid_cwd {
            cmd.arg("--bind").arg(dir).arg(dir);
            cmd.arg("--chdir").arg(dir);
        } else {
            cmd.arg("--chdir").arg(profile_dir);
        }
        cmd.args(["--remount-ro", "/", "--", gui_binary]);
        cmd
    } else {
        tracing::warn!(
            gui_binary,
            "bubblewrap not found on PATH; launching GUI binary without filesystem namespace isolation"
        );
        let mut cmd = Command::new(gui_binary);
        apply_minimal_child_env(&mut cmd);
        if let Some(ref dir) = valid_cwd {
            cmd.current_dir(dir);
        } else {
            cmd.current_dir(profile_dir);
        }
        cmd
    };

    gui_cmd
        .env("DISPLAY", &display)
        .env("XAUTHORITY", &xauth_path)
        .env("HOME", profile_dir)
        .env("XDG_CONFIG_HOME", profile_dir.join(".config"))
        .env("XDG_DATA_HOME", profile_dir.join(".local/share"))
        .env("XDG_CACHE_HOME", profile_dir.join(".cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let gui = gui_cmd.spawn().map_err(|e| {
        let _ = xvfb.start_kill();
        let _ = std::fs::remove_file(&xauth_path);
        format!("failed to spawn GUI binary `{gui_binary}`: {e}")
    })?;

    let mut vnc = None;
    let mut websockify = None;
    let mut novnc_port = None;
    let mut vnc_passwd_path = None;
    if binary_on_path("x11vnc") && binary_on_path("websockify") {
        let rfb_port = 5900 + display_num;
        let ws_port = 6080 + (display_num - 100);
        let pw_file = run_dir.join(format!("vncpw-{bridge_session_id}"));
        if let Ok(pw_secret) = random_hex(12)
            && write_secret_file(&pw_file, pw_secret.as_bytes()).is_ok()
        {
            let mut vnc_cmd = Command::new("x11vnc");
            apply_minimal_child_env(&mut vnc_cmd);
            vnc_cmd
                .args(["-display", &display, "-auth"])
                .arg(&xauth_path)
                .args([
                    "-localhost",
                    "-rfbport",
                    &rfb_port.to_string(),
                    "-passwdfile",
                ])
                .arg(&pw_file)
                .args(["-forever", "-shared"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if let Ok(v) = vnc_cmd.spawn() {
                vnc = Some(v);
                vnc_passwd_path = Some(pw_file);
                let mut ws_cmd = Command::new("websockify");
                apply_minimal_child_env(&mut ws_cmd);
                ws_cmd
                    .args([
                        &format!("127.0.0.1:{ws_port}"),
                        &format!("127.0.0.1:{rfb_port}"),
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                if let Ok(w) = ws_cmd.spawn() {
                    websockify = Some(w);
                    novnc_port = Some(ws_port);
                }
            } else {
                let _ = std::fs::remove_file(&pw_file);
            }
        }
    }

    Ok((
        X11Children {
            xvfb: Some(xvfb),
            gui: Some(gui),
            vnc,
            websockify,
            xauth_path: Some(xauth_path.clone()),
            vnc_passwd_path,
        },
        novnc_port,
        xauth_path,
    ))
}

fn build_bridge_router(bridge: GuiBridge) -> Router {
    let v1 = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/prompt", post(prompt_session))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .layer(middleware::from_fn_with_state(
            bridge.clone(),
            require_bridge_auth,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/takeover/{id}", get(takeover_page))
        .route("/takeover/{id}/manual", post(takeover_manual_input))
        .nest("/v1", v1)
        .with_state(bridge)
}

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = raw.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
        Some(token.trim())
    } else {
        None
    }
}

async fn require_bridge_auth(
    State(bridge): State<GuiBridge>,
    req: Request,
    next: Next,
) -> Response {
    let expected = bridge
        .inner
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .config
        .bridge_token
        .clone();
    if let Some(expected_tok) = expected.filter(|t| !t.is_empty()) {
        let ok = extract_bearer(req.headers())
            .map(|cand| constant_time_eq(cand, &expected_tok))
            .unwrap_or(false);
        if !ok {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid or missing bridge Bearer token",
            );
        }
    }
    next.run(req).await
}

async fn healthz(State(bridge): State<GuiBridge>) -> Json<BridgeHealth> {
    let guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    Json(BridgeHealth {
        status: "ok".into(),
        driver: guard.resolved_driver.into(),
        active_sessions: guard.sessions.len(),
        max_sessions: guard.config.max_sessions,
        xvfb_available: guard.xvfb_available,
        gui_binary_available: guard.gui_binary_available,
        novnc_available: guard.novnc_available,
    })
}

async fn list_sessions(State(bridge): State<GuiBridge>) -> Json<ListGuiSessionsResponse> {
    let guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    let mut sessions: Vec<_> = guard
        .sessions
        .values()
        .map(ActiveSession::to_info)
        .collect();
    sessions.sort_by(|a, b| a.bridge_session_id.cmp(&b.bridge_session_id));
    Json(ListGuiSessionsResponse {
        sessions,
        max_sessions: guard.config.max_sessions,
    })
}

async fn get_session(State(bridge): State<GuiBridge>, Path(id): Path<String>) -> Response {
    let guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.sessions.get(&id) {
        Some(s) => Json(s.to_info()).into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("gui session {id} not found"),
        ),
    }
}

struct SlotReservation {
    inner: Arc<Mutex<ManagerInner>>,
    display_num: u16,
    committed: bool,
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        if !self.committed {
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            guard.reserved_slots = guard.reserved_slots.saturating_sub(1);
            guard.used_displays.remove(&self.display_num);
        }
    }
}

async fn create_session(
    State(bridge): State<GuiBridge>,
    Json(body): Json<CreateGuiSessionRequest>,
) -> Response {
    if let Err(msg) = validate_member_id(&body.member_id) {
        return json_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
    }

    let (profile_root, resolved_driver, gui_binary, base_url, display_num) = {
        let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let in_use = guard.sessions.len() + guard.reserved_slots;
        if in_use >= guard.config.max_sessions {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "limit_exceeded",
                format!(
                    "GUI bridge concurrency cap reached ({in_use}/{})",
                    guard.config.max_sessions
                ),
            );
        }
        let Some(display_num) = allocate_display(&mut guard.used_displays) else {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "limit_exceeded",
                "no free Xvfb display slots available",
            );
        };
        guard.reserved_slots += 1;
        (
            guard.config.profile_root.clone(),
            guard.resolved_driver,
            guard.config.gui_binary.clone(),
            guard.base_url.clone(),
            display_num,
        )
    };

    let mut reservation = SlotReservation {
        inner: bridge.inner.clone(),
        display_num,
        committed: false,
    };

    let member_id_for_fs = body.member_id.clone();
    let profile_dir = match tokio::task::spawn_blocking(move || {
        ensure_isolated_profile(&profile_root, &member_id_for_fs)
    })
    .await
    {
        Ok(Ok(dir)) => dir,
        Ok(Err(msg)) => return json_error(StatusCode::BAD_REQUEST, "invalid_input", msg),
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("profile setup task failed: {e}"),
            );
        }
    };

    let bridge_session_id = format!("gui-{}", uuid::Uuid::now_v7());
    let takeover_token = match random_hex(32) {
        Ok(t) => t,
        Err(msg) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg),
    };

    let (children, novnc_port, xauth_path) = if resolved_driver == "x11" {
        match spawn_x11_session(
            &bridge_session_id,
            display_num,
            &profile_dir,
            &gui_binary,
            body.cwd.as_deref(),
        )
        .await
        {
            Ok((children, ws_port, xauth)) => (Some(children), ws_port, Some(xauth)),
            Err(msg) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg),
        }
    } else {
        (None, None, None)
    };

    let takeover_url = format!("{base_url}/takeover/{bridge_session_id}?k={takeover_token}");
    let ts = now_ms();
    let mut transcript = VecDeque::with_capacity(8);
    transcript.push_back(TranscriptLine {
        role: "system".into(),
        text: format!("Antigravity GUI isolated session ready on display :{display_num}"),
        ts,
    });
    let session = ActiveSession {
        bridge_session_id: bridge_session_id.clone(),
        om_session_id: body.om_session_id,
        member_id: body.member_id,
        profile_dir,
        display_num,
        driver: resolved_driver,
        takeover_token,
        takeover_url,
        novnc_port,
        xauth_path,
        status: "ready".into(),
        last_active: Instant::now(),
        last_active_ms: ts,
        transcript,
        prompt_lock: Arc::new(tokio::sync::Mutex::new(())),
        children,
    };
    let info = session.to_info();

    {
        let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.reserved_slots = guard.reserved_slots.saturating_sub(1);
        guard.sessions.insert(bridge_session_id, session);
        reservation.committed = true;
    }

    (StatusCode::CREATED, Json(info)).into_response()
}

fn requires_human_takeover(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("captcha")
        || lower.contains("verify human")
        || lower.contains("2fa")
        || lower.contains("please login")
        || lower.contains("requires login")
        || prompt.contains("验证码")
        || prompt.contains("人机验证")
        || prompt.contains("需要登录")
        || prompt.contains("扫码登录")
}

async fn prompt_session(
    State(bridge): State<GuiBridge>,
    Path(id): Path<String>,
    Json(body): Json<GuiPromptRequest>,
) -> Response {
    let text = body.prompt.trim().to_string();
    if text.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "prompt cannot be empty",
        );
    }

    let (prompt_lock, driver, display_num, takeover_url, xauth_path) = {
        let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(sess) = guard.sessions.get_mut(&id) else {
            return json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("gui session {id} not found"),
            );
        };
        sess.last_active = Instant::now();
        sess.last_active_ms = now_ms();
        let ts = sess.last_active_ms;
        sess.push_transcript(TranscriptLine {
            role: "user".into(),
            text: text.clone(),
            ts,
        });
        (
            Arc::clone(&sess.prompt_lock),
            sess.driver,
            sess.display_num,
            sess.takeover_url.clone(),
            sess.xauth_path.clone(),
        )
    };

    // Serialize prompt injection per session so concurrent X11 keystrokes never interleave.
    let _serial_guard = prompt_lock.lock().await;

    // Security policy: NEVER attempt to bypass login, 2FA, or CAPTCHA gates.
    // Surface the takeover flag via `takeover_required` + `takeover_url` field without
    // embedding the secret `?k=` token inside the chat reply text.
    if requires_human_takeover(&text) {
        let reply = "检测到登录 / 身份校验或人机验证场景，GUI 桥接不会自动绕过安全验证。\n\
             请点击右上角「接管 GUI (noVNC)」直接在该成员独立隔离会话中完成人工操作。"
            .to_string();
        let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sess) = guard.sessions.get_mut(&id) {
            sess.status = "awaiting_human_takeover".into();
            sess.push_transcript(TranscriptLine {
                role: "assistant".into(),
                text: reply.clone(),
                ts: now_ms(),
            });
        }
        return Json(GuiPromptResponse {
            reply,
            takeover_required: true,
            takeover_url,
            status: "awaiting_human_takeover".into(),
        })
        .into_response();
    }

    let reply = if driver == "x11" && binary_on_path("xdotool") {
        let display = format!(":{display_num}");
        let mut type_cmd = Command::new("xdotool");
        apply_minimal_child_env(&mut type_cmd);
        type_cmd
            .env("DISPLAY", &display)
            .args(["type", "--clearmodifiers", "--delay", "12", "--", &text])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(ref auth) = xauth_path {
            type_cmd.env("XAUTHORITY", auth);
        }
        let typed = tokio::time::timeout(XDOTOOL_TIMEOUT, type_cmd.status()).await;

        let submitted = if matches!(typed, Ok(Ok(ref st)) if st.success()) {
            let mut key_cmd = Command::new("xdotool");
            apply_minimal_child_env(&mut key_cmd);
            key_cmd
                .env("DISPLAY", &display)
                .args(["key", "Return"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if let Some(ref auth) = xauth_path {
                key_cmd.env("XAUTHORITY", auth);
            }
            tokio::time::timeout(XDOTOOL_TIMEOUT, key_cmd.status()).await
        } else {
            Ok(Err(std::io::Error::other("xdotool type failed")))
        };

        match (typed, submitted) {
            (Ok(Ok(t)), Ok(Ok(s))) if t.success() && s.success() => format!(
                "[Antigravity GUI {display}] 已将指令注入隔离窗口并提交。如需实时查看画面或手动接管，请点击「接管 GUI」。"
            ),
            _ => format!(
                "[Antigravity GUI {display}] X11 输入注入未成功或超时，请点击「接管 GUI」通过远程画面人工接管。"
            ),
        }
    } else {
        format!("[Antigravity GUI :{display_num} · simulated] 已转达输入并收到回复：{text}")
    };

    let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(sess) = guard.sessions.get_mut(&id) {
        sess.status = "ready".into();
        sess.last_active = Instant::now();
        sess.last_active_ms = now_ms();
        let ts = sess.last_active_ms;
        sess.push_transcript(TranscriptLine {
            role: "assistant".into(),
            text: reply.clone(),
            ts,
        });
    }

    Json(GuiPromptResponse {
        reply,
        takeover_required: false,
        takeover_url,
        status: "ready".into(),
    })
    .into_response()
}

async fn cancel_session(State(bridge): State<GuiBridge>, Path(id): Path<String>) -> Response {
    let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(sess) = guard.sessions.get_mut(&id) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("gui session {id} not found"),
        );
    };
    sess.status = "cancelled".into();
    sess.last_active = Instant::now();
    sess.last_active_ms = now_ms();
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_session(State(bridge): State<GuiBridge>, Path(id): Path<String>) -> Response {
    let removed = {
        let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.sessions.remove(&id)
    };
    match removed {
        Some(sess) => {
            let display_num = sess.display_num;
            if let Some(children) = sess.children {
                children.terminate_and_reap().await;
            }
            bridge
                .inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .used_displays
                .remove(&display_num);
            StatusCode::NO_CONTENT.into_response()
        }
        None => json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("gui session {id} not found"),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct TakeoverQuery {
    k: Option<String>,
}

fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn apply_takeover_security_headers(mut res: Response) -> Response {
    let headers = res.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    res
}

async fn takeover_page(
    State(bridge): State<GuiBridge>,
    Path(id): Path<String>,
    Query(query): Query<TakeoverQuery>,
) -> Response {
    let guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(sess) = guard.sessions.get(&id) else {
        return apply_takeover_security_headers(
            (StatusCode::NOT_FOUND, "GUI session not found").into_response(),
        );
    };
    let ok = query
        .k
        .as_deref()
        .map(|k| constant_time_eq(k, &sess.takeover_token))
        .unwrap_or(false);
    if !ok {
        return apply_takeover_security_headers(
            (StatusCode::UNAUTHORIZED, "Invalid takeover token").into_response(),
        );
    }

    // Use server-recorded `novnc_port` (never an untrusted query parameter).
    let novnc_note = match sess.novnc_port {
        Some(port) => format!(
            "<div class=\"banner\">实时 noVNC WebSocket 通道已就绪：<code>ws://127.0.0.1:{port}</code>（Display <code>:{}</code>）</div>",
            sess.display_num
        ),
        None => format!(
            "<div class=\"banner\">当前运行于 <code>{}</code> 模式（Display <code>:{}</code>），支持直接人工接管交互。</div>",
            escape_html(sess.driver),
            sess.display_num
        ),
    };

    let transcript_html: String = sess
        .transcript
        .iter()
        .map(|line| {
            format!(
                "<div class=\"line role-{}\" data-ts=\"{}\"><span class=\"badge\">{}</span><pre>{}</pre></div>",
                escape_html(&line.role),
                line.ts,
                escape_html(&line.role.to_uppercase()),
                escape_html(&line.text)
            )
        })
        .collect();

    let token_q = escape_html(&sess.takeover_token);
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<meta name="referrer" content="no-referrer" />
<title>OpenMirasim GUI 远程接管 · {}</title>
<style>
  body {{ margin:0; font-family: system-ui, sans-serif; background:#0e1116; color:#e6edf3; padding:24px; }}
  .card {{ max-width:860px; margin:0 auto; background:#161b22; border:1px solid #30363d; border-radius:12px; padding:20px; }}
  h1 {{ margin:0 0 8px; font-size:18px; }}
  .meta {{ font-size:12.5px; color:#8b949e; margin-bottom:14px; font-family:monospace; }}
  .banner {{ background:#1f2d27; border:1px solid #2ea043; color:#7ee787; padding:10px 12px; border-radius:8px; font-size:13px; margin-bottom:16px; }}
  .stream {{ background:#0d1117; border:1px solid #30363d; border-radius:8px; padding:12px; max-height:380px; overflow:auto; margin-bottom:14px; }}
  .line {{ margin-bottom:10px; }}
  .badge {{ display:inline-block; font-size:11px; font-weight:600; padding:2px 6px; border-radius:4px; background:#21262d; color:#58a6ff; margin-bottom:4px; }}
  pre {{ margin:0; white-space:pre-wrap; font-size:13px; }}
  form {{ display:flex; gap:8px; }}
  input {{ flex:1; background:#0d1117; border:1px solid #30363d; color:#e6edf3; border-radius:8px; padding:10px 12px; font-size:13px; }}
  button {{ background:#238636; color:#fff; border:none; border-radius:8px; padding:10px 16px; font-weight:600; cursor:pointer; }}
</style>
</head>
<body>
<div class="card">
  <h1>Antigravity GUI 远程人工接管（noVNC）</h1>
  <div class="meta">member={} · display=:{} · status={}</div>
  {}
  <div class="stream" id="gui-transcript">{}</div>
  <form method="post" action="/takeover/{}/manual?k={}">
    <input name="text" type="text" placeholder="人工接管输入（例如完成登录确认或直接输入指令）" required />
    <button type="submit">人工发送</button>
  </form>
</div>
</body>
</html>"#,
        escape_html(&sess.bridge_session_id),
        escape_html(&sess.member_id),
        sess.display_num,
        escape_html(&sess.status),
        novnc_note,
        transcript_html,
        escape_html(&sess.bridge_session_id),
        token_q,
    );
    apply_takeover_security_headers(Html(html).into_response())
}

#[derive(Debug, Deserialize)]
struct ManualForm {
    text: String,
}

async fn takeover_manual_input(
    State(bridge): State<GuiBridge>,
    Path(id): Path<String>,
    Query(query): Query<TakeoverQuery>,
    axum::Form(form): axum::Form<ManualForm>,
) -> Response {
    let text = form.text.trim();
    if text.is_empty() || text.len() > MAX_MANUAL_INPUT_BYTES {
        return apply_takeover_security_headers(
            (
                StatusCode::BAD_REQUEST,
                "Manual input must be 1..=8192 bytes",
            )
                .into_response(),
        );
    }

    let mut guard = bridge.inner.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(sess) = guard.sessions.get_mut(&id) else {
        return apply_takeover_security_headers(
            (StatusCode::NOT_FOUND, "GUI session not found").into_response(),
        );
    };
    let ok = query
        .k
        .as_deref()
        .map(|k| constant_time_eq(k, &sess.takeover_token))
        .unwrap_or(false);
    if !ok {
        return apply_takeover_security_headers(
            (StatusCode::UNAUTHORIZED, "Invalid takeover token").into_response(),
        );
    }
    let ts = now_ms();
    sess.status = "ready".into();
    sess.last_active = Instant::now();
    sess.last_active_ms = ts;
    sess.push_transcript(TranscriptLine {
        role: "human-takeover".into(),
        text: text.to_string(),
        ts,
    });
    let location = format!(
        "/takeover/{}?k={}",
        sess.bridge_session_id, sess.takeover_token
    );
    apply_takeover_security_headers(
        (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response(),
    )
}
