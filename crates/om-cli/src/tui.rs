//! Ratatui terminal dashboard and direct `/ws/pty` attach client (`om-cli tui`, Phase 3).
//!
//! Designed for headless Linux servers: inspect all team sessions, watch live `/ws/events`
//! streams, approve pending tool calls, submit prompts, and press `t` to attach directly
//! to a session's native binary PTY stream (detach with `Ctrl+]`).

use bytes::Bytes;
use clap::Args;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::{SinkExt, StreamExt};
use om_core::api::{ClientMessage, ListSessionsResponse, ServerMessage};
use om_core::approval::{ApprovalDecision, ApprovalOptionKind};
use om_core::auth::Principal;
use om_core::content::{ContentBlock, Prompt};
use om_core::event::{EventPayload, SessionEvent};
use om_core::session::SessionSummary;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Url};
use serde_json::json;
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{
    AUTHORIZATION as WS_AUTHORIZATION, HeaderValue as WsHeaderValue,
};

const MAX_TRANSCRIPT_LINES: usize = 400;
const MAX_LINE_CHARS: usize = 4096;

#[derive(Args)]
pub struct TuiArgs {
    /// Central `om-server` base URL.
    #[arg(long, env = "OM_SERVER_URL", default_value = "http://127.0.0.1:4939")]
    pub server: String,

    /// Bearer access token (`omt_...` device token or static bootstrap token).
    /// Prefer `OM_TOKEN` env var or `--token-file` over passing secrets on argv.
    #[arg(long, env = "OM_TOKEN", default_value = "")]
    pub token: String,

    /// Optional path to a file containing the bearer token (trimmed; used when `--token` / `OM_TOKEN` is empty).
    #[arg(long)]
    pub token_file: Option<PathBuf>,
}

impl fmt::Debug for TuiArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TuiArgs")
            .field("server", &self.server)
            .field(
                "token",
                if self.token.trim().is_empty() {
                    &""
                } else {
                    &"<redacted>"
                },
            )
            .field("token_file", &self.token_file)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct PendingApprovalSummary {
    pub id: String,
    pub title: String,
    pub allow_option: String,
    pub allow_label: String,
    pub reject_option: String,
    pub reject_label: String,
}

enum WatcherEvent {
    Session(Box<SessionEvent>),
    Status(String),
}

/// Pure UI state machine (testable headlessly via `ratatui::backend::TestBackend`).
#[derive(Debug, Clone)]
pub struct TuiState {
    pub server_url: String,
    pub principal: Option<Principal>,
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
    pub transcript: Vec<String>,
    pub pending_approvals: VecDeque<PendingApprovalSummary>,
    pub last_seq: u64,
    pub input_mode: bool,
    pub input_buffer: String,
    pub status_line: String,
}

