//! Antigravity GUI Bridge harness adapter (`antigravity-gui`, W2-5).
//!
//! Connects out-of-process to a running `om-gui-bridge` daemon over loopback/private HTTP
//! so Linux Xvfb/GUI failures stay completely isolated from `om-server`.

use super::probe::is_safe_path_segment;
use crate::stream::BoundedReceiverStream;
use async_trait::async_trait;
use om_core::approval::ApprovalDecision;
use om_core::content::{ContentBlock, Prompt};
use om_core::harness::{
    Harness, HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::ids::SessionId;
use om_core::session::StopReason;
use om_core::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Maximum response body read from `om-gui-bridge` (1 MiB).
const MAX_BRIDGE_RESPONSE_BYTES: usize = 1024 * 1024;

/// Capacity of the bounded harness event channel.
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// Configuration for connecting `om-server` to an external `om-gui-bridge` process.
#[derive(Debug, Clone)]
pub struct GuiBridgeConfig {
    /// Base URL of the external `om-gui-bridge` daemon (e.g. `http://127.0.0.1:4941`).
    pub base_url: String,
    /// Optional shared Bearer token matching `OM_GUI_BRIDGE_TOKEN`.
    pub token: Option<String>,
}

impl GuiBridgeConfig {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
        }
    }

    /// Reads `OM_GUI_BRIDGE_URL` and optional `OM_GUI_BRIDGE_TOKEN` from the environment.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("OM_GUI_BRIDGE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let token = std::env::var("OM_GUI_BRIDGE_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Some(Self::new(url, token))
    }
}

/// Live remote-screen metadata returned by `GET /api/v1/sessions/:id/remote-screen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteScreenInfo {
    pub available: bool,
    pub bridge_session_id: String,
    pub takeover_url: String,
    pub display: String,
    pub driver: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_dir: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BridgeHealthWire {
    status: String,
    driver: String,
    active_sessions: usize,
    max_sessions: usize,
}

#[derive(Debug, Clone, Serialize)]
struct CreateGuiReq {
    om_session_id: String,
    member_id: String,
    cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GuiSessionWire {
    bridge_session_id: String,
    #[serde(default)]
    profile_dir: String,
    display: String,
    driver: String,
    takeover_url: String,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct GuiPromptReq {
    prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GuiPromptWire {
    reply: String,
    takeover_required: bool,
    #[serde(default)]
    #[allow(dead_code)]
    takeover_url: String,
    status: String,
}

/// Health probe result for the external GUI bridge.
#[derive(Debug, Clone)]
pub struct GuiBridgeProbe {
    pub available: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

async fn read_body_capped(mut res: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    if let Some(len) = res.content_length()
        && len as usize > cap
    {
        return Err(format!("response exceeds {cap} bytes"));
    }
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(format!("response exceeds {cap} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn validate_takeover_url(raw: &str, base_url: &str) -> CoreResult<String> {
    let parsed = reqwest::Url::parse(raw)
        .map_err(|_| CoreError::Harness("bridge returned an invalid takeover_url".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::Harness(
            "bridge takeover_url must use http or https".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CoreError::Harness(
            "bridge takeover_url must not contain userinfo".into(),
        ));
    }
    if parsed.host_str().is_none() || !parsed.path().starts_with("/takeover/") {
        return Err(CoreError::Harness(
            "bridge takeover_url must target /takeover/...".into(),
        ));
    }
    // If an explicit public URL is NOT configured on the host, require takeover_url origin to
    // match the configured bridge base_url origin so a compromised bridge cannot redirect users
    // to arbitrary third-party origins.
    if let Ok(base) = reqwest::Url::parse(base_url) {
        let same_origin = parsed.origin() == base.origin();
        let allowed_public = std::env::var("OM_GUI_PUBLIC_URL")
            .ok()
            .and_then(|u| reqwest::Url::parse(u.trim()).ok())
            .is_some_and(|pub_u| parsed.origin() == pub_u.origin());
        if !same_origin && !allowed_public {
            return Err(CoreError::Harness(
                "bridge takeover_url origin does not match configured bridge or OM_GUI_PUBLIC_URL"
                    .into(),
            ));
        }
    }
    Ok(parsed.to_string())
}

/// Probes an external `om-gui-bridge` instance via `GET /healthz`.
pub async fn probe_gui_bridge(config: &GuiBridgeConfig) -> GuiBridgeProbe {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return GuiBridgeProbe {
                available: false,
                version: None,
                error: Some(format!("http client error: {e}")),
            };
        }
    };

    let url = format!("{}/healthz", config.base_url);
    match client.get(&url).send().await {
        Ok(res) if res.status().is_success() => match read_body_capped(res, 64 * 1024).await {
            Ok(bytes) => match serde_json::from_slice::<BridgeHealthWire>(&bytes) {
                Ok(h) => GuiBridgeProbe {
                    available: h.status == "ok",
                    version: Some(format!(
                        "{} ({}/{} slots)",
                        h.driver, h.active_sessions, h.max_sessions
                    )),
                    error: None,
                },
                Err(e) => GuiBridgeProbe {
                    available: false,
                    version: None,
                    error: Some(format!("invalid healthz JSON: {e}")),
                },
            },
            Err(e) => GuiBridgeProbe {
                available: false,
                version: None,
                error: Some(format!("failed to read healthz body: {e}")),
            },
        },
        Ok(res) => GuiBridgeProbe {
            available: false,
            version: None,
            error: Some(format!("bridge returned HTTP {}", res.status())),
        },
        Err(e) => GuiBridgeProbe {
            available: false,
            version: None,
            error: Some(format!("bridge unreachable at {}: {e}", config.base_url)),
        },
    }
}

/// Thread-safe registry tracking active remote-screen links for `antigravity-gui` sessions.
#[derive(Clone, Default)]
pub struct RemoteScreenRegistry {
    inner: Arc<Mutex<HashMap<SessionId, RemoteScreenInfo>>>,
}

impl RemoteScreenRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session_id: SessionId, info: RemoteScreenInfo) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, info);
    }

    pub fn get(&self, session_id: &SessionId) -> Option<RemoteScreenInfo> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    pub fn update_status(&self, session_id: &SessionId, status: &str) {
        if let Some(info) = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(session_id)
        {
            info.status = status.to_string();
        }
    }

    pub fn remove(&self, session_id: &SessionId) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
    }
}

