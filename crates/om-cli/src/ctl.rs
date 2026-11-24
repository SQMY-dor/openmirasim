//! Pure-JSON HTTP control plane client (`om-cli ctl`, W1-6).

use clap::{ArgAction, Args, Subcommand, ValueEnum};
use om_core::approval::ApprovalDecision;
use om_core::auth::Role;
use om_core::content::Prompt;
use om_core::ids::WorkspaceId;
use om_core::queue::SubmitMode;
use om_core::sandbox::SandboxMode;
use om_core::session::SessionConfig;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Args)]
pub struct CtlArgs {
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

    /// Pretty-print JSON output (default: compact single-line JSON for scripting/jq).
    #[arg(long, global = true)]
    pub pretty: bool,

    #[command(subcommand)]
    pub command: CtlCommand,
}

impl fmt::Debug for CtlArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CtlArgs")
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
            .field("pretty", &self.pretty)
            .field("command", &self.command)
            .finish()
    }
}

#[derive(Debug, Subcommand)]
pub enum CtlCommand {
    /// Check server health (`GET /healthz`).
    Health,
    /// Show the currently authenticated member principal (`GET /api/v1/me`).
    Me,
    /// Manage team members and roles (W2-1 RBAC).
    Members {
        #[command(subcommand)]
        action: MemberCmd,
    },
    /// Manage device tokens (W2-1 RBAC).
    Tokens {
        #[command(subcommand)]
        action: TokenCmd,
    },
    /// List registered harnesses (or probe live host binaries with `--probe`).
    Harnesses {
        #[arg(long)]
        probe: bool,
    },
    /// List configured KeyProxy providers (`GET /api/v1/providers`).
    Providers,
    /// Manage Git workspaces.
    Workspaces {
        #[command(subcommand)]
        action: WorkspaceCmd,
    },
    /// Manage sessions, events, worktrees, files, live Git diffs, proxy stats, and remote GUI screens.
    Sessions {
        #[command(subcommand)]
        action: SessionCmd,
    },
    /// Submit a prompt (supports Phase 0 strict mode or W2-3 `--mode queue|next|steer`).
    Prompt {
        session_id: String,
        text: String,
        #[arg(long, value_enum)]
        mode: Option<CliSubmitMode>,
    },
    /// Cancel the active turn on a session.
    Cancel { session_id: String },
    /// Inspect or manipulate a session's prompt queue (W2-3).
    Queue {
        #[command(subcommand)]
        action: QueueCmd,
    },
    /// Inspect/resolve approvals and remembered approval rules.
    Approvals {
        #[command(subcommand)]
        action: ApprovalCmd,
    },
    /// Query audit logs (`GET /api/v1/audit`, Admin/Owner only).
    Audit {
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Manage automated schedules (Phase 4).
    Schedules {
        #[command(subcommand)]
        action: ScheduleCmd,
    },
    /// Manage goal-driven agent loops (Phase 4).
    Goals {
        #[command(subcommand)]
        action: GoalCmd,
    },
    /// Manage Session-to-Session dispatches (Phase 4).
    Dispatches {
        #[command(subcommand)]
        action: DispatchCmd,
    },
    /// Manage Space / Workspace two-tier MCP Hub servers (Phase 4).
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// Manage Space / Workspace two-tier Skills lifecycle (Phase 4).
    Skills {
        #[command(subcommand)]
        action: SkillCmd,
    },
    /// Inspect effective inherited MCP Hub + Skills configuration (Phase 4).
    Hub {
        #[command(subcommand)]
        action: HubCmd,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliRole {
    Viewer,
    Member,
    Admin,
    Owner,
}

impl From<CliRole> for Role {
    fn from(v: CliRole) -> Self {
        match v {
            CliRole::Viewer => Role::Viewer,
            CliRole::Member => Role::Member,
            CliRole::Admin => Role::Admin,
            CliRole::Owner => Role::Owner,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum MemberCmd {
    /// List space members.
    List,
    /// Add a new member to the space (Admin/Owner).
    Add {
        #[arg(long)]
        name: String,
        #[arg(long, value_enum, default_value_t = CliRole::Member)]
        role: CliRole,
    },
    /// Change a member's role.
    SetRole {
        member_id: String,
        #[arg(long, value_enum)]
        role: CliRole,
    },
    /// Disable a member and revoke all of their tokens.
    Disable { member_id: String },
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// List device tokens for a member.
    List { member_id: String },
    /// Issue a new device token (secret is printed once in JSON output).
    Issue {
        member_id: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        ttl_secs: Option<u64>,
    },
    /// Revoke a device token immediately.
    Revoke { token_id: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliSandboxMode {
    Direct,
    Bwrap,
}

impl From<CliSandboxMode> for SandboxMode {
    fn from(v: CliSandboxMode) -> Self {
        match v {
            CliSandboxMode::Direct => SandboxMode::Direct,
            CliSandboxMode::Bwrap => SandboxMode::Bwrap,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliSubmitMode {
    Queue,
    Next,
    Steer,
}

impl From<CliSubmitMode> for SubmitMode {
    fn from(v: CliSubmitMode) -> Self {
        match v {
            CliSubmitMode::Queue => SubmitMode::Queue,
            CliSubmitMode::Next => SubmitMode::Next,
            CliSubmitMode::Steer => SubmitMode::Steer,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCmd {
    /// List registered workspaces in the active space.
    List,
    /// Get a single workspace by id.
    Get { id: String },
    /// Register an existing local Git repository as a workspace.
    Register {
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    /// List sessions.
    List,
    /// Get session summary by id.
    Get { id: String },
    /// Create a new session.
    Create {
        #[arg(long, default_value = "opencode")]
        harness: String,
        #[arg(long, default_value = "/tmp")]
        cwd: String,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum, default_value_t = CliSandboxMode::Bwrap)]
        sandbox: CliSandboxMode,
        #[arg(long)]
        title: Option<String>,
    },
    /// Resume a failed session.
    Resume { id: String },
    /// Close a session permanently.
    Close { id: String },
    /// List historical events after `after_seq`.
    Events {
        id: String,
        #[arg(long, default_value_t = 0)]
        after_seq: u64,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Get worktree metadata for a session.
    Worktree { id: String },
    /// Delete a session's on-disk worktree checkout (keeping its Git branch).
    RemoveWorktree { id: String },
    /// List files in a session's worktree.
    Files { id: String },
    /// Read a relative file inside a session's worktree.
    File { id: String, path: String },
    /// Get live Git diff for a session's worktree.
    Diff { id: String },
    /// Get KeyProxy request counters for a session.
    ProxyStats { id: String },
    /// Get remote GUI takeover screen info (for `antigravity-gui` sessions).
    RemoteScreen { id: String },
    /// Get native PTY status and `.cast` recording metadata for a session.
    Pty { id: String },
    /// Terminate a session's active native PTY (preserves `.cast` recording).
    KillPty { id: String },
}

#[derive(Debug, Subcommand)]
pub enum QueueCmd {
    /// List queued prompts on a session.
    List { session_id: String },
    /// Withdraw a queued prompt before it starts.
    Withdraw {
        session_id: String,
        queued_id: String,
    },
    /// Move a queued prompt to `position` (0 = front).
    Move {
        session_id: String,
        queued_id: String,
        position: u32,
    },
    /// Resume a paused queue.
    Resume { session_id: String },
}

#[derive(Debug, Subcommand)]
pub enum ApprovalCmd {
    /// List approvals for a session.
    List {
        session_id: String,
        #[arg(long)]
        pending: bool,
    },
    /// Resolve a pending approval by selecting an option or cancelling.
    Resolve {
        approval_id: String,
        #[arg(long, conflicts_with = "cancel")]
        option: Option<String>,
        #[arg(long)]
        cancel: bool,
    },
    /// List remembered always-allow / always-reject rules on a session.
    Rules { session_id: String },
    /// Clear all remembered approval rules on a session.
    ClearRules { session_id: String },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCmd {
    /// List all schedules in the current space.
    List,
    /// Create a recurring interval schedule targeting an existing session.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 3600)]
        interval_secs: u64,
        #[arg(long)]
        session: String,
        #[arg(long)]
        prompt: String,
    },
    /// Enable or pause a schedule (`--enabled true|false`).
    SetEnabled {
        schedule_id: String,
        #[arg(long, action = ArgAction::Set)]
        enabled: bool,
    },
    /// Manually trigger a schedule immediately.
    Trigger { schedule_id: String },
    /// List recent runs of a schedule.
    Runs {
        schedule_id: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Delete a schedule.
    Delete { schedule_id: String },
}

#[derive(Debug, Subcommand)]
pub enum GoalCmd {
    /// List goals on a session.
    List { session_id: String },
    /// Start a goal-driven loop on a session.
    Set {
        session_id: String,
        #[arg(long)]
        objective: String,
        #[arg(long)]
        max_turns: Option<u32>,
    },
    /// Stop an active goal.
    Stop { goal_id: String },
}

#[derive(Debug, Subcommand)]
pub enum DispatchCmd {
    /// List sent/received dispatches for a session.
    List {
        session_id: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Dispatch a prompt from one session to another existing session.
    Send {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        prompt: String,
        #[arg(long, value_enum, default_value_t = CliSubmitMode::Queue)]
        mode: CliSubmitMode,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// List registered MCP servers.
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Register a Space or Workspace MCP server (stores env key names only).
    Add {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "space")]
        scope: String,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long, default_value = "stdio")]
        transport: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "env-key")]
        env_keys: Vec<String>,
        #[arg(long, default_value = "")]
        description: String,
        /// Admin-only flag controlling whether a Space MCP is globally available (`true|false`).
        #[arg(long, action = ArgAction::Set)]
        global_available: Option<bool>,
    },
    /// Enable or disable an MCP server (`--enabled true|false`).
    SetEnabled {
        id: String,
        #[arg(long, action = ArgAction::Set)]
        enabled: bool,
    },
    /// Control whether a Space MCP server is globally available to all members (Admin/Owner).
    SetGlobal {
        id: String,
        #[arg(long, action = ArgAction::Set)]
        global_available: bool,
    },
    /// Probe command/endpoint readiness for an MCP server.
    Probe { id: String },
    /// Delete an MCP server.
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
pub enum SkillCmd {
    /// List registered skills.
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Install a new Space or Workspace skill.
    Install {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "space")]
        scope: String,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long, default_value = "1.0.0")]
        version: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        instructions: String,
        #[arg(long, default_value = "active")]
        lifecycle: String,
    },
    /// Update skill lifecycle state or enabled flag.
    Update {
        id: String,
        #[arg(long)]
        lifecycle: Option<String>,
        #[arg(long, action = ArgAction::Set)]
        enabled: Option<bool>,
    },
    /// Delete a skill.
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
pub enum HubCmd {
    /// Show effective merged Space + Workspace MCP servers, Skills, and client config JSON.
    Effective {
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug)]
pub struct CtlError {
    pub status: Option<u16>,
    pub message: String,
    pub body: Option<Value>,
}

fn validate_path_segment<'a>(label: &str, value: &'a str) -> Result<&'a str, CtlError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '\0') || c.is_control())
    {
        return Err(CtlError {
            status: None,
            message: format!(
                "invalid {label}: path separators and control characters are forbidden"
            ),
            body: None,
        });
    }
    Ok(value)
}

fn resolve_token(inline: &str, token_file: Option<&Path>) -> Result<String, CtlError> {
    let trimmed = inline.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }
    if let Some(path) = token_file {
        let content = std::fs::read_to_string(path).map_err(|e| CtlError {
            status: None,
            message: format!("failed to read --token-file {}: {e}", path.display()),
            body: None,
        })?;
        return Ok(content.trim().to_string());
    }
    Ok(String::new())
}

struct ApiClient {
    base_url: Url,
    token: String,
    http: Client,
}

impl ApiClient {
    fn new(server: &str, token: &str) -> Result<Self, CtlError> {
        let base_url = Url::parse(server.trim()).map_err(|e| CtlError {
            status: None,
            message: format!("invalid server URL: {e}"),
            body: None,
        })?;
        match base_url.scheme() {
            "http" | "https" if base_url.host_str().is_some() => {}
            _ => {
                return Err(CtlError {
                    status: None,
                    message: "server URL must use http:// or https:// with a valid host".into(),
                    body: None,
                });
            }
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CtlError {
                status: None,
                message: format!("failed to build HTTP client: {e}"),
                body: None,
            })?;
        Ok(Self {
            base_url,
            token: token.trim().to_string(),
            http,
        })
    }

    fn build_url(&self, segments: &[&str], query: &[(&str, String)]) -> Result<Url, CtlError> {
        let mut url = self.base_url.clone();
        url.set_query(None);
        url.set_fragment(None);
        {
            let mut segs = url.path_segments_mut().map_err(|()| CtlError {
                status: None,
                message: "server URL cannot be used as a base URL".into(),
                body: None,
            })?;
            segs.pop_if_empty();
            for s in segments {
                segs.push(s);
            }
        }
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url)
    }

    async fn send_segments(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
        auth: bool,
    ) -> Result<Value, CtlError> {
        let url = self.build_url(segments, query)?;
        let mut req = self.http.request(method, url);
        if auth && !self.token.is_empty() {
            req = req.header(AUTHORIZATION, format!("Bearer {}", self.token));
        }
        if let Some(payload) = body {
            let bytes = serde_json::to_vec(&payload).map_err(|e| CtlError {
                status: None,
                message: format!("failed to serialize request body: {e}"),
                body: None,
            })?;
            req = req.header(CONTENT_TYPE, "application/json").body(bytes);
        }

        let res = req.send().await.map_err(|e| CtlError {
            status: None,
            message: format!("request failed: {e}"),
            body: None,
        })?;
        let status = res.status();
        let bytes = res.bytes().await.map_err(|e| CtlError {
            status: Some(status.as_u16()),
            message: format!("failed to read response body: {e}"),
            body: None,
        })?;

        if !status.is_success() {
            let parsed = serde_json::from_slice::<Value>(&bytes).ok();
            let message = parsed
                .as_ref()
                .and_then(|v| v.get("message").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(CtlError {
                status: Some(status.as_u16()),
                message,
                body: parsed,
            });
        }

        if status.as_u16() == 204 || bytes.is_empty() {
            return Ok(json!({ "ok": true }));
        }

        serde_json::from_slice::<Value>(&bytes).map_err(|e| CtlError {
            status: Some(status.as_u16()),
            message: format!("invalid JSON response: {e}"),
            body: None,
        })
    }
}

pub async fn run(args: CtlArgs) -> Result<Value, CtlError> {
    let token = resolve_token(&args.token, args.token_file.as_deref())?;
    let client = ApiClient::new(&args.server, &token)?;

    match args.command {
        CtlCommand::Health => {
            client
                .send_segments(Method::GET, &["healthz"], &[], None, false)
                .await
        }
        CtlCommand::Me => {
            client
                .send_segments(Method::GET, &["api", "v1", "me"], &[], None, true)
                .await
        }
        CtlCommand::Members { action } => match action {
            MemberCmd::List => {
                client
                    .send_segments(Method::GET, &["api", "v1", "members"], &[], None, true)
                    .await
            }
            MemberCmd::Add { name, role } => {
                let r: Role = role.into();
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "members"],
                        &[],
                        Some(json!({ "display_name": name, "role": r })),
                        true,
                    )
                    .await
            }
            MemberCmd::SetRole { member_id, role } => {
                let member_id = validate_path_segment("member_id", &member_id)?;
                let r: Role = role.into();
                client
                    .send_segments(
                        Method::PATCH,
                        &["api", "v1", "members", member_id],
                        &[],
                        Some(json!({ "role": r })),
                        true,
                    )
                    .await
            }
            MemberCmd::Disable { member_id } => {
                let member_id = validate_path_segment("member_id", &member_id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "members", member_id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Tokens { action } => match action {
            TokenCmd::List { member_id } => {
                let member_id = validate_path_segment("member_id", &member_id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "members", member_id, "tokens"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            TokenCmd::Issue {
                member_id,
                label,
                ttl_secs,
            } => {
                let member_id = validate_path_segment("member_id", &member_id)?;
                let mut body = json!({ "label": label });
                if let Some(ttl) = ttl_secs {
                    body["ttl_secs"] = json!(ttl);
                }
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "members", member_id, "tokens"],
                        &[],
                        Some(body),
                        true,
                    )
                    .await
            }
            TokenCmd::Revoke { token_id } => {
                let token_id = validate_path_segment("token_id", &token_id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "tokens", token_id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Harnesses { probe } => {
            if probe {
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "harnesses", "probe"],
                        &[],
                        None,
                        true,
                    )
                    .await
            } else {
                client
                    .send_segments(Method::GET, &["api", "v1", "harnesses"], &[], None, true)
                    .await
            }
        }
        CtlCommand::Providers => {
            client
                .send_segments(Method::GET, &["api", "v1", "providers"], &[], None, true)
                .await
        }
        CtlCommand::Workspaces { action } => match action {
            WorkspaceCmd::List => {
                client
                    .send_segments(Method::GET, &["api", "v1", "workspaces"], &[], None, true)
                    .await
            }
            WorkspaceCmd::Get { id } => {
                let id = validate_path_segment("workspace_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "workspaces", id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            WorkspaceCmd::Register { name, path } => {
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "workspaces"],
                        &[],
                        Some(json!({ "name": name, "repo_path": path })),
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Sessions { action } => match action {
            SessionCmd::List => {
                client
                    .send_segments(Method::GET, &["api", "v1", "sessions"], &[], None, true)
                    .await
            }
            SessionCmd::Get { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(Method::GET, &["api", "v1", "sessions", id], &[], None, true)
                    .await
            }
            SessionCmd::Create {
                harness,
                cwd,
                workspace,
                model,
                sandbox,
                title,
            } => {
                let config = SessionConfig {
                    harness_id: harness,
                    cwd,
                    workspace_id: workspace.map(WorkspaceId::from),
                    model,
                    sandbox: sandbox.into(),
                    title,
                };
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "sessions"],
                        &[],
                        Some(json!({ "config": config })),
                        true,
                    )
                    .await
            }
            SessionCmd::Resume { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "sessions", id, "resume"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Close { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "sessions", id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Events {
                id,
                after_seq,
                limit,
            } => {
                let id = validate_path_segment("session_id", &id)?;
                let mut q = vec![("after_seq", after_seq.to_string())];
                if let Some(lim) = limit {
                    q.push(("limit", lim.to_string()));
                }
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "events"],
                        &q,
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Worktree { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "worktree"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::RemoveWorktree { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "sessions", id, "worktree"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Files { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "files"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::File { id, path } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "file"],
                        &[("path", path)],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Diff { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "diff"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::ProxyStats { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "proxy", "stats"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::RemoteScreen { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "remote-screen"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Pty { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", id, "pty"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::KillPty { id } => {
                let id = validate_path_segment("session_id", &id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "sessions", id, "pty"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Prompt {
            session_id,
            text,
            mode,
        } => {
            let session_id = validate_path_segment("session_id", &session_id)?;
            let mut body = json!({ "prompt": Prompt::text(text) });
            if let Some(m) = mode {
                let submit_mode: SubmitMode = m.into();
                body["mode"] = serde_json::to_value(submit_mode).unwrap_or(Value::Null);
            }
            client
                .send_segments(
                    Method::POST,
                    &["api", "v1", "sessions", session_id, "prompts"],
                    &[],
                    Some(body),
                    true,
                )
                .await
        }
        CtlCommand::Cancel { session_id } => {
            let session_id = validate_path_segment("session_id", &session_id)?;
            client
                .send_segments(
                    Method::POST,
                    &["api", "v1", "sessions", session_id, "cancel"],
                    &[],
                    None,
                    true,
                )
                .await
        }
        CtlCommand::Queue { action } => match action {
            QueueCmd::List { session_id } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", session_id, "queue"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            QueueCmd::Withdraw {
                session_id,
                queued_id,
            } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                let queued_id = validate_path_segment("queued_id", &queued_id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "sessions", session_id, "queue", queued_id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            QueueCmd::Move {
                session_id,
                queued_id,
                position,
            } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                let queued_id = validate_path_segment("queued_id", &queued_id)?;
                client
                    .send_segments(
                        Method::POST,
                        &[
                            "api", "v1", "sessions", session_id, "queue", queued_id, "move",
                        ],
                        &[],
                        Some(json!({ "position": position })),
                        true,
                    )
                    .await
            }
            QueueCmd::Resume { session_id } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "sessions", session_id, "queue", "resume"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Approvals { action } => match action {
            ApprovalCmd::List {
                session_id,
                pending,
            } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                let q = if pending {
                    vec![("pending_only", "true".to_string())]
                } else {
                    Vec::new()
                };
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", session_id, "approvals"],
                        &q,
                        None,
                        true,
                    )
                    .await
            }
            ApprovalCmd::Resolve {
                approval_id,
                option,
                cancel,
            } => {
                let approval_id = validate_path_segment("approval_id", &approval_id)?;
                let decision = if cancel {
                    ApprovalDecision::Cancelled
                } else if let Some(option_id) = option {
                    ApprovalDecision::Selected { option_id }
                } else {
                    return Err(CtlError {
                        status: None,
                        message: "must pass either --option <option_id> or --cancel".into(),
                        body: None,
                    });
                };
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "approvals", approval_id, "resolve"],
                        &[],
                        Some(json!({ "decision": decision })),
                        true,
                    )
                    .await
            }
            ApprovalCmd::Rules { session_id } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", session_id, "approval-rules"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            ApprovalCmd::ClearRules { session_id } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "sessions", session_id, "approval-rules"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Audit { target, limit } => {
            let mut q = vec![("limit", limit.to_string())];
            if let Some(t) = target {
                q.push(("target", t));
            }
            client
                .send_segments(Method::GET, &["api", "v1", "audit"], &q, None, true)
                .await
        }
        CtlCommand::Schedules { action } => match action {
            ScheduleCmd::List => {
                client
                    .send_segments(Method::GET, &["api", "v1", "schedules"], &[], None, true)
                    .await
            }
            ScheduleCmd::Create {
                name,
                interval_secs,
                session,
                prompt,
            } => {
                if interval_secs == 0 {
                    return Err(CtlError {
                        status: None,
                        message: "interval_secs must be greater than 0".into(),
                        body: None,
                    });
                }
                let start_at = i64::try_from(interval_secs)
                    .ok()
                    .and_then(|s| s.checked_mul(1000))
                    .and_then(|ms| om_core::now_ms().checked_add(ms))
                    .ok_or_else(|| CtlError {
                        status: None,
                        message: "interval_secs overflowed timestamp range".into(),
                        body: None,
                    })?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "schedules"],
                        &[],
                        Some(json!({
                            "name": name,
                            "spec": {
                                "kind": "every",
                                "interval_secs": interval_secs,
                                "start_at": start_at,
                            },
                            "target": {
                                "kind": "session",
                                "session_id": session,
                                "prompt": Prompt::text(prompt),
                                "mode": "queue",
                            },
                            "enabled": true,
                        })),
                        true,
                    )
                    .await
            }
            ScheduleCmd::SetEnabled {
                schedule_id,
                enabled,
            } => {
                let schedule_id = validate_path_segment("schedule_id", &schedule_id)?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "schedules", schedule_id, "enabled"],
                        &[],
                        Some(json!({ "enabled": enabled })),
                        true,
                    )
                    .await
            }
            ScheduleCmd::Trigger { schedule_id } => {
                let schedule_id = validate_path_segment("schedule_id", &schedule_id)?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "schedules", schedule_id, "trigger"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            ScheduleCmd::Runs { schedule_id, limit } => {
                let schedule_id = validate_path_segment("schedule_id", &schedule_id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "schedules", schedule_id, "runs"],
                        &[("limit", limit.to_string())],
                        None,
                        true,
                    )
                    .await
            }
            ScheduleCmd::Delete { schedule_id } => {
                let schedule_id = validate_path_segment("schedule_id", &schedule_id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "schedules", schedule_id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Goals { action } => match action {
            GoalCmd::List { session_id } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", session_id, "goals"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            GoalCmd::Set {
                session_id,
                objective,
                max_turns,
            } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                let mut body = json!({ "objective": objective });
                if let Some(mt) = max_turns {
                    body["max_turns"] = json!(mt);
                }
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "sessions", session_id, "goals"],
                        &[],
                        Some(body),
                        true,
                    )
                    .await
            }
            GoalCmd::Stop { goal_id } => {
                let goal_id = validate_path_segment("goal_id", &goal_id)?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "goals", goal_id, "stop"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Dispatches { action } => match action {
            DispatchCmd::List { session_id, limit } => {
                let session_id = validate_path_segment("session_id", &session_id)?;
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "sessions", session_id, "dispatches"],
                        &[("limit", limit.to_string())],
                        None,
                        true,
                    )
                    .await
            }
            DispatchCmd::Send {
                from,
                to,
                prompt,
                mode,
            } => {
                let m: SubmitMode = mode.into();
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "dispatches"],
                        &[],
                        Some(json!({
                            "from_session": from,
                            "target": {
                                "kind": "session",
                                "session_id": to,
                                "mode": m,
                            },
                            "prompt": Prompt::text(prompt),
                        })),
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Mcp { action } => match action {
            McpCmd::List { scope, workspace } => {
                let mut q = Vec::new();
                if let Some(s) = scope {
                    q.push(("scope", s));
                }
                if let Some(w) = workspace {
                    q.push(("workspace_id", w));
                }
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "mcp", "servers"],
                        &q,
                        None,
                        true,
                    )
                    .await
            }
            McpCmd::Add {
                name,
                scope,
                workspace,
                transport,
                command,
                args,
                url,
                env_keys,
                description,
                global_available,
            } => {
                let mut body = json!({
                    "name": name,
                    "scope": scope,
                    "workspace_id": workspace,
                    "transport": transport,
                    "command": command,
                    "args": args,
                    "url": url,
                    "env_keys": env_keys,
                    "description": description,
                    "enabled": true,
                });
                if let Some(ga) = global_available {
                    body["global_available"] = json!(ga);
                }
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "mcp", "servers"],
                        &[],
                        Some(body),
                        true,
                    )
                    .await
            }
            McpCmd::SetEnabled { id, enabled } => {
                let id = validate_path_segment("mcp_id", &id)?;
                client
                    .send_segments(
                        Method::PATCH,
                        &["api", "v1", "mcp", "servers", id],
                        &[],
                        Some(json!({ "enabled": enabled })),
                        true,
                    )
                    .await
            }
            McpCmd::SetGlobal {
                id,
                global_available,
            } => {
                let id = validate_path_segment("mcp_id", &id)?;
                client
                    .send_segments(
                        Method::PATCH,
                        &["api", "v1", "mcp", "servers", id],
                        &[],
                        Some(json!({ "global_available": global_available })),
                        true,
                    )
                    .await
            }
            McpCmd::Probe { id } => {
                let id = validate_path_segment("mcp_id", &id)?;
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "mcp", "servers", id, "probe"],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            McpCmd::Delete { id } => {
                let id = validate_path_segment("mcp_id", &id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "mcp", "servers", id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Skills { action } => match action {
            SkillCmd::List { scope, workspace } => {
                let mut q = Vec::new();
                if let Some(s) = scope {
                    q.push(("scope", s));
                }
                if let Some(w) = workspace {
                    q.push(("workspace_id", w));
                }
                client
                    .send_segments(Method::GET, &["api", "v1", "skills"], &q, None, true)
                    .await
            }
            SkillCmd::Install {
                name,
                scope,
                workspace,
                version,
                description,
                instructions,
                lifecycle,
            } => {
                client
                    .send_segments(
                        Method::POST,
                        &["api", "v1", "skills"],
                        &[],
                        Some(json!({
                            "name": name,
                            "scope": scope,
                            "workspace_id": workspace,
                            "version": version,
                            "description": description,
                            "instructions": instructions,
                            "lifecycle": lifecycle,
                            "enabled": true,
                        })),
                        true,
                    )
                    .await
            }
            SkillCmd::Update {
                id,
                lifecycle,
                enabled,
            } => {
                let id = validate_path_segment("skill_id", &id)?;
                let mut body = json!({});
                if let Some(lc) = lifecycle {
                    body["lifecycle"] = json!(lc);
                }
                if let Some(en) = enabled {
                    body["enabled"] = json!(en);
                }
                client
                    .send_segments(
                        Method::PATCH,
                        &["api", "v1", "skills", id],
                        &[],
                        Some(body),
                        true,
                    )
                    .await
            }
            SkillCmd::Delete { id } => {
                let id = validate_path_segment("skill_id", &id)?;
                client
                    .send_segments(
                        Method::DELETE,
                        &["api", "v1", "skills", id],
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Hub { action } => match action {
            HubCmd::Effective { workspace } => {
                let mut q = Vec::new();
                if let Some(w) = workspace {
                    q.push(("workspace_id", w));
                }
                client
                    .send_segments(
                        Method::GET,
                        &["api", "v1", "hub", "effective"],
                        &q,
                        None,
                        true,
                    )
                    .await
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        ctl: CtlArgs,
    }

    #[test]
    fn rejects_path_traversal_and_special_url_chars() {
        for bad in [
            "",
            ".",
            "..",
            "../members/mem_admin#",
            "a/b",
            "id?x=1",
            "id#frag",
        ] {
            assert!(
                validate_path_segment("id", bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert_eq!(
            validate_path_segment("id", "sess_01abc-def").unwrap(),
            "sess_01abc-def"
        );
    }

    #[test]
    fn set_enabled_and_set_global_accept_explicit_bool_values() {
        let parsed = TestCli::try_parse_from([
            "om-cli",
            "--token",
            "secret_should_redact",
            "schedules",
            "set-enabled",
            "sched-1",
            "--enabled",
            "false",
        ])
        .unwrap();
        let debug_str = format!("{:?}", parsed.ctl);
        assert!(
            !debug_str.contains("secret_should_redact"),
            "Debug must redact token: {debug_str}"
        );
        match parsed.ctl.command {
            CtlCommand::Schedules {
                action:
                    ScheduleCmd::SetEnabled {
                        schedule_id,
                        enabled,
                    },
            } => {
                assert_eq!(schedule_id, "sched-1");
                assert!(!enabled);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let mcp_parsed = TestCli::try_parse_from([
            "om-cli",
            "mcp",
            "set-global",
            "mcp-1",
            "--global-available",
            "false",
        ])
        .unwrap();
        match mcp_parsed.ctl.command {
            CtlCommand::Mcp {
                action:
                    McpCmd::SetGlobal {
                        id,
                        global_available,
                    },
            } => {
                assert_eq!(id, "mcp-1");
                assert!(!global_available);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