impl TuiState {
    pub fn new(server_url: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            principal: None,
            sessions: Vec::new(),
            selected: 0,
            transcript: Vec::new(),
            pending_approvals: VecDeque::new(),
            last_seq: 0,
            input_mode: false,
            input_buffer: String::new(),
            status_line: "已连接 · 按 j/k 切换会话 · i 发指令 · t 直连 PTY · q 退出".into(),
        }
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected)
    }

    pub fn current_approval(&self) -> Option<&PendingApprovalSummary> {
        self.pending_approvals.front()
    }

    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = if self.selected == 0 {
                self.sessions.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn reset_stream_view(&mut self) {
        self.transcript.clear();
        self.pending_approvals.clear();
        self.last_seq = 0;
    }

    fn push_line(&mut self, mut line: String) {
        if line.chars().count() > MAX_LINE_CHARS {
            line = line.chars().take(MAX_LINE_CHARS).collect::<String>();
            line.push('…');
        }
        self.transcript.push(line);
        if self.transcript.len() > MAX_TRANSCRIPT_LINES {
            let drain = self.transcript.len() - MAX_TRANSCRIPT_LINES;
            self.transcript.drain(..drain);
        }
    }

    pub fn apply_event(&mut self, ev: &SessionEvent) {
        // Always update session list status/head_seq if matching.
        if let EventPayload::StatusChanged { to, .. } = &ev.payload
            && let Some(sess) = self.sessions.iter_mut().find(|s| s.id == ev.session_id)
        {
            sess.status = *to;
            sess.head_seq = sess.head_seq.max(ev.seq);
        }

        // Ignore transcript/approval events from sessions other than the active selection.
        if let Some(selected) = self.selected_session()
            && selected.id != ev.session_id
        {
            return;
        }
        if ev.seq <= self.last_seq {
            return;
        }
        self.last_seq = ev.seq;

        match &ev.payload {
            EventPayload::PromptSubmitted { prompt, .. } => {
                let txt = render_blocks(&prompt.content);
                self.push_line(format!("[#{}] > {txt}", ev.seq));
            }
            EventPayload::AgentMessageChunk { content, .. } => {
                let txt = render_block(content);
                if let Some(last) = self.transcript.last_mut()
                    && last.starts_with("Agent: ")
                    && last.chars().count() < MAX_LINE_CHARS
                {
                    let remaining = MAX_LINE_CHARS.saturating_sub(last.chars().count());
                    last.extend(txt.chars().take(remaining));
                } else {
                    self.push_line(format!("Agent: {txt}"));
                }
            }
            EventPayload::ToolCall { tool_call, .. } => {
                self.push_line(format!(
                    "[#{}] [Tool:{:?}] {}",
                    ev.seq, tool_call.kind, tool_call.title
                ));
            }
            EventPayload::ApprovalRequested { approval, .. } => {
                // Strictly prefer one-shot AllowOnce / RejectOnce so `[a]` never silently
                // persists an AllowAlways rule under an "允许一次" prompt.
                let allow_choice = approval
                    .options
                    .iter()
                    .find(|o| o.kind == ApprovalOptionKind::AllowOnce)
                    .or_else(|| approval.options.iter().find(|o| o.kind.is_allow()));
                let reject_choice = approval
                    .options
                    .iter()
                    .find(|o| o.kind == ApprovalOptionKind::RejectOnce)
                    .or_else(|| approval.options.iter().find(|o| !o.kind.is_allow()));
                let (allow_option, allow_label) = allow_choice
                    .map(|o| (o.option_id.clone(), o.name.clone()))
                    .unwrap_or_else(|| ("allow_once".into(), "允许一次".into()));
                let (reject_option, reject_label) = reject_choice
                    .map(|o| (o.option_id.clone(), o.name.clone()))
                    .unwrap_or_else(|| ("reject_once".into(), "拒绝".into()));
                if !self
                    .pending_approvals
                    .iter()
                    .any(|p| p.id == approval.id.as_str())
                {
                    self.pending_approvals.push_back(PendingApprovalSummary {
                        id: approval.id.to_string(),
                        title: approval.title.clone(),
                        allow_option,
                        allow_label,
                        reject_option,
                        reject_label,
                    });
                }
                self.push_line(format!("[#{}] [审批请求] {}", ev.seq, approval.title));
            }
            EventPayload::ApprovalResolved {
                approval_id,
                decision,
                ..
            } => {
                self.pending_approvals
                    .retain(|p| p.id != approval_id.as_str());
                self.push_line(format!("[#{}] [审批完成] {decision:?}", ev.seq));
            }
            EventPayload::TurnEnded { stop_reason, .. } => {
                self.push_line(format!(
                    "[#{}] --- Turn ended ({stop_reason:?}) ---",
                    ev.seq
                ));
            }
            EventPayload::Error { message } => {
                self.push_line(format!("[#{}] [ERROR] {message}", ev.seq));
            }
            _ => {}
        }
    }
}

fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::ResourceLink { name, uri, .. } => format!("[{name}]({uri})"),
    }
}

fn render_blocks(blocks: &[ContentBlock]) -> String {
    blocks.iter().map(render_block).collect::<Vec<_>>().join("")
}