/// [`Harness`] adapter delegating `"antigravity-gui"` sessions to external `om-gui-bridge`.
pub struct GuiBridgeHarness {
    config: GuiBridgeConfig,
    http: reqwest::Client,
    screens: RemoteScreenRegistry,
}

impl GuiBridgeHarness {
    pub fn new(config: GuiBridgeConfig, screens: RemoteScreenRegistry) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        Self {
            config,
            http,
            screens,
        }
    }

    pub fn config(&self) -> &GuiBridgeConfig {
        &self.config
    }

    pub fn screens(&self) -> &RemoteScreenRegistry {
        &self.screens
    }
}

fn sanitize_profile_key(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "member-default".into()
    } else {
        cleaned
    }
}

fn prompt_to_text(prompt: &Prompt) -> String {
    prompt
        .content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::ResourceLink { uri, .. } => uri.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Harness for GuiBridgeHarness {
    fn id(&self) -> &str {
        "antigravity-gui"
    }

    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            native_approval: false,
            steer: false,
            resume: true,
            structured_tools: false,
            pty_attach: false,
            remote_screen: true,
        }
    }

    async fn start(&self, ctx: StartContext) -> CoreResult<StartedSession> {
        // Isolate GUI profile per member when authenticated, or per session when anonymous.
        let member_key = match &ctx.created_by {
            Some(member) => sanitize_profile_key(member.as_str()),
            None => sanitize_profile_key(&format!("sess-{}", ctx.session_id)),
        };

        // Reuse existing bridge session when resuming, if still live on the bridge.
        let wire = if let Some(native_id) = ctx.resume_native_session_id.as_deref() {
            if !is_safe_path_segment(native_id) {
                return Err(CoreError::InvalidInput(
                    "invalid resume bridge_session_id".into(),
                ));
            }
            let mut req = self
                .http
                .get(format!("{}/v1/sessions/{native_id}", self.config.base_url));
            if let Some(tok) = &self.config.token {
                req = req.bearer_auth(tok);
            }
            match req.send().await {
                Ok(res) if res.status().is_success() => {
                    let bytes = read_body_capped(res, MAX_BRIDGE_RESPONSE_BYTES)
                        .await
                        .map_err(|e| CoreError::Harness(format!("gui bridge read error: {e}")))?;
                    serde_json::from_slice::<GuiSessionWire>(&bytes).ok()
                }
                _ => None,
            }
        } else {
            None
        };

        let info = match wire {
            Some(existing) => existing,
            None => {
                let body = serde_json::to_vec(&CreateGuiReq {
                    om_session_id: ctx.session_id.to_string(),
                    member_id: member_key,
                    cwd: if ctx.config.cwd.is_empty() {
                        None
                    } else {
                        Some(ctx.config.cwd.clone())
                    },
                })
                .map_err(|e| CoreError::Harness(format!("serialize bridge req: {e}")))?;

                let mut req = self
                    .http
                    .post(format!("{}/v1/sessions", self.config.base_url))
                    .header("content-type", "application/json")
                    .body(body);
                if let Some(tok) = &self.config.token {
                    req = req.bearer_auth(tok);
                }
                let res = req
                    .send()
                    .await
                    .map_err(|e| CoreError::Harness(format!("gui bridge unreachable: {e}")))?;
                let status = res.status();
                let bytes = read_body_capped(res, MAX_BRIDGE_RESPONSE_BYTES)
                    .await
                    .map_err(|e| CoreError::Harness(format!("gui bridge response read: {e}")))?;
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(CoreError::LimitExceeded(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ));
                }
                if !status.is_success() {
                    return Err(CoreError::Harness(format!(
                        "gui bridge create session failed (HTTP {status}): {}",
                        String::from_utf8_lossy(&bytes)
                    )));
                }
                serde_json::from_slice::<GuiSessionWire>(&bytes)
                    .map_err(|e| CoreError::Harness(format!("invalid bridge session JSON: {e}")))?
            }
        };

        if !is_safe_path_segment(&info.bridge_session_id) {
            return Err(CoreError::Harness(
                "bridge returned an unsafe bridge_session_id".into(),
            ));
        }
        let validated_takeover_url =
            validate_takeover_url(&info.takeover_url, &self.config.base_url)?;

        let remote_info = RemoteScreenInfo {
            available: true,
            bridge_session_id: info.bridge_session_id.clone(),
            takeover_url: validated_takeover_url,
            display: info.display.clone(),
            driver: info.driver.clone(),
            profile_dir: info.profile_dir.clone(),
            status: info.status.clone(),
        };
        self.screens.insert(ctx.session_id.clone(), remote_info);

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let _ = tx.try_send(HarnessEvent::NativeSessionId(
            info.bridge_session_id.clone(),
        ));

        let session = GuiBridgeSession {
            session_id: ctx.session_id,
            bridge_session_id: info.bridge_session_id,
            config: self.config.clone(),
            http: self.http.clone(),
            screens: self.screens.clone(),
            tx: Arc::new(Mutex::new(Some(tx))),
            turn_active: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            active_turn: Arc::new(Mutex::new(None)),
        };
        Ok(StartedSession {
            session: Box::new(session),
            events: Box::pin(BoundedReceiverStream::new(rx)),
        })
    }
}

