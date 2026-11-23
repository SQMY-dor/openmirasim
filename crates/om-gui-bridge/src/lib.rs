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
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

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
    /// Optional shared Bearer token protecting `/v1/*` bridge control endpoints.
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

fn binary_on_path(bin: &str) -> bool {
    let p = FsPath::new(bin);
    if p.components().count() > 1 {
        return p.is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
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

fn ensure_isolated_profile(profile_root: &FsPath, member_id: &str) -> Result<PathBuf, String> {
    let safe_id = validate_member_id(member_id)?;
    std::fs::create_dir_all(profile_root)
        .map_err(|e| format!("failed to create profile root: {e}"))?;
    let dir = profile_root.join(safe_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create member profile: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("failed to chmod member profile to 0700: {e}"))?;
    }
    // Create standard XDG subdirectories inside the member's isolated home.
    for sub in [".config", ".local/share", ".cache"] {
        let _ = std::fs::create_dir_all(dir.join(sub));
    }
    Ok(dir)
}

struct X11Children {
    xvfb: Option<Child>,
    gui: Option<Child>,
    vnc: Option<Child>,
    websockify: Option<Child>,
}

impl X11Children {
    fn kill_all(&mut self) {
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
}

impl Drop for X11Children {
    fn drop(&mut self) {
        self.kill_all();
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
    status: String,
    last_active: Instant,
    last_active_ms: i64,
    transcript: Vec<TranscriptLine>,
    children: Option<X11Children>,
}

impl ActiveSession {
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
        std::fs::create_dir_all(&config.profile_root)
            .map_err(|e| format!("failed to initialize profile_root: {e}"))?;

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
                let reaped = reap_idle_sessions(&reaper_inner);
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
        self.inner.lock().unwrap().base_url.clone()
    }

    pub fn active_count(&self) -> usize {
        self.inner.lock().unwrap().sessions.len()
    }
}

fn reap_idle_sessions(inner: &Arc<Mutex<ManagerInner>>) -> usize {
    let mut guard = inner.lock().unwrap();
    let timeout = guard.config.idle_timeout;
    let now = Instant::now();
    let stale: Vec<String> = guard
        .sessions
        .iter()
        .filter(|(_, s)| now.duration_since(s.last_active) >= timeout)
        .map(|(id, _)| id.clone())
        .collect();
    let count = stale.len();
    for id in stale {
        if let Some(mut sess) = guard.sessions.remove(&id) {
            if let Some(children) = sess.children.as_mut() {
                children.kill_all();
            }
            guard.used_displays.remove(&sess.display_num);
        }
    }
    count
}

fn allocate_display(used: &mut HashSet<u16>) -> Option<u16> {
    (100..200).find(|&d| used.insert(d))
}

fn spawn_x11_session(
    display_num: u16,
    profile_dir: &FsPath,
    gui_binary: &str,
    cwd: Option<&str>,
) -> Result<(X11Children, Option<u16>), String> {
    let display = format!(":{display_num}");
    let xvfb = Command::new("Xvfb")
        .args([&display, "-screen", "0", "1440x900x24", "-nolisten", "tcp"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn Xvfb: {e}"))?;

    let mut gui_cmd = Command::new(gui_binary);
    gui_cmd
        .env("DISPLAY", &display)
        .env("HOME", profile_dir)
        .env("XDG_CONFIG_HOME", profile_dir.join(".config"))
        .env("XDG_DATA_HOME", profile_dir.join(".local/share"))
        .env("XDG_CACHE_HOME", profile_dir.join(".cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = cwd
        && !dir.is_empty()
        && FsPath::new(dir).is_dir()
    {
        gui_cmd.current_dir(dir);
    }
    let gui = gui_cmd
        .spawn()
        .map_err(|e| format!("failed to spawn GUI binary `{gui_binary}`: {e}"))?;

    let mut vnc = None;
    let mut websockify = None;
    let mut novnc_port = None;
    if binary_on_path("x11vnc") && binary_on_path("websockify") {
        let rfb_port = 5900 + display_num;
        let ws_port = 6080 + (display_num - 100);
        if let Ok(v) = Command::new("x11vnc")
            .args([
                "-display",
                &display,
                "-localhost",
                "-rfbport",
                &rfb_port.to_string(),
                "-nopw",
                "-forever",
                "-shared",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            vnc = Some(v);
            if let Ok(w) = Command::new("websockify")
                .args([
                    &format!("127.0.0.1:{ws_port}"),
                    &format!("127.0.0.1:{rfb_port}"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                websockify = Some(w);
                novnc_port = Some(ws_port);
            }
        }
    }

    Ok((
        X11Children {
            xvfb: Some(xvfb),
            gui: Some(gui),
            vnc,
            websockify,
        },
        novnc_port,
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
    let expected = bridge.inner.lock().unwrap().config.bridge_token.clone();
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
    let guard = bridge.inner.lock().unwrap();
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
    let guard = bridge.inner.lock().unwrap();
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
    let guard = bridge.inner.lock().unwrap();
    match guard.sessions.get(&id) {
        Some(s) => Json(s.to_info()).into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("gui session {id} not found"),
        ),
    }
}

async fn create_session(
    State(bridge): State<GuiBridge>,
    Json(body): Json<CreateGuiSessionRequest>,
) -> Response {
    let mut guard = bridge.inner.lock().unwrap();
    if guard.sessions.len() >= guard.config.max_sessions {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "limit_exceeded",
            format!(
                "GUI bridge concurrency cap reached ({}/{})",
                guard.sessions.len(),
                guard.config.max_sessions
            ),
        );
    }

    let profile_dir = match ensure_isolated_profile(&guard.config.profile_root, &body.member_id) {
        Ok(dir) => dir,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, "invalid_input", msg),
    };

    let Some(display_num) = allocate_display(&mut guard.used_displays) else {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "limit_exceeded",
            "no free Xvfb display slots available",
        );
    };

    let bridge_session_id = format!("gui-{}", uuid::Uuid::now_v7());
    let takeover_token = uuid::Uuid::now_v7().simple().to_string();

    let (children, takeover_url) = if guard.resolved_driver == "x11" {
        match spawn_x11_session(
            display_num,
            &profile_dir,
            &guard.config.gui_binary,
            body.cwd.as_deref(),
        ) {
            Ok((children, Some(ws_port))) => (
                Some(children),
                format!(
                    "{}/takeover/{bridge_session_id}?k={takeover_token}&novnc_port={ws_port}",
                    guard.base_url
                ),
            ),
            Ok((children, None)) => (
                Some(children),
                format!(
                    "{}/takeover/{bridge_session_id}?k={takeover_token}",
                    guard.base_url
                ),
            ),
            Err(msg) => {
                guard.used_displays.remove(&display_num);
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg);
            }
        }
    } else {
        (
            None,
            format!(
                "{}/takeover/{bridge_session_id}?k={takeover_token}",
                guard.base_url
            ),
        )
    };

    let ts = now_ms();
    let session = ActiveSession {
        bridge_session_id: bridge_session_id.clone(),
        om_session_id: body.om_session_id,
        member_id: body.member_id,
        profile_dir,
        display_num,
        driver: guard.resolved_driver,
        takeover_token,
        takeover_url,
        status: "ready".into(),
        last_active: Instant::now(),
        last_active_ms: ts,
        transcript: vec![TranscriptLine {
            role: "system".into(),
            text: format!("Antigravity GUI isolated session ready on display :{display_num}"),
            ts,
        }],
        children,
    };
    let info = session.to_info();
    guard.sessions.insert(bridge_session_id, session);
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

    let (driver, display_num, takeover_url) = {
        let mut guard = bridge.inner.lock().unwrap();
        let Some(sess) = guard.sessions.get_mut(&id) else {
            return json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("gui session {id} not found"),
            );
        };
        sess.last_active = Instant::now();
        sess.last_active_ms = now_ms();
        sess.transcript.push(TranscriptLine {
            role: "user".into(),
            text: text.clone(),
            ts: sess.last_active_ms,
        });
        (sess.driver, sess.display_num, sess.takeover_url.clone())
    };

    // Security policy: NEVER attempt to bypass login, 2FA, or CAPTCHA gates.
    // Surface the isolated per-member remote-screen takeover URL instead.
    if requires_human_takeover(&text) {
        let reply = format!(
            "检测到登录 / 身份校验或人机验证场景，GUI 桥接不会自动绕过安全验证。\n\
             请点击右上角「接管 GUI (noVNC)」直接在该成员独立隔离会话中完成人工操作：\n{takeover_url}"
        );
        let mut guard = bridge.inner.lock().unwrap();
        if let Some(sess) = guard.sessions.get_mut(&id) {
            sess.status = "awaiting_human_takeover".into();
            sess.transcript.push(TranscriptLine {
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
        let typed = Command::new("xdotool")
            .env("DISPLAY", &display)
            .args(["type", "--clearmodifiers", "--delay", "12", "--", &text])
            .status()
            .await;
        let submitted = Command::new("xdotool")
            .env("DISPLAY", &display)
            .args(["key", "Return"])
            .status()
            .await;
        match (typed, submitted) {
            (Ok(t), Ok(s)) if t.success() && s.success() => format!(
                "[Antigravity GUI {display}] 已将指令注入隔离窗口并提交。如需实时查看画面或手动接管，请打开：{takeover_url}"
            ),
            _ => format!(
                "[Antigravity GUI {display}] X11 输入注入未成功，请通过远程画面人工接管：{takeover_url}"
            ),
        }
    } else {
        format!("[Antigravity GUI :{display_num} · simulated] 已转达输入并收到回复：{text}")
    };

    let mut guard = bridge.inner.lock().unwrap();
    if let Some(sess) = guard.sessions.get_mut(&id) {
        sess.status = "ready".into();
        sess.last_active = Instant::now();
        sess.last_active_ms = now_ms();
        sess.transcript.push(TranscriptLine {
            role: "assistant".into(),
            text: reply.clone(),
            ts: sess.last_active_ms,
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
    let mut guard = bridge.inner.lock().unwrap();
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
    let mut guard = bridge.inner.lock().unwrap();
    match guard.sessions.remove(&id) {
        Some(mut sess) => {
            if let Some(children) = sess.children.as_mut() {
                children.kill_all();
            }
            guard.used_displays.remove(&sess.display_num);
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
    novnc_port: Option<u16>,
}

fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn takeover_page(
    State(bridge): State<GuiBridge>,
    Path(id): Path<String>,
    Query(query): Query<TakeoverQuery>,
) -> Response {
    let guard = bridge.inner.lock().unwrap();
    let Some(sess) = guard.sessions.get(&id) else {
        return (StatusCode::NOT_FOUND, "GUI session not found").into_response();
    };
    let ok = query
        .k
        .as_deref()
        .map(|k| constant_time_eq(k, &sess.takeover_token))
        .unwrap_or(false);
    if !ok {
        return (StatusCode::UNAUTHORIZED, "Invalid takeover token").into_response();
    }

    let novnc_note = match query.novnc_port {
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

    let token_q = escape_html(query.k.as_deref().unwrap_or_default());
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
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
  <div class="meta">member={} · display=:{} · profile={} · status={}</div>
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
        escape_html(&sess.profile_dir.to_string_lossy()),
        escape_html(&sess.status),
        novnc_note,
        transcript_html,
        escape_html(&sess.bridge_session_id),
        token_q,
    );
    Html(html).into_response()
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
    let mut guard = bridge.inner.lock().unwrap();
    let Some(sess) = guard.sessions.get_mut(&id) else {
        return (StatusCode::NOT_FOUND, "GUI session not found").into_response();
    };
    let ok = query
        .k
        .as_deref()
        .map(|k| constant_time_eq(k, &sess.takeover_token))
        .unwrap_or(false);
    if !ok {
        return (StatusCode::UNAUTHORIZED, "Invalid takeover token").into_response();
    }
    let ts = now_ms();
    sess.status = "ready".into();
    sess.last_active = Instant::now();
    sess.last_active_ms = ts;
    sess.transcript.push(TranscriptLine {
        role: "human-takeover".into(),
        text: form.text.trim().to_string(),
        ts,
    });
    let location = format!("/takeover/{id}?k={}", sess.takeover_token);
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}