/// Renders the full Ratatui dashboard frame.
pub fn render_tui(frame: &mut Frame<'_>, state: &TuiState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(frame.area());

    // Header
    let who = state
        .principal
        .as_ref()
        .map(|p| format!("{} ({:?})", p.display_name, p.role))
        .unwrap_or_else(|| "connecting...".into());
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " OpenMirasim TUI ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  |  {}  |  {} 会话",
            state.server_url,
            who,
            state.sessions.len()
        )),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, root[0]);

    // Main split: left session list, right event transcript + approval banner
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(root[1]);

    let items: Vec<ListItem> = if state.sessions.is_empty() {
        vec![ListItem::new("（暂无会话）")]
    } else {
        state
            .sessions
            .iter()
            .map(|s| {
                let title = s
                    .config
                    .title
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or(s.config.harness_id.as_str());
                ListItem::new(format!("{title} [{:?}] #{}", s.status, s.head_seq))
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" 会话列表 (j/k) ")
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut list_state = ListState::default();
    if !state.sessions.is_empty() {
        list_state.select(Some(state.selected.min(state.sessions.len() - 1)));
    }
    frame.render_stateful_widget(list, body[0], &mut list_state);

    let current_app = state.current_approval();
    let right_chunks = if current_app.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Min(4)])
            .split(body[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(0), Constraint::Min(4)])
            .split(body[1])
    };

    if let Some(app) = current_app {
        let queue_note = if state.pending_approvals.len() > 1 {
            format!(" (+{} 待处理)", state.pending_approvals.len() - 1)
        } else {
            String::new()
        };
        let banner = Paragraph::new(format!(
            "待审批{queue_note}: {}  |  按 [a] {} ({})  按 [x] {} ({})",
            app.title, app.allow_label, app.allow_option, app.reject_label, app.reject_option
        ))
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default().title(" 审批提醒 ").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
        frame.render_widget(banner, right_chunks[0]);
    }

    let title = state
        .selected_session()
        .map(|s| format!(" 事件流 · {} ({}) ", s.config.harness_id, s.config.cwd))
        .unwrap_or_else(|| " 事件流 ".into());
    let max_lines = right_chunks[1].height.saturating_sub(2) as usize;
    let visible = if state.transcript.len() > max_lines && max_lines > 0 {
        &state.transcript[state.transcript.len() - max_lines..]
    } else {
        &state.transcript[..]
    };
    let stream_text: Vec<Line> = visible.iter().map(|l| Line::from(l.as_str())).collect();
    let stream = Paragraph::new(stream_text)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(stream, right_chunks[1]);

    // Prompt input box
    let input_title = if state.input_mode {
        " 发送指令 [输入中 · Enter 提交 · Esc 取消] "
    } else {
        " 发送指令 [按 i 输入] "
    };
    let input_style = if state.input_mode {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let input = Paragraph::new(state.input_buffer.as_str())
        .style(input_style)
        .block(Block::default().title(input_title).borders(Borders::ALL));
    frame.render_widget(input, root[2]);

    // Footer status / keybindings
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        &state.status_line,
        Style::default().fg(Color::Gray),
    )]));
    frame.render_widget(footer, root[3]);
}

fn validate_id<'a>(label: &str, value: &'a str) -> Result<&'a str, String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '\0') || c.is_control())
    {
        return Err(format!("invalid {label}"));
    }
    Ok(value)
}

fn parse_base_url(server: &str) -> Result<Url, String> {
    let url = Url::parse(server.trim()).map_err(|e| format!("invalid server URL: {e}"))?;
    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => Ok(url),
        _ => Err("server URL must use http:// or https:// with a valid host".into()),
    }
}

fn resolve_token(inline: &str, token_file: Option<&Path>) -> Result<String, String> {
    let trimmed = inline.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }
    if let Some(path) = token_file {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read --token-file {}: {e}", path.display()))?;
        return Ok(content.trim().to_string());
    }
    Ok(String::new())
}