struct ActiveGuiTurn {
    finished: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct GuiBridgeSession {
    session_id: SessionId,
    bridge_session_id: String,
    config: GuiBridgeConfig,
    http: reqwest::Client,
    screens: RemoteScreenRegistry,
    tx: Arc<Mutex<Option<mpsc::Sender<HarnessEvent>>>>,
    turn_active: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    active_turn: Arc<Mutex<Option<ActiveGuiTurn>>>,
}

impl Drop for GuiBridgeSession {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.screens.remove(&self.session_id);
        if let Some(turn) = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            turn.finished.store(true, Ordering::SeqCst);
            turn.handle.abort();
        }
    }
}

async fn emit_gui_event(tx: &Mutex<Option<mpsc::Sender<HarnessEvent>>>, ev: HarnessEvent) {
    let sender = tx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(sender) = sender {
        let _ = sender.send(ev).await;
    }
}

#[async_trait]
impl HarnessSession for GuiBridgeSession {
    fn native_session_id(&self) -> Option<String> {
        Some(self.bridge_session_id.clone())
    }

    async fn prompt(&self, prompt: Prompt) -> CoreResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::Harness(
                "antigravity-gui session is closed".into(),
            ));
        }
        if prompt.is_empty() {
            return Err(CoreError::InvalidInput("prompt must not be empty".into()));
        }
        if self
            .turn_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(CoreError::InvalidInput("a turn is already running".into()));
        }

        let text = prompt_to_text(&prompt);
        let http = self.http.clone();
        let url = format!(
            "{}/v1/sessions/{}/prompt",
            self.config.base_url, self.bridge_session_id
        );
        let token = self.config.token.clone();
        let tx = self.tx.clone();
        let screens = self.screens.clone();
        let sid = self.session_id.clone();
        let turn_active = Arc::clone(&self.turn_active);
        let finished = Arc::new(AtomicBool::new(false));
        let finished_for_task = Arc::clone(&finished);

        let handle = tokio::spawn(async move {
            let body = match serde_json::to_vec(&GuiPromptReq { prompt: text }) {
                Ok(b) => b,
                Err(e) => {
                    if !finished_for_task.swap(true, Ordering::SeqCst) {
                        turn_active.store(false, Ordering::SeqCst);
                        emit_gui_event(
                            &tx,
                            HarnessEvent::TurnEnded {
                                stop_reason: StopReason::Failed,
                                error: Some(format!("serialize prompt: {e}")),
                            },
                        )
                        .await;
                    }
                    return;
                }
            };

            let mut req = http
                .post(&url)
                .header("content-type", "application/json")
                .body(body);
            if let Some(tok) = token {
                req = req.bearer_auth(tok);
            }

            let outcome = match req.send().await {
                Ok(res) if res.status().is_success() => {
                    match read_body_capped(res, MAX_BRIDGE_RESPONSE_BYTES).await {
                        Ok(bytes) => match serde_json::from_slice::<GuiPromptWire>(&bytes) {
                            Ok(wire) => {
                                screens.update_status(&sid, &wire.status);
                                let mut text = wire.reply;
                                // Never embed the secret-bearing takeover_url into the shared
                                // event stream (which Viewers can read); prompt-capable members
                                // fetch it from GET /api/v1/sessions/:id/remote-screen instead.
                                if wire.takeover_required {
                                    text.push_str(
                                        "\n\n[需要人工远程接管：请点击页面顶部的「接管 GUI」按钮打开受权接管画面]",
                                    );
                                }
                                Ok(text)
                            }
                            Err(e) => Err(format!("invalid gui bridge prompt reply: {e}")),
                        },
                        Err(e) => Err(format!("gui bridge body read failed: {e}")),
                    }
                }
                Ok(res) => Err(format!("gui bridge prompt failed: HTTP {}", res.status())),
                Err(e) => Err(format!("gui bridge prompt request failed: {e}")),
            };

            if finished_for_task.swap(true, Ordering::SeqCst) {
                return;
            }
            turn_active.store(false, Ordering::SeqCst);
            match outcome {
                Ok(text) => {
                    emit_gui_event(&tx, HarnessEvent::MessageChunk(ContentBlock::Text { text }))
                        .await;
                    emit_gui_event(
                        &tx,
                        HarnessEvent::TurnEnded {
                            stop_reason: StopReason::EndTurn,
                            error: None,
                        },
                    )
                    .await;
                }
                Err(err) => {
                    emit_gui_event(
                        &tx,
                        HarnessEvent::TurnEnded {
                            stop_reason: StopReason::Failed,
                            error: Some(err),
                        },
                    )
                    .await;
                }
            }
        });

        *self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ActiveGuiTurn { finished, handle });

        Ok(())
    }

    async fn respond_approval(
        &self,
        _request_id: &str,
        decision: ApprovalDecision,
    ) -> CoreResult<()> {
        if matches!(decision, ApprovalDecision::Cancelled) {
            return Ok(());
        }
        Err(CoreError::Unsupported {
            harness: "antigravity-gui".into(),
            capability: "native_approval",
        })
    }

    async fn cancel(&self) -> CoreResult<()> {
        let mut req = self.http.post(format!(
            "{}/v1/sessions/{}/cancel",
            self.config.base_url, self.bridge_session_id
        ));
        if let Some(tok) = &self.config.token {
            req = req.bearer_auth(tok);
        }
        let _ = req.send().await;

        let should_emit = {
            let mut guard = self
                .active_turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(turn) = guard.take() {
                turn.handle.abort();
                !turn.finished.swap(true, Ordering::SeqCst)
            } else {
                false
            }
        };
        self.turn_active.store(false, Ordering::SeqCst);
        if should_emit {
            emit_gui_event(
                &self.tx,
                HarnessEvent::TurnEnded {
                    stop_reason: StopReason::Cancelled,
                    error: None,
                },
            )
            .await;
        }
        Ok(())
    }

    async fn shutdown(&self) -> CoreResult<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(turn) = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            turn.finished.store(true, Ordering::SeqCst);
            turn.handle.abort();
        }
        self.turn_active.store(false, Ordering::SeqCst);

        let mut req = self.http.delete(format!(
            "{}/v1/sessions/{}",
            self.config.base_url, self.bridge_session_id
        ));
        if let Some(tok) = &self.config.token {
            req = req.bearer_auth(tok);
        }
        let _ = req.send().await;
        self.screens.remove(&self.session_id);
        emit_gui_event(&self.tx, HarnessEvent::Exited { error: None }).await;
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(())
    }
}
