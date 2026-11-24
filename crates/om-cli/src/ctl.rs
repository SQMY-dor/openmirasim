//! Pure-JSON HTTP control plane client (`om-cli ctl`, W1-6).

use clap::{Args, Subcommand, ValueEnum};
use om_core::approval::ApprovalDecision;
use om_core::auth::Role;
use om_core::content::Prompt;
use om_core::ids::WorkspaceId;
use om_core::queue::SubmitMode;
use om_core::sandbox::SandboxMode;
use om_core::session::SessionConfig;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Args)]
pub struct CtlArgs {
    /// Central `om-server` base URL.
    #[arg(long, env = "OM_SERVER_URL", default_value = "http://127.0.0.1:4939")]
    pub server: String,

    /// Bearer access token (`omt_...` device token or static bootstrap token).
    #[arg(long, env = "OM_TOKEN", default_value = "")]
    pub token: String,

    /// Pretty-print JSON output (default: compact single-line JSON for scripting/jq).
    #[arg(long, global = true)]
    pub pretty: bool,

    #[command(subcommand)]
    pub command: CtlCommand,
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
        #[arg(long, value_enum, default_value_t = CliSandboxMode::Direct)]
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
        position: usize,
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

pub struct CtlError {
    pub status: Option<u16>,
    pub message: String,
    pub body: Option<Value>,
}

struct ApiClient {
    base: String,
    token: String,
    http: Client,
}

impl ApiClient {
    fn new(server: &str, token: &str) -> Result<Self, CtlError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| CtlError {
                status: None,
                message: format!("failed to build HTTP client: {e}"),
                body: None,
            })?;
        Ok(Self {
            base: server.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http,
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        auth: bool,
    ) -> Result<Value, CtlError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.http.request(method, &url);
        if !query.is_empty() {
            req = req.query(query);
        }
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
    let client = ApiClient::new(&args.server, &args.token)?;

    match args.command {
        CtlCommand::Health => client.send(Method::GET, "/healthz", &[], None, false).await,
        CtlCommand::Me => {
            client
                .send(Method::GET, "/api/v1/me", &[], None, true)
                .await
        }
        CtlCommand::Members { action } => match action {
            MemberCmd::List => {
                client
                    .send(Method::GET, "/api/v1/members", &[], None, true)
                    .await
            }
            MemberCmd::Add { name, role } => {
                let r: Role = role.into();
                client
                    .send(
                        Method::POST,
                        "/api/v1/members",
                        &[],
                        Some(json!({ "display_name": name, "role": r })),
                        true,
                    )
                    .await
            }
            MemberCmd::SetRole { member_id, role } => {
                let r: Role = role.into();
                client
                    .send(
                        Method::PATCH,
                        &format!("/api/v1/members/{member_id}"),
                        &[],
                        Some(json!({ "role": r })),
                        true,
                    )
                    .await
            }
            MemberCmd::Disable { member_id } => {
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/members/{member_id}"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Tokens { action } => match action {
            TokenCmd::List { member_id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/members/{member_id}/tokens"),
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
                let mut body = json!({ "label": label });
                if let Some(ttl) = ttl_secs {
                    body["ttl_secs"] = json!(ttl);
                }
                client
                    .send(
                        Method::POST,
                        &format!("/api/v1/members/{member_id}/tokens"),
                        &[],
                        Some(body),
                        true,
                    )
                    .await
            }
            TokenCmd::Revoke { token_id } => {
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/tokens/{token_id}"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
        },
        CtlCommand::Harnesses { probe } => {
            let path = if probe {
                "/api/v1/harnesses/probe"
            } else {
                "/api/v1/harnesses"
            };
            client.send(Method::GET, path, &[], None, true).await
        }
        CtlCommand::Providers => {
            client
                .send(Method::GET, "/api/v1/providers", &[], None, true)
                .await
        }
        CtlCommand::Workspaces { action } => match action {
            WorkspaceCmd::List => {
                client
                    .send(Method::GET, "/api/v1/workspaces", &[], None, true)
                    .await
            }
            WorkspaceCmd::Get { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/workspaces/{id}"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            WorkspaceCmd::Register { name, path } => {
                client
                    .send(
                        Method::POST,
                        "/api/v1/workspaces",
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
                    .send(Method::GET, "/api/v1/sessions", &[], None, true)
                    .await
            }
            SessionCmd::Get { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}"),
                        &[],
                        None,
                        true,
                    )
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
                    .send(
                        Method::POST,
                        "/api/v1/sessions",
                        &[],
                        Some(json!({ "config": config })),
                        true,
                    )
                    .await
            }
            SessionCmd::Resume { id } => {
                client
                    .send(
                        Method::POST,
                        &format!("/api/v1/sessions/{id}/resume"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Close { id } => {
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/sessions/{id}"),
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
                let mut q = vec![("after_seq", after_seq.to_string())];
                if let Some(lim) = limit {
                    q.push(("limit", lim.to_string()));
                }
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/events"),
                        &q,
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Worktree { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/worktree"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::RemoveWorktree { id } => {
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/sessions/{id}/worktree"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Files { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/files"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::File { id, path } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/file"),
                        &[("path", path)],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Diff { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/diff"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::ProxyStats { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/proxy/stats"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::RemoteScreen { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/remote-screen"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::Pty { id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{id}/pty"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            SessionCmd::KillPty { id } => {
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/sessions/{id}/pty"),
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
            let mut body = json!({ "prompt": Prompt::text(text) });
            if let Some(m) = mode {
                let submit_mode: SubmitMode = m.into();
                body["mode"] = serde_json::to_value(submit_mode).unwrap_or(Value::Null);
            }
            client
                .send(
                    Method::POST,
                    &format!("/api/v1/sessions/{session_id}/prompts"),
                    &[],
                    Some(body),
                    true,
                )
                .await
        }
        CtlCommand::Cancel { session_id } => {
            client
                .send(
                    Method::POST,
                    &format!("/api/v1/sessions/{session_id}/cancel"),
                    &[],
                    None,
                    true,
                )
                .await
        }
        CtlCommand::Queue { action } => match action {
            QueueCmd::List { session_id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{session_id}/queue"),
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
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/sessions/{session_id}/queue/{queued_id}"),
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
                client
                    .send(
                        Method::POST,
                        &format!("/api/v1/sessions/{session_id}/queue/{queued_id}/move"),
                        &[],
                        Some(json!({ "position": position })),
                        true,
                    )
                    .await
            }
            QueueCmd::Resume { session_id } => {
                client
                    .send(
                        Method::POST,
                        &format!("/api/v1/sessions/{session_id}/queue/resume"),
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
                let q = if pending {
                    vec![("pending_only", "true".to_string())]
                } else {
                    Vec::new()
                };
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{session_id}/approvals"),
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
                    .send(
                        Method::POST,
                        &format!("/api/v1/approvals/{approval_id}/resolve"),
                        &[],
                        Some(json!({ "decision": decision })),
                        true,
                    )
                    .await
            }
            ApprovalCmd::Rules { session_id } => {
                client
                    .send(
                        Method::GET,
                        &format!("/api/v1/sessions/{session_id}/approval-rules"),
                        &[],
                        None,
                        true,
                    )
                    .await
            }
            ApprovalCmd::ClearRules { session_id } => {
                client
                    .send(
                        Method::DELETE,
                        &format!("/api/v1/sessions/{session_id}/approval-rules"),
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
                .send(Method::GET, "/api/v1/audit", &q, None, true)
                .await
        }
    }
}
