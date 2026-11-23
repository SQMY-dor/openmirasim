//! Antigravity GUI Bridge harness adapter (`antigravity-gui`, W2-5).
//!
//! Connects out-of-process to a running `om-gui-bridge` daemon over loopback/private HTTP
//! so Linux Xvfb/GUI failures stay completely isolated from `om-server`.

use async_trait::async_trait;
use futures::StreamExt;
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
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

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

/// Probes an external `om-gui-bridge` instance via `GET /healthz`.
pub async fn probe_gui_bridge(config: &GuiBridgeConfig) -> GuiBridgeProbe {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
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
        Ok(res) if res.status().is_success() => match res.bytes().await {
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
        if let Ok(mut map) = self.inner.lock() {
            map.insert(session_id, info);
        }
    }

    pub fn get(&self, session_id: &SessionId) -> Option<RemoteScreenInfo> {
        self.inner.lock().ok()?.get(session_id).cloned()
    }

    pub fn update_status(&self, session_id: &SessionId, status: &str) {
        if let Ok(mut map) = self.inner.lock()
            && let Some(info) = map.get_mut(session_id)
        {
            info.status = status.to_string();
        }
    }

    pub fn remove(&self, session_id: &SessionId) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(session_id);
        }
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
        let member_key = sanitize_profile_key(&format!("sess-{}", ctx.session_id));

        // Reuse existing bridge session when resuming, if still live on the bridge.
        let wire = if let Some(native_id) = ctx.resume_native_session_id.as_deref() {
            let mut req = self
                .http
                .get(format!("{}/v1/sessions/{native_id}", self.config.base_url));
            if let Some(tok) = &self.config.token {
                req = req.bearer_auth(tok);
            }
            match req.send().await {
                Ok(res) if res.status().is_success() => {
                    let bytes = res
                        .bytes()
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
                let bytes = res
                    .bytes()
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

        let remote_info = RemoteScreenInfo {
            available: true,
            bridge_session_id: info.bridge_session_id.clone(),
            takeover_url: info.takeover_url.clone(),
            display: info.display.clone(),
            driver: info.driver.clone(),
            profile_dir: info.profile_dir.clone(),
            status: info.status.clone(),
        };
        self.screens.insert(ctx.session_id.clone(), remote_info);

        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(HarnessEvent::NativeSessionId(
            info.bridge_session_id.clone(),
        ));

        let session = GuiBridgeSession {
            session_id: ctx.session_id,
            bridge_session_id: info.bridge_session_id,
            config: self.config.clone(),
            http: self.http.clone(),
            screens: self.screens.clone(),
            tx: Arc::new(Mutex::new(Some(tx))),
        };
        let events =
            futures::stream::unfold(
                rx,
                |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) },
            )
            .boxed();
        Ok(StartedSession {
            session: Box::new(session),
            events,
        })
    }
}

struct GuiBridgeSession {
    session_id: SessionId,
    bridge_session_id: String,
    config: GuiBridgeConfig,
    http: reqwest::Client,
    screens: RemoteScreenRegistry,
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<HarnessEvent>>>>,
}

impl GuiBridgeSession {
    fn emit(&self, ev: HarnessEvent) {
        if let Ok(guard) = self.tx.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(ev);
        }
    }
}

#[async_trait]
impl HarnessSession for GuiBridgeSession {
    fn native_session_id(&self) -> Option<String> {
        Some(self.bridge_session_id.clone())
    }

    async fn prompt(&self, prompt: Prompt) -> CoreResult<()> {
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

        tokio::spawn(async move {
            let emit = |ev: HarnessEvent| {
                if let Ok(guard) = tx.lock()
                    && let Some(sender) = guard.as_ref()
                {
                    let _ = sender.send(ev);
                }
            };

            let body = match serde_json::to_vec(&GuiPromptReq { prompt: text }) {
                Ok(b) => b,
                Err(e) => {
                    emit(HarnessEvent::TurnEnded {
                        stop_reason: StopReason::Failed,
                        error: Some(format!("serialize prompt: {e}")),
                    });
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

            match req.send().await {
                Ok(res) if res.status().is_success() => match res.bytes().await {
                    Ok(bytes) => match serde_json::from_slice::<GuiPromptWire>(&bytes) {
                        Ok(wire) => {
                            screens.update_status(&sid, &wire.status);
                            let mut text = wire.reply;
                            if wire.takeover_required && !text.contains(&wire.takeover_url) {
                                text.push_str("\n\n远程接管地址: ");
                                text.push_str(&wire.takeover_url);
                            }
                            emit(HarnessEvent::MessageChunk(ContentBlock::Text { text }));
                            emit(HarnessEvent::TurnEnded {
                                stop_reason: StopReason::EndTurn,
                                error: None,
                            });
                        }
                        Err(e) => emit(HarnessEvent::TurnEnded {
                            stop_reason: StopReason::Failed,
                            error: Some(format!("invalid gui bridge prompt reply: {e}")),
                        }),
                    },
                    Err(e) => emit(HarnessEvent::TurnEnded {
                        stop_reason: StopReason::Failed,
                        error: Some(format!("gui bridge body read failed: {e}")),
                    }),
                },
                Ok(res) => emit(HarnessEvent::TurnEnded {
                    stop_reason: StopReason::Failed,
                    error: Some(format!("gui bridge prompt failed: HTTP {}", res.status())),
                }),
                Err(e) => emit(HarnessEvent::TurnEnded {
                    stop_reason: StopReason::Failed,
                    error: Some(format!("gui bridge prompt request failed: {e}")),
                }),
            }
        });

        Ok(())
    }

    async fn respond_approval(
        &self,
        _request_id: &str,
        _decision: ApprovalDecision,
    ) -> CoreResult<()> {
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
        self.emit(HarnessEvent::TurnEnded {
            stop_reason: StopReason::Cancelled,
            error: None,
        });
        Ok(())
    }

    async fn shutdown(&self) -> CoreResult<()> {
        let mut req = self.http.delete(format!(
            "{}/v1/sessions/{}",
            self.config.base_url, self.bridge_session_id
        ));
        if let Some(tok) = &self.config.token {
            req = req.bearer_auth(tok);
        }
        let _ = req.send().await;
        self.screens.remove(&self.session_id);
        self.emit(HarnessEvent::Exited { error: None });
        if let Ok(mut guard) = self.tx.lock() {
            *guard = None;
        }
        Ok(())
    }
}
