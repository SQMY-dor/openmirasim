//! Ratatui terminal dashboard and direct `/ws/pty` attach client (`om-cli tui`, Phase 3).
//!
//! Designed for headless Linux servers: inspect all team sessions, watch live `/ws/events`
//! streams, approve pending tool calls, submit prompts, and press `t` to attach directly
//! to a session's native binary PTY stream (detach with `Ctrl+]`).

use bytes::Bytes;
use clap::Args;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::{SinkExt, StreamExt};
use om_core::api::{ClientMessage, ListSessionsResponse, ServerMessage};
use om_core::approval::ApprovalDecision;
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
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::json;
use std::io::{self, Write};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Debug, Args)]
pub struct TuiArgs {
    /// Central `om-server` base URL.
    #[arg(long, env = "OM_SERVER_URL", default_value = "http://127.0.0.1:4939")]
    pub server: String,

    /// Bearer access token (`omt_...` device token or static bootstrap token).
    #[arg(long, env = "OM_TOKEN", default_value = "")]
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct PendingApprovalSummary {
    pub id: String,
    pub title: String,
    pub allow_option: String,
    pub reject_option: String,
}

/// Pure UI state machine (testable headlessly via `ratatui::backend::TestBackend`).
#[derive(Debug, Clone)]
pub struct TuiState {
    pub server_url: String,
    pub principal: Option<Principal>,
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
    pub transcript: Vec<String>,
    pub pending_approval: Option<PendingApprovalSummary>,
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
            pending_approval: None,
            input_mode: false,
            input_buffer: String::new(),
            status_line: "已连接 · 按 j/k 切换会话 · i 发指令 · t 直连 PTY · q 退出".into(),
        }
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected)
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
        self.pending_approval = None;
    }

    pub fn apply_event(&mut self, ev: &SessionEvent) {
        match &ev.payload {
            EventPayload::PromptSubmitted { prompt, .. } => {
                let txt = render_blocks(&prompt.content);
                self.transcript.push(format!("[#{}] > {txt}", ev.seq));
            }
            EventPayload::AgentMessageChunk { content, .. } => {
                let txt = render_block(content);
                if let Some(last) = self.transcript.last_mut()
                    && last.starts_with("Agent: ")
                {
                    last.push_str(&txt);
                } else {
                    self.transcript.push(format!("Agent: {txt}"));
                }
            }
            EventPayload::ToolCall { tool_call, .. } => {
                self.transcript.push(format!(
                    "[#{}] [Tool:{:?}] {}",
                    ev.seq, tool_call.kind, tool_call.title
                ));
            }
            EventPayload::ApprovalRequested { approval, .. } => {
                let allow_opt = approval
                    .options
                    .iter()
                    .find(|o| o.kind.is_allow())
                    .map(|o| o.option_id.clone())
                    .unwrap_or_else(|| "allow_once".into());
                let reject_opt = approval
                    .options
                    .iter()
                    .find(|o| !o.kind.is_allow())
                    .map(|o| o.option_id.clone())
                    .unwrap_or_else(|| "reject_once".into());
                self.pending_approval = Some(PendingApprovalSummary {
                    id: approval.id.to_string(),
                    title: approval.title.clone(),
                    allow_option: allow_opt,
                    reject_option: reject_opt,
                });
                self.transcript
                    .push(format!("[#{}] [审批请求] {}", ev.seq, approval.title));
            }
            EventPayload::ApprovalResolved {
                approval_id,
                decision,
                ..
            } => {
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|p| p.id == approval_id.as_str())
                {
                    self.pending_approval = None;
                }
                self.transcript
                    .push(format!("[#{}] [审批完成] {decision:?}", ev.seq));
            }
            EventPayload::TurnEnded { stop_reason, .. } => {
                self.transcript.push(format!(
                    "[#{}] --- Turn ended ({stop_reason:?}) ---",
                    ev.seq
                ));
            }
            EventPayload::StatusChanged { to, .. } => {
                if let Some(sess) = self.sessions.iter_mut().find(|s| s.id == ev.session_id) {
                    sess.status = *to;
                    sess.head_seq = sess.head_seq.max(ev.seq);
                }
            }
            EventPayload::Error { message } => {
                self.transcript
                    .push(format!("[#{}] [ERROR] {message}", ev.seq));
            }
            _ => {}
        }
        if self.transcript.len() > 400 {
            let drain = self.transcript.len() - 400;
            self.transcript.drain(..drain);
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

    let right_chunks = if state.pending_approval.is_some() {
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

    if let Some(app) = &state.pending_approval {
        let banner = Paragraph::new(format!(
            "待审批: {}  |  按 [a] 允许一次 ({})  按 [x] 拒绝 ({})",
            app.title, app.allow_option, app.reject_option
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

fn ws_url(base_http: &str, path: &str, token: &str) -> String {
    let base = base_http.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{base}")
    };
    if path.contains('?') {
        format!("{ws_base}{path}&token={token}")
    } else {
        format!("{ws_base}{path}?token={token}")
    }
}

async fn fetch_me(http: &Client, base: &str, token: &str) -> Result<Principal, String> {
    let res = http
        .get(format!("{}/api/v1/me", base.trim_end_matches('/')))
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
    base: &str,
    token: &str,
) -> Result<Vec<SessionSummary>, String> {
    let res = http
        .get(format!("{}/api/v1/sessions", base.trim_end_matches('/')))
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
    base: String,
    token: String,
    session_id: String,
    tx: mpsc::UnboundedSender<SessionEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let url = ws_url(&base, "/ws/events", &token);
        let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&url).await else {
            return;
        };
        let hello = serde_json::to_string(&ClientMessage::Hello {
            protocol_version: 1,
        })
        .unwrap();
        if ws.send(WsMessage::Text(hello.into())).await.is_err() {
            return;
        }
        while let Some(Ok(frame)) = ws.next().await {
            if let WsMessage::Text(txt) = frame
                && let Ok(msg) = serde_json::from_str::<ServerMessage>(&txt)
            {
                match msg {
                    ServerMessage::Welcome { .. } => {
                        let sub = serde_json::to_string(&ClientMessage::Subscribe {
                            session_id: session_id.clone().into(),
                            last_seq: 0,
                        })
                        .unwrap();
                        if ws.send(WsMessage::Text(sub.into())).await.is_err() {
                            break;
                        }
                    }
                    ServerMessage::Event { event } => {
                        if tx.send(*event).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    })
}

/// Attaches the user's raw TTY directly to `/ws/pty/{session_id}` (detach with `Ctrl+]`).
async fn attach_pty_interactive(
    base: &str,
    token: &str,
    session_id: &str,
) -> Result<String, String> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let url = ws_url(
        base,
        &format!("/ws/pty/{session_id}?cols={cols}&rows={rows}"),
        token,
    );
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| format!("failed to connect /ws/pty/{session_id}: {e}"))?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut stdout = io::stdout();
    let _ = write!(
        stdout,
        "\r\n[OpenMirasim PTY attached to {session_id} · press Ctrl+] to detach]\r\n"
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
                        ..
                    }) if modifiers.contains(KeyModifiers::CONTROL) => {
                        stop_reader.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    Event::Key(key) => {
                        if let Some(bytes) = encode_key(key) {
                            let _ = input_tx.blocking_send(WsMessage::Binary(Bytes::from(bytes)));
                        }
                    }
                    Event::Resize(c, r) => {
                        let msg = json!({"type": "resize", "cols": c, "rows": r}).to_string();
                        let _ = input_tx.blocking_send(WsMessage::Text(msg.into()));
                    }
                    _ => {}
                }
            }
        }
    });

    let mut exit_reason = "detached from PTY (Ctrl+])".to_string();
    loop {
        tokio::select! {
            Some(msg) = input_rx.recv() => {
                if ws_tx.send(msg).await.is_err() {
                    break;
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

    stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ws_tx.send(WsMessage::Close(None)).await;
    let _ = input_thread.join();
    Ok(exit_reason)
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
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

pub async fn run(args: TuiArgs) -> Result<(), String> {
    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let mut state = TuiState::new(&args.server);
    state.principal = Some(fetch_me(&http, &args.server, &args.token).await?);
    state.sessions = fetch_sessions(&http, &args.server, &args.token).await?;

    enable_raw_mode().map_err(|e| format!("enable raw mode failed: {e}"))?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.to_string());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            return Err(e.to_string());
        }
    };

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let mut watcher: Option<tokio::task::JoinHandle<()>> = state.selected_session().map(|s| {
        spawn_event_watcher(
            args.server.clone(),
            args.token.clone(),
            s.id.to_string(),
            ev_tx.clone(),
        )
    });

    let res = 'tui: loop {
        while let Ok(ev) = ev_rx.try_recv() {
            state.apply_event(&ev);
        }

        if let Err(e) = terminal.draw(|f| render_tui(f, &state)) {
            break 'tui Err(e.to_string());
        }

        if event::poll(Duration::from_millis(60)).unwrap_or(false)
            && let Ok(Event::Key(key)) = event::read()
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
                            let body = serde_json::to_vec(&json!({
                                "prompt": Prompt::text(prompt_text),
                                "mode": "queue",
                            }))
                            .unwrap();
                            match http
                                .post(format!(
                                    "{}/api/v1/sessions/{sid}/prompts",
                                    args.server.trim_end_matches('/')
                                ))
                                .header(AUTHORIZATION, format!("Bearer {}", args.token))
                                .header(CONTENT_TYPE, "application/json")
                                .body(body)
                                .send()
                                .await
                            {
                                Ok(r) if r.status().is_success() => {
                                    state.status_line = "指令已发送".into();
                                }
                                Ok(r) => {
                                    state.status_line = format!("发送失败: HTTP {}", r.status());
                                }
                                Err(e) => {
                                    state.status_line = format!("发送失败: {e}");
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
                    state.reset_stream_view();
                    if let Some(h) = watcher.take() {
                        h.abort();
                    }
                    if let Some(s) = state.selected_session() {
                        watcher = Some(spawn_event_watcher(
                            args.server.clone(),
                            args.token.clone(),
                            s.id.to_string(),
                            ev_tx.clone(),
                        ));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    state.select_prev();
                    state.reset_stream_view();
                    if let Some(h) = watcher.take() {
                        h.abort();
                    }
                    if let Some(s) = state.selected_session() {
                        watcher = Some(spawn_event_watcher(
                            args.server.clone(),
                            args.token.clone(),
                            s.id.to_string(),
                            ev_tx.clone(),
                        ));
                    }
                }
                KeyCode::Char('i') => {
                    state.input_mode = true;
                }
                KeyCode::Char('r') => {
                    if let Ok(list) = fetch_sessions(&http, &args.server, &args.token).await {
                        state.sessions = list;
                        if state.selected >= state.sessions.len() && !state.sessions.is_empty() {
                            state.selected = state.sessions.len() - 1;
                        }
                        state.status_line = "已刷新会话列表".into();
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('x') => {
                    if let Some(app) = state.pending_approval.clone() {
                        let opt = if key.code == KeyCode::Char('a') {
                            app.allow_option
                        } else {
                            app.reject_option
                        };
                        let decision = ApprovalDecision::Selected { option_id: opt };
                        let body = serde_json::to_vec(&json!({ "decision": decision })).unwrap();
                        let _ = http
                            .post(format!(
                                "{}/api/v1/approvals/{}/resolve",
                                args.server.trim_end_matches('/'),
                                app.id
                            ))
                            .header(AUTHORIZATION, format!("Bearer {}", args.token))
                            .header(CONTENT_TYPE, "application/json")
                            .body(body)
                            .send()
                            .await;
                        state.status_line = "审批裁决已提交".into();
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(sid) = state.selected_session().map(|s| s.id.to_string()) {
                        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
                        match attach_pty_interactive(&args.server, &args.token, &sid).await {
                            Ok(reason) => state.status_line = reason,
                            Err(e) => state.status_line = format!("PTY 连接失败: {e}"),
                        }
                        let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
                        let _ = terminal.clear();
                    }
                }
                _ => {}
            }
        }
    };

    if let Some(h) = watcher {
        h.abort();
    }
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use om_core::ids::{SessionId, SpaceId, TurnId};
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

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_tui(f, &state)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("OpenMirasim TUI"));
        assert!(rendered.contains("Phase 3 TUI Session"));
        assert!(rendered.contains("hello from ratatui"));
    }
}