fn build_http_url(base: &Url, segments: &[&str]) -> Result<Url, String> {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|()| "server URL cannot be a base".to_string())?;
        segs.pop_if_empty();
        for s in segments {
            segs.push(s);
        }
    }
    Ok(url)
}

fn build_ws_url(base: &Url, segments: &[&str], query: &[(&str, String)]) -> Result<Url, String> {
    let mut url = build_http_url(base, segments)?;
    let ws_scheme = match base.scheme() {
        "https" => "wss",
        _ => "ws",
    };
    url.set_scheme(ws_scheme)
        .map_err(|()| "failed to derive WebSocket scheme".to_string())?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
}

/// Builds a WebSocket upgrade request carrying the bearer credential exclusively in the
/// `Authorization` HTTP header (never in URL query parameters).
fn build_ws_request(
    url: &Url,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, String> {
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("invalid websocket request: {e}"))?;
    let trimmed = token.trim();
    if !trimmed.is_empty() {
        let header_val = WsHeaderValue::from_str(&format!("Bearer {trimmed}"))
            .map_err(|_| "token contains invalid HTTP header characters".to_string())?;
        req.headers_mut().insert(WS_AUTHORIZATION, header_val);
    }
    Ok(req)
}

async fn fetch_me(http: &Client, base: &Url, token: &str) -> Result<Principal, String> {
    let url = build_http_url(base, &["api", "v1", "me"])?;
    let res = http
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("GET /api/v1/me failed: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("authentication failed: HTTP {}", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

async fn fetch_sessions(
    http: &Client,
    base: &Url,
    token: &str,
) -> Result<Vec<SessionSummary>, String> {
    let url = build_http_url(base, &["api", "v1", "sessions"])?;
    let res = http
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("GET /api/v1/sessions failed: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("list sessions failed: HTTP {}", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    let parsed: ListSessionsResponse = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(parsed.sessions)
}

fn spawn_event_watcher(
    base: Url,
    token: String,
    session_id: String,
    tx: mpsc::UnboundedSender<WatcherEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_seq: u64 = 0;
        let mut backoff_ms: u64 = 500;

        loop {
            let url = match build_ws_url(&base, &["ws", "events"], &[]) {
                Ok(u) => u,
                Err(e) => {
                    let _ = tx.send(WatcherEvent::Status(format!("事件流地址错误: {e}")));
                    return;
                }
            };
            let req = match build_ws_request(&url, &token) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(WatcherEvent::Status(format!("事件流认证配置错误: {e}")));
                    return;
                }
            };

            let (mut ws, _) = match tokio_tungstenite::connect_async(req).await {
                Ok(conn) => {
                    backoff_ms = 500;
                    conn
                }
                Err(e) => {
                    let _ = tx.send(WatcherEvent::Status(format!(
                        "事件流连接失败 ({e})，{backoff_ms}ms 后重连…"
                    )));
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(10_000);
                    continue;
                }
            };

            let hello = serde_json::to_string(&ClientMessage::Hello {
                protocol_version: 1,
            })
            .unwrap();
            if ws.send(WsMessage::Text(hello.into())).await.is_err() {
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }

            let mut fatal = false;
            while let Some(frame_res) = ws.next().await {
                let Ok(frame) = frame_res else {
                    break;
                };
                if let WsMessage::Text(txt) = frame
                    && let Ok(msg) = serde_json::from_str::<ServerMessage>(&txt)
                {
                    match msg {
                        ServerMessage::Welcome { .. } => {
                            let sub = serde_json::to_string(&ClientMessage::Subscribe {
                                session_id: session_id.clone().into(),
                                last_seq,
                            })
                            .unwrap();
                            if ws.send(WsMessage::Text(sub.into())).await.is_err() {
                                break;
                            }
                        }
                        ServerMessage::Subscribed { .. } => {
                            let _ = tx.send(WatcherEvent::Status(
                                "事件流已同步 · j/k 切换 · i 发指令 · t 直连 PTY · q 退出".into(),
                            ));
                        }
                        ServerMessage::Event { event } => {
                            if event.session_id.as_str() == session_id {
                                last_seq = last_seq.max(event.seq);
                                if tx.send(WatcherEvent::Session(event)).is_err() {
                                    return;
                                }
                            }
                        }
                        ServerMessage::Resync {
                            session_id: sid, ..
                        } if sid.as_str() == session_id => {
                            let _ = tx.send(WatcherEvent::Status(
                                "检测到慢消费，正在按 last_seq 重新同步…".into(),
                            ));
                            let sub = serde_json::to_string(&ClientMessage::Subscribe {
                                session_id: session_id.clone().into(),
                                last_seq,
                            })
                            .unwrap();
                            if ws.send(WsMessage::Text(sub.into())).await.is_err() {
                                break;
                            }
                        }
                        ServerMessage::Error {
                            session_id: err_sid,
                            error,
                        } => {
                            if err_sid
                                .as_ref()
                                .is_some_and(|sid| sid.as_str() != session_id)
                            {
                                continue;
                            }
                            let code = format!("{:?}", error.code).to_lowercase();
                            let _ = tx.send(WatcherEvent::Status(format!(
                                "事件流错误 [{code}]: {}",
                                error.message
                            )));
                            if matches!(
                                code.as_str(),
                                "unauthorized" | "forbidden" | "notfound" | "unsupported"
                            ) {
                                fatal = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }

            if fatal || tx.is_closed() {
                return;
            }
            let _ = tx.send(WatcherEvent::Status(format!(
                "事件流已断开，{backoff_ms}ms 后自动重连…"
            )));
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(10_000);
        }
    })
}

/// Attaches the user's raw TTY directly to `/ws/pty/{session_id}` (detach with `Ctrl+]`).
async fn attach_pty_interactive(
    base: &Url,
    token: &str,
    session_id: &str,
) -> Result<String, String> {
    let sid = validate_id("session_id", session_id)?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let url = build_ws_url(
        base,
        &["ws", "pty", sid],
        &[("cols", cols.to_string()), ("rows", rows.to_string())],
    )?;
    let req = build_ws_request(&url, token)?;
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("failed to connect /ws/pty/{sid}: {e}"))?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut stdout = io::stdout();
    let _ = write!(
        stdout,
        "\r\n[OpenMirasim PTY attached to {sid} · press Ctrl+] to detach]\r\n"
    );
    let _ = stdout.flush();

    let (input_tx, mut input_rx) = mpsc::channel::<WsMessage>(64);
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_reader = stop_flag.clone();

    let input_thread = std::thread::spawn(move || {
        while !stop_reader.load(std::sync::atomic::Ordering::Relaxed) {
            if event::poll(Duration::from_millis(40)).unwrap_or(false)
                && let Ok(ev) = event::read()
            {
                match ev {
                    Event::Key(KeyEvent {
                        code: KeyCode::Char(']'),
                        modifiers,
                        kind: KeyEventKind::Press,
                        ..
                    }) if modifiers.contains(KeyModifiers::CONTROL) => {
                        stop_reader.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(bytes) = encode_key(key)
                            && input_tx
                                .blocking_send(WsMessage::Binary(Bytes::from(bytes)))
                                .is_err()
                        {
                            break;
                        }
                    }
                    Event::Resize(c, r) => {
                        let msg = json!({"type": "resize", "cols": c, "rows": r}).to_string();
                        if input_tx.blocking_send(WsMessage::Text(msg.into())).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    let mut exit_reason = "detached from PTY (Ctrl+])".to_string();
    loop {
        tokio::select! {
            msg = input_rx.recv() => {
                match msg {
                    Some(msg) => {
                        if ws_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let _ = stdout.write_all(&data);
                        let _ = stdout.flush();
                    }
                    Some(Ok(WsMessage::Text(txt))) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                            && v["type"] == "exited"
                        {
                            exit_reason = format!("PTY exited (code={})", v["code"]);
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        exit_reason = "PTY stream closed".into();
                        break;
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
        }
    }

    // Signal stop and drop `input_rx` BEFORE joining `input_thread` so any blocked
    // `input_tx.blocking_send(...)` unblocks immediately rather than deadlocking.
    stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(input_rx);
    let _ = ws_tx.send(WsMessage::Close(None)).await;
    let _ = input_thread.join();
    Ok(exit_reason)
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
        && c.is_ascii()
    {
        let upper = c.to_ascii_uppercase() as u8;
        if upper.is_ascii_uppercase() {
            return Some(vec![upper - b'@']);
        }
    }
    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}

/// RAII guard restoring terminal raw mode and alternate screen on exit or panic.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| format!("enable raw mode failed: {e}"))?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e.to_string());
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(e.to_string())
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

pub async fn run(args: TuiArgs) -> Result<(), String> {
    let base_url = parse_base_url(&args.server)?;
    let token = resolve_token(&args.token, args.token_file.as_deref())?;
    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    let mut state = TuiState::new(base_url.as_str().trim_end_matches('/'));
    state.principal = Some(fetch_me(&http, &base_url, &token).await?);
    state.sessions = fetch_sessions(&http, &base_url, &token).await?;

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev_hook(info);
    }));

    let mut guard = TerminalGuard::enter()?;

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<WatcherEvent>();
    let mut watcher: Option<tokio::task::JoinHandle<()>> = state.selected_session().map(|s| {
        spawn_event_watcher(
            base_url.clone(),
            token.clone(),
            s.id.to_string(),
            ev_tx.clone(),
        )
    });

    let res = 'tui: loop {
        while let Ok(msg) = ev_rx.try_recv() {
            match msg {
                WatcherEvent::Session(ev) => state.apply_event(&ev),
                WatcherEvent::Status(line) => state.status_line = line,
            }
        }

        if let Err(e) = guard.terminal.draw(|f| render_tui(f, &state)) {
            break 'tui Err(e.to_string());
        }

        if event::poll(Duration::from_millis(60)).unwrap_or(false)
            && let Ok(Event::Key(key)) = event::read()
            && key.kind == KeyEventKind::Press
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break 'tui Ok(());
            }

            if state.input_mode {
                match key.code {
                    KeyCode::Esc => {
                        state.input_mode = false;
                    }
                    KeyCode::Backspace => {
                        state.input_buffer.pop();
                    }
                    KeyCode::Char(c) => {
                        state.input_buffer.push(c);
                    }
                    KeyCode::Enter => {
                        let prompt_text = state.input_buffer.trim().to_string();
                        state.input_buffer.clear();
                        state.input_mode = false;
                        if !prompt_text.is_empty()
                            && let Some(sid) = state.selected_session().map(|s| s.id.to_string())
                        {
                            match validate_id("session_id", &sid).and_then(|valid_sid| {
                                build_http_url(
                                    &base_url,
                                    &["api", "v1", "sessions", valid_sid, "prompts"],
                                )
                            }) {
                                Ok(url) => {
                                    let body = serde_json::to_vec(&json!({
                                        "prompt": Prompt::text(prompt_text),
                                        "mode": "queue",
                                    }))
                                    .unwrap();
                                    match http
                                        .post(url)
                                        .header(AUTHORIZATION, format!("Bearer {token}"))
                                        .header(CONTENT_TYPE, "application/json")
                                        .body(body)
                                        .send()
                                        .await
                                    {
                                        Ok(r) if r.status().is_success() => {
                                            state.status_line = "指令已发送".into();
                                        }
                                        Ok(r) => {
                                            state.status_line =
                                                format!("发送失败: HTTP {}", r.status());
                                        }
                                        Err(e) => {
                                            state.status_line = format!("发送失败: {e}");
                                        }
                                    }
                                }
                                Err(e) => {
                                    state.status_line = format!("无效会话 ID: {e}");
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') => break 'tui Ok(()),
                KeyCode::Char('j') | KeyCode::Down => {
                    state.select_next();
                    if let Some(h) = watcher.take() {
                        h.abort();
                    }
                    while ev_rx.try_recv().is_ok() {}
                    state.reset_stream_view();
                    if let Some(s) = state.selected_session() {
                        watcher = Some(spawn_event_watcher(
                            base_url.clone(),
                            token.clone(),
                            s.id.to_string(),
                            ev_tx.clone(),
                        ));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    state.select_prev();
                    if let Some(h) = watcher.take() {
                        h.abort();
                    }
                    while ev_rx.try_recv().is_ok() {}
                    state.reset_stream_view();
                    if let Some(s) = state.selected_session() {
                        watcher = Some(spawn_event_watcher(
                            base_url.clone(),
                            token.clone(),
                            s.id.to_string(),
                            ev_tx.clone(),
                        ));
                    }
                }
                KeyCode::Char('i') => {
                    state.input_mode = true;
                }
                KeyCode::Char('r') => match fetch_sessions(&http, &base_url, &token).await {
                    Ok(list) => {
                        state.sessions = list;
                        if state.selected >= state.sessions.len() && !state.sessions.is_empty() {
                            state.selected = state.sessions.len() - 1;
                        }
                        state.status_line = "已刷新会话列表".into();
                    }
                    Err(e) => {
                        state.status_line = format!("刷新会话列表失败: {e}");
                    }
                },
                KeyCode::Char('a') | KeyCode::Char('x') => {
                    if let Some(app) = state.current_approval().cloned() {
                        let opt = if key.code == KeyCode::Char('a') {
                            app.allow_option
                        } else {
                            app.reject_option
                        };
                        match validate_id("approval_id", &app.id).and_then(|aid| {
                            build_http_url(&base_url, &["api", "v1", "approvals", aid, "resolve"])
                        }) {
                            Ok(url) => {
                                let decision = ApprovalDecision::Selected { option_id: opt };
                                let body =
                                    serde_json::to_vec(&json!({ "decision": decision })).unwrap();
                                match http
                                    .post(url)
                                    .header(AUTHORIZATION, format!("Bearer {token}"))
                                    .header(CONTENT_TYPE, "application/json")
                                    .body(body)
                                    .send()
                                    .await
                                {
                                    Ok(r) if r.status().is_success() => {
                                        state.status_line = "审批裁决已提交".into();
                                    }
                                    Ok(r) => {
                                        state.status_line =
                                            format!("审批提交失败: HTTP {}", r.status());
                                    }
                                    Err(e) => {
                                        state.status_line = format!("审批提交失败: {e}");
                                    }
                                }
                            }
                            Err(e) => {
                                state.status_line = format!("无效审批 ID: {e}");
                            }
                        }
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(sid) = state.selected_session().map(|s| s.id.to_string()) {
                        let _ = execute!(guard.terminal.backend_mut(), LeaveAlternateScreen);
                        match attach_pty_interactive(&base_url, &token, &sid).await {
                            Ok(reason) => state.status_line = reason,
                            Err(e) => state.status_line = format!("PTY 连接失败: {e}"),
                        }
                        let _ = execute!(guard.terminal.backend_mut(), EnterAlternateScreen);
                        let _ = guard.terminal.clear();
                    }
                }
                _ => {}
            }
        }
    };

    if let Some(h) = watcher {
        h.abort();
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use om_core::approval::{ApprovalOption, ApprovalRecord, ApprovalStatus};
    use om_core::ids::{ApprovalId, SessionId, SpaceId, TurnId};
    use om_core::sandbox::SandboxMode;
    use om_core::session::{SessionConfig, SessionStatus};
    use ratatui::backend::TestBackend;

    #[test]
    fn ratatui_dashboard_renders_sessions_events_and_approval_banner() {
        let mut state = TuiState::new("http://127.0.0.1:4939");
        let sid = SessionId::from("sess-tui-1");
        state.sessions.push(SessionSummary {
            id: sid.clone(),
            space_id: SpaceId::from("space-1"),
            config: SessionConfig {
                harness_id: "opencode".into(),
                cwd: "/srv/repo".into(),
                workspace_id: None,
                model: None,
                sandbox: SandboxMode::Bwrap,
                title: Some("Phase 3 TUI Session".into()),
            },
            status: SessionStatus::Idle,
            head_seq: 2,
            native_session_id: None,
            created_at: 0,
            updated_at: 0,
        });
        state.apply_event(&SessionEvent {
            session_id: sid.clone(),
            seq: 1,
            ts: 0,
            payload: EventPayload::AgentMessageChunk {
                turn_id: TurnId::from("t1"),
                content: ContentBlock::Text {
                    text: "hello from ratatui".into(),
                },
            },
        });

        // Event from another session must not pollute the selected session transcript.
        state.apply_event(&SessionEvent {
            session_id: SessionId::from("sess-other"),
            seq: 2,
            ts: 0,
            payload: EventPayload::AgentMessageChunk {
                turn_id: TurnId::from("t2"),
                content: ContentBlock::Text {
                    text: "FOREIGN_LEAK".into(),
                },
            },
        });

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_tui(f, &state)).unwrap();

        let buf = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                rendered.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(rendered.contains("OpenMirasim TUI"));
        assert!(rendered.contains("Phase 3 TUI Session"));
        assert!(rendered.contains("hello from ratatui"));
        assert!(!rendered.contains("FOREIGN_LEAK"));
    }

    #[test]
    fn approval_prefers_allow_once_over_allow_always_and_queues() {
        let mut state = TuiState::new("http://127.0.0.1:4939");
        let sid = SessionId::from("sess-1");
        state.apply_event(&SessionEvent {
            session_id: sid.clone(),
            seq: 1,
            ts: 0,
            payload: EventPayload::ApprovalRequested {
                turn_id: Some(TurnId::from("t1")),
                approval: ApprovalRecord {
                    id: ApprovalId::from("appr-1"),
                    session_id: sid.clone(),
                    tool_call_id: "tc-1".into(),
                    tool_kind: None,
                    locations: vec![],
                    title: "Run cargo test".into(),
                    options: vec![
                        ApprovalOption {
                            option_id: "always".into(),
                            name: "始终允许".into(),
                            kind: ApprovalOptionKind::AllowAlways,
                        },
                        ApprovalOption {
                            option_id: "once".into(),
                            name: "允许一次".into(),
                            kind: ApprovalOptionKind::AllowOnce,
                        },
                        ApprovalOption {
                            option_id: "deny".into(),
                            name: "拒绝一次".into(),
                            kind: ApprovalOptionKind::RejectOnce,
                        },
                    ],
                    status: ApprovalStatus::Pending,
                    decision: None,
                    resolved_by: None,
                    auto_rule: None,
                    created_at: 0,
                    resolved_at: None,
                },
            },
        });
        let cur = state.current_approval().unwrap();
        assert_eq!(cur.allow_option, "once");
        assert_eq!(cur.reject_option, "deny");
    }

    #[test]
    fn ws_request_uses_bearer_header_and_encode_key_guards_non_ascii() {
        let base = parse_base_url("https://om.internal:4939").unwrap();
        let url = build_ws_url(&base, &["ws", "events"], &[]).unwrap();
        assert_eq!(url.as_str(), "wss://om.internal:4939/ws/events");
        let req = build_ws_request(&url, " omt_secret_token ").unwrap();
        assert!(!req.uri().to_string().contains("omt_secret_token"));
        assert_eq!(
            req.headers().get(WS_AUTHORIZATION).unwrap(),
            "Bearer omt_secret_token"
        );

        // Non-ASCII character with Ctrl modifier must NOT truncate to ASCII control byte (e.g., Ctrl+C).
        let non_ascii = KeyEvent::new(KeyCode::Char('\u{0143}'), KeyModifiers::CONTROL);
        assert_ne!(encode_key(non_ascii), Some(vec![0x03]));
    }
}
