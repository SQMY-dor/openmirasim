//! Space / Workspace two-tier MCP Hub & Skills lifecycle registry (Phase 4).
//!
//! Architecture & safety invariants:
//! - **Per-member MCP isolation + Admin global-availability control**:
//!   - `Space`-scoped MCP servers are managed exclusively by `Admin`/`Owner` (`ManageWorkspaces`).
//!     Each `Space` MCP carries an explicit `global_available: bool` toggle so administrators
//!     control exactly which MCP servers are globally available to all space members.
//!   - `Workspace`-scoped MCP servers are strictly isolated per `owner_member_id`: each member
//!     creates, views, probes, edits, and resolves only their own workspace MCP servers plus
//!     administrator-approved `global_available == true` Space MCP servers.
//! - **Secret & env isolation**: MCP configs store ONLY allowlisted environment variable key
//!   names (`env_keys`), never plaintext values and never server/loader/provider secrets.
//! - **Atomic persistence**: changes are staged and flushed atomically (`0600` unique temp file +
//!   `sync_all` + rename) before committing to memory so disk failures never diverge state.

use super::push::{validate_external_url, write_atomic_private_json};
use om_core::ids::{MemberId, SpaceId, WorkspaceId};
use om_core::{CoreError, CoreResult, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MAX_MCP_SERVERS_PER_SPACE: usize = 100;
pub const MAX_MCP_SERVERS_PER_MEMBER: usize = 30;
pub const MAX_SKILLS_PER_SPACE: usize = 200;
pub const MAX_SKILL_INSTRUCTIONS_BYTES: usize = 64 * 1024;

const MAX_DESCRIPTION_BYTES: usize = 2048;
const MAX_ARGS_COUNT: usize = 64;
const MAX_ARG_BYTES: usize = 1024;
const MAX_ENV_KEYS_COUNT: usize = 32;
const MAX_COMMAND_BYTES: usize = 256;

/// Sensitive / dynamic-linker / server-secret environment variable names that must never
/// be forwarded through MCP configs.
const BLOCKED_ENV_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "IFS",
    "ENV",
    "BASH_ENV",
    "CDPATH",
    "GLOBIGNORE",
    "NODE_OPTIONS",
    "RUBYOPT",
    "PERL5OPT",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

const BLOCKED_ENV_PREFIXES: &[&str] = &["OM_", "LD_", "DYLD_", "PYTHON", "GIT_"];

/// Standard executable prefixes permitted for non-Admin stdio MCP server commands.
const STANDARD_BIN_PREFIXES: &[&str] = &["/bin/", "/usr/bin/", "/usr/local/bin/"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubScopeLevel {
    Space,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Sse,
    StreamableHttp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerRecord {
    pub id: String,
    pub space_id: SpaceId,
    pub scope: HubScopeLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Member that owns this private workspace MCP server (`None` for Space-scoped MCPs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_member_id: Option<MemberId>,
    /// Admin-controlled switch deciding whether a `Space`-scoped MCP is globally available
    /// to all space members. Always `false` for private `Workspace`-scoped MCPs.
    #[serde(default = "default_true")]
    pub global_available: bool,
    pub name: String,
    pub transport: McpTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Environment variable key names forwarded to the MCP server (never plaintext values).
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub description: String,
    pub enabled: bool,
    pub created_by: MemberId,
    pub created_at: i64,
    pub updated_at: i64,
}

impl McpServerRecord {
    /// Returns the owning member for Workspace-scoped MCPs (falling back to `created_by` for
    /// records persisted before `owner_member_id` was added).
    pub fn effective_owner(&self) -> Option<&MemberId> {
        match self.scope {
            HubScopeLevel::Space => None,
            HubScopeLevel::Workspace => Some(self.owner_member_id.as_ref().unwrap_or(&self.created_by)),
        }
    }

    /// Whether `caller` is permitted to see, probe, or resolve this MCP server.
    pub fn is_visible_to(&self, caller: &MemberId, is_admin: bool) -> bool {
        match self.scope {
            HubScopeLevel::Space => self.global_available || is_admin,
            HubScopeLevel::Workspace => self.effective_owner() == Some(caller),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateMcpServerRequest {
    pub scope: HubScopeLevel,
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    pub name: String,
    pub transport: McpTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional Admin-only flag controlling global availability of a `Space`-scoped MCP
    /// (defaults to `true` for `Space` scope, always `false` for `Workspace` scope).
    #[serde(default)]
    pub global_available: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateMcpServerRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub global_available: Option<bool>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env_keys: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMcpServersResponse {
    pub servers: Vec<McpServerRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpProbeResult {
    pub id: String,
    pub name: String,
    pub transport: McpTransport,
    pub reachable: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillLifecycle {
    Draft,
    Active,
    Deprecated,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRecord {
    pub id: String,
    pub space_id: SpaceId,
    pub scope: HubScopeLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub instructions: String,
    pub lifecycle: SkillLifecycle,
    pub enabled: bool,
    pub created_by: MemberId,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSkillRequest {
    pub scope: HubScopeLevel,
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    pub name: String,
    #[serde(default = "default_skill_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub instructions: String,
    #[serde(default = "default_skill_lifecycle")]
    pub lifecycle: SkillLifecycle,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateSkillRequest {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub lifecycle: Option<SkillLifecycle>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSkillsResponse {
    pub skills: Vec<SkillRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveMcpServer {
    #[serde(flatten)]
    pub record: McpServerRecord,
    pub overridden_space_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveSkill {
    #[serde(flatten)]
    pub record: SkillRecord,
    pub overridden_space_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveHubResponse {
    pub space_id: SpaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    pub mcp_servers: Vec<EffectiveMcpServer>,
    pub skills: Vec<EffectiveSkill>,
    /// Ready-to-inject `mcpServers` map compatible with `opencode` / `cc` / `codexcli` / `dsh`.
    pub mcp_client_config: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedHubState {
    mcp_servers: Vec<McpServerRecord>,
    skills: Vec<SkillRecord>,
}

fn default_true() -> bool {
    true
}

fn default_skill_version() -> String {
    "1.0.0".into()
}

fn default_skill_lifecycle() -> SkillLifecycle {
    SkillLifecycle::Active
}

fn validate_slug(kind: &str, raw: &str) -> CoreResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 64
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        || trimmed.starts_with('.')
    {
        return Err(CoreError::InvalidInput(format!(
            "invalid {kind} name `{raw}`: use 1-64 ASCII letters, digits, `.`, `_`, or `-`"
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_scope(scope: HubScopeLevel, workspace_id: &Option<WorkspaceId>) -> CoreResult<()> {
    match (scope, workspace_id) {
        (HubScopeLevel::Space, Some(_)) => Err(CoreError::InvalidInput(
            "space-scoped hub entries must not specify workspace_id".into(),
        )),
        (HubScopeLevel::Workspace, None) => Err(CoreError::InvalidInput(
            "workspace-scoped hub entries require workspace_id".into(),
        )),
        _ => Ok(()),
    }
}

fn validate_description(raw: &str) -> CoreResult<String> {
    let trimmed = raw.trim();
    if trimmed.len() > MAX_DESCRIPTION_BYTES {
        return Err(CoreError::InvalidInput(format!(
            "description exceeds {MAX_DESCRIPTION_BYTES} bytes"
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_args(args: Vec<String>) -> CoreResult<Vec<String>> {
    if args.len() > MAX_ARGS_COUNT {
        return Err(CoreError::InvalidInput(format!(
            "MCP args exceed maximum count ({MAX_ARGS_COUNT})"
        )));
    }
    for arg in &args {
        if arg.len() > MAX_ARG_BYTES || arg.bytes().any(|b| b == 0) {
            return Err(CoreError::InvalidInput(format!(
                "MCP arg must not contain NUL and must be at most {MAX_ARG_BYTES} bytes"
            )));
        }
    }
    Ok(args)
}

fn is_blocked_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    BLOCKED_ENV_EXACT.contains(&upper.as_str())
        || BLOCKED_ENV_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
}

fn validate_env_keys(keys: &[String]) -> CoreResult<Vec<String>> {
    if keys.len() > MAX_ENV_KEYS_COUNT {
        return Err(CoreError::InvalidInput(format!(
            "env_keys exceeds maximum count ({MAX_ENV_KEYS_COUNT})"
        )));
    }
    let mut out = Vec::with_capacity(keys.len());
    for raw in keys {
        let k = raw.trim();
        if k.is_empty() || k.len() > 80 || k.contains('=') {
            return Err(CoreError::InvalidInput(format!(
                "invalid env_key `{raw}`: supply variable names only (never KEY=VALUE secrets)"
            )));
        }
        let mut chars = k.bytes();
        let first_ok = chars
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
        let rest_ok = chars.all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !first_ok || !rest_ok {
            return Err(CoreError::InvalidInput(format!(
                "invalid env_key `{raw}`: must be a POSIX environment variable name"
            )));
        }
        if is_blocked_env_key(k) {
            return Err(CoreError::InvalidInput(format!(
                "env_key `{k}` is reserved or sensitive and cannot be forwarded to MCP servers"
            )));
        }
        if !out.iter().any(|existing: &String| existing == k) {
            out.push(k.to_string());
        }
    }
    Ok(out)
}

fn validate_stdio_command(cmd: &str, is_admin: bool) -> CoreResult<String> {
    let trimmed = cmd.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_COMMAND_BYTES
        || trimmed.bytes().any(|b| b <= 0x1f || b == 0x7f)
    {
        return Err(CoreError::InvalidInput(
            "stdio MCP command must be 1..=256 printable characters".into(),
        ));
    }
    let p = Path::new(trimmed);
    if p.components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(CoreError::InvalidInput(
            "stdio MCP command must not contain `..` path traversal".into(),
        ));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        if !p.is_absolute() {
            return Err(CoreError::InvalidInput(
                "stdio MCP command must be a bare executable name or an absolute path".into(),
            ));
        }
        if !is_admin
            && !STANDARD_BIN_PREFIXES
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
        {
            return Err(CoreError::InvalidInput(
                "non-admin stdio MCP command paths must be bare names or under /bin, /usr/bin, or /usr/local/bin"
                    .into(),
            ));
        }
    }
    Ok(trimmed.to_string())
}

fn validate_mcp_transport(
    transport: McpTransport,
    command: &Option<String>,
    url: &Option<String>,
    is_admin: bool,
) -> CoreResult<(Option<String>, Option<String>)> {
    match transport {
        McpTransport::Stdio => {
            let raw_cmd = command
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CoreError::InvalidInput("stdio MCP server requires a non-empty command".into())
                })?;
            if url.as_deref().is_some_and(|u| !u.trim().is_empty()) {
                return Err(CoreError::InvalidInput(
                    "stdio MCP server must not set url".into(),
                ));
            }
            let cmd = validate_stdio_command(raw_cmd, is_admin)?;
            Ok((Some(cmd), None))
        }
        McpTransport::Sse | McpTransport::StreamableHttp => {
            let raw_url = url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CoreError::InvalidInput("HTTP/SSE MCP server requires a valid url".into())
                })?;
            if command.as_deref().is_some_and(|c| !c.trim().is_empty()) {
                return Err(CoreError::InvalidInput(
                    "HTTP/SSE MCP server must not set command".into(),
                ));
            }
            let validated = validate_external_url(raw_url, "MCP server url", false, false)?;
            Ok((None, Some(validated)))
        }
    }
}

fn is_executable_file(path: &Path) -> bool {
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

fn resolve_command_on_path(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    if p.is_absolute() || cmd.contains('/') {
        return (p.is_absolute() && is_executable_file(p)).then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(cmd))
        .find(|candidate| is_executable_file(candidate))
}

/// Thread-safe, optionally file-backed two-tier MCP Hub & Skills registry.
#[derive(Clone)]
pub struct HubStore {
    inner: Arc<Mutex<PersistedHubState>>,
    persist_path: Option<PathBuf>,
}

impl HubStore {
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let mut initial = persist_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<PersistedHubState>(&bytes).ok())
            .unwrap_or_default();
        // Backfill owner_member_id & global_available invariants for legacy registries.
        for rec in &mut initial.mcp_servers {
            if rec.scope == HubScopeLevel::Workspace {
                if rec.owner_member_id.is_none() {
                    rec.owner_member_id = Some(rec.created_by.clone());
                }
                rec.global_available = false;
            }
        }
        Self {
            inner: Arc::new(Mutex::new(initial)),
            persist_path,
        }
    }

    fn flush_snapshot(&self, state: &PersistedHubState) -> CoreResult<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| CoreError::Storage(format!("serialize hub registry: {e}")))?;
        write_atomic_private_json(path, &bytes)
    }

    /// Lists MCP servers visible to `caller`:
    /// - `Space` MCPs where `global_available == true` (or all `Space` MCPs when `is_admin`)
    /// - `Workspace` MCPs owned by `caller` (strictly isolated per member).
    pub fn list_mcp(
        &self,
        space_id: &SpaceId,
        caller: &MemberId,
        is_admin: bool,
        scope: Option<HubScopeLevel>,
        workspace_id: Option<&WorkspaceId>,
    ) -> Vec<McpServerRecord> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .mcp_servers
            .iter()
            .filter(|s| &s.space_id == space_id)
            .filter(|s| s.is_visible_to(caller, is_admin))
            .filter(|s| scope.is_none_or(|sc| s.scope == sc))
            .filter(|s| workspace_id.is_none_or(|w| s.workspace_id.as_ref() == Some(w)))
            .cloned()
            .collect()
    }

    /// Fetches a single MCP server if visible to `caller` (or returns `NotFound` to hide
    /// other members' private MCP servers and admin-only unpublished Space MCPs).
    pub fn get_mcp(
        &self,
        space_id: &SpaceId,
        id: &str,
        caller: &MemberId,
        is_admin: bool,
    ) -> CoreResult<McpServerRecord> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .mcp_servers
            .iter()
            .find(|s| &s.space_id == space_id && s.id == id && s.is_visible_to(caller, is_admin))
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("mcp server {id}")))
    }

    /// Fetches any MCP server in `space_id` by ID (for Admin moderation/delete checks).
    pub fn get_mcp_for_admin(&self, space_id: &SpaceId, id: &str) -> CoreResult<McpServerRecord> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .mcp_servers
            .iter()
            .find(|s| &s.space_id == space_id && s.id == id)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("mcp server {id}")))
    }

    pub fn create_mcp(
        &self,
        space_id: SpaceId,
        by: MemberId,
        is_admin: bool,
        req: CreateMcpServerRequest,
    ) -> CoreResult<McpServerRecord> {
        validate_scope(req.scope, &req.workspace_id)?;
        let name = validate_slug("mcp server", &req.name)?;
        let (command, url) = validate_mcp_transport(req.transport, &req.command, &req.url, is_admin)?;
        let args = validate_args(req.args)?;
        let env_keys = validate_env_keys(&req.env_keys)?;
        let description = validate_description(&req.description)?;

        let (owner_member_id, global_available) = match req.scope {
            HubScopeLevel::Space => {
                if !is_admin {
                    return Err(CoreError::Forbidden(
                        "only admins may create globally-managed Space MCP servers".into(),
                    ));
                }
                (None, req.global_available.unwrap_or(true))
            }
            HubScopeLevel::Workspace => {
                if req.global_available == Some(true) {
                    return Err(CoreError::Forbidden(
                        "workspace MCP servers are member-isolated; only admins can publish global Space MCP servers"
                            .into(),
                    ));
                }
                (Some(by.clone()), false)
            }
        };

        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let space_count = guard
            .mcp_servers
            .iter()
            .filter(|s| s.space_id == space_id)
            .count();
        let member_count = guard
            .mcp_servers
            .iter()
            .filter(|s| {
                s.space_id == space_id
                    && s.scope == HubScopeLevel::Workspace
                    && s.effective_owner() == Some(&by)
            })
            .count();
        let duplicate = guard.mcp_servers.iter().any(|s| {
            s.space_id == space_id
                && s.scope == req.scope
                && s.workspace_id == req.workspace_id
                && s.effective_owner() == owner_member_id.as_ref()
                && s.name.eq_ignore_ascii_case(&name)
        });
        if duplicate {
            return Err(CoreError::InvalidInput(format!(
                "MCP server `{name}` already exists in this scope"
            )));
        }
        if space_count >= MAX_MCP_SERVERS_PER_SPACE {
            return Err(CoreError::LimitExceeded(format!(
                "maximum MCP servers per space reached ({MAX_MCP_SERVERS_PER_SPACE})"
            )));
        }
        if req.scope == HubScopeLevel::Workspace && member_count >= MAX_MCP_SERVERS_PER_MEMBER {
            return Err(CoreError::LimitExceeded(format!(
                "maximum personal MCP servers per member reached ({MAX_MCP_SERVERS_PER_MEMBER})"
            )));
        }

        let now = now_ms();
        let record = McpServerRecord {
            id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
            space_id,
            scope: req.scope,
            workspace_id: req.workspace_id,
            owner_member_id,
            global_available,
            name,
            transport: req.transport,
            command,
            args,
            url,
            env_keys,
            description,
            enabled: req.enabled,
            created_by: by,
            created_at: now,
            updated_at: now,
        };
        let mut next = guard.clone();
        next.mcp_servers.insert(0, record.clone());
        self.flush_snapshot(&next)?;
        *guard = next;
        Ok(record)
    }

    pub fn update_mcp(
        &self,
        space_id: &SpaceId,
        id: &str,
        caller: &MemberId,
        is_admin: bool,
        req: UpdateMcpServerRequest,
    ) -> CoreResult<McpServerRecord> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = guard
            .mcp_servers
            .iter()
            .position(|s| &s.space_id == space_id && s.id == id && s.is_visible_to(caller, is_admin))
            .ok_or_else(|| CoreError::NotFound(format!("mcp server {id}")))?;
        let mut updated = guard.mcp_servers[idx].clone();

        match updated.scope {
            HubScopeLevel::Space => {
                if !is_admin {
                    return Err(CoreError::Forbidden(
                        "only admins may modify Space MCP servers".into(),
                    ));
                }
                if let Some(ga) = req.global_available {
                    updated.global_available = ga;
                }
            }
            HubScopeLevel::Workspace => {
                if updated.effective_owner() != Some(caller) {
                    return Err(CoreError::NotFound(format!("mcp server {id}")));
                }
                if req.global_available == Some(true) {
                    return Err(CoreError::Forbidden(
                        "workspace MCP servers are member-isolated and cannot be marked globally available"
                            .into(),
                    ));
                }
                updated.global_available = false;
            }
        }

        if let Some(enabled) = req.enabled {
            updated.enabled = enabled;
        }
        if let Some(desc) = req.description {
            updated.description = validate_description(&desc)?;
        }
        if let Some(args) = req.args {
            updated.args = validate_args(args)?;
        }
        if let Some(keys) = req.env_keys {
            updated.env_keys = validate_env_keys(&keys)?;
        }
        let next_cmd = req.command.or(updated.command.clone());
        let next_url = req.url.or(updated.url.clone());
        let (cmd, url) = validate_mcp_transport(updated.transport, &next_cmd, &next_url, is_admin)?;
        updated.command = cmd;
        updated.url = url;
        updated.updated_at = now_ms();

        let mut next = guard.clone();
        next.mcp_servers[idx] = updated.clone();
        self.flush_snapshot(&next)?;
        *guard = next;
        Ok(updated)
    }

    pub fn delete_mcp(
        &self,
        space_id: &SpaceId,
        id: &str,
        caller: &MemberId,
        is_admin: bool,
    ) -> CoreResult<McpServerRecord> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = guard
            .mcp_servers
            .iter()
            .position(|s| {
                &s.space_id == space_id
                    && s.id == id
                    && (is_admin || s.is_visible_to(caller, false))
            })
            .ok_or_else(|| CoreError::NotFound(format!("mcp server {id}")))?;
        let target = &guard.mcp_servers[idx];
        match target.scope {
            HubScopeLevel::Space if !is_admin => {
                return Err(CoreError::Forbidden(
                    "only admins may delete Space MCP servers".into(),
                ));
            }
            HubScopeLevel::Workspace if target.effective_owner() != Some(caller) && !is_admin => {
                return Err(CoreError::NotFound(format!("mcp server {id}")));
            }
            _ => {}
        }
        let mut next = guard.clone();
        let removed = next.mcp_servers.remove(idx);
        self.flush_snapshot(&next)?;
        *guard = next;
        Ok(removed)
    }

    pub fn probe_mcp(
        &self,
        space_id: &SpaceId,
        id: &str,
        caller: &MemberId,
        is_admin: bool,
    ) -> CoreResult<McpProbeResult> {
        let rec = self.get_mcp(space_id, id, caller, is_admin)?;
        let (reachable, detail) = match rec.transport {
            McpTransport::Stdio => {
                let cmd = rec.command.as_deref().unwrap_or_default();
                if resolve_command_on_path(cmd).is_some() {
                    (true, "executable ready".into())
                } else {
                    (false, format!("command `{cmd}` not found or not executable"))
                }
            }
            McpTransport::Sse | McpTransport::StreamableHttp => {
                let url = rec.url.as_deref().unwrap_or_default();
                match validate_external_url(url, "MCP server url", false, false)
                    .ok()
                    .and_then(|u| reqwest::Url::parse(&u).ok())
                {
                    Some(u) => (
                        true,
                        format!(
                            "validated {} endpoint {}",
                            u.scheme(),
                            u.host_str().unwrap_or_default()
                        ),
                    ),
                    None => (false, "invalid or disallowed endpoint URL".into()),
                }
            }
        };
        Ok(McpProbeResult {
            id: rec.id,
            name: rec.name,
            transport: rec.transport,
            reachable,
            detail,
        })
    }

    pub fn list_skills(
        &self,
        space_id: &SpaceId,
        scope: Option<HubScopeLevel>,
        workspace_id: Option<&WorkspaceId>,
    ) -> Vec<SkillRecord> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .skills
            .iter()
            .filter(|s| &s.space_id == space_id)
            .filter(|s| scope.is_none_or(|sc| s.scope == sc))
            .filter(|s| workspace_id.is_none_or(|w| s.workspace_id.as_ref() == Some(w)))
            .cloned()
            .collect()
    }

    pub fn get_skill(&self, space_id: &SpaceId, id: &str) -> CoreResult<SkillRecord> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .skills
            .iter()
            .find(|s| &s.space_id == space_id && s.id == id)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("skill {id}")))
    }

    pub fn create_skill(
        &self,
        space_id: SpaceId,
        by: MemberId,
        req: CreateSkillRequest,
    ) -> CoreResult<SkillRecord> {
        validate_scope(req.scope, &req.workspace_id)?;
        let name = validate_slug("skill", &req.name)?;
        let version = req.version.trim().to_string();
        if version.is_empty() || version.len() > 32 {
            return Err(CoreError::InvalidInput(
                "skill version must be 1-32 characters".into(),
            ));
        }
        let description = validate_description(&req.description)?;
        let instructions = req.instructions.trim().to_string();
        if instructions.is_empty() || instructions.len() > MAX_SKILL_INSTRUCTIONS_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "skill instructions must be non-empty and at most {MAX_SKILL_INSTRUCTIONS_BYTES} bytes"
            )));
        }

        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let duplicate = guard.skills.iter().any(|s| {
            s.space_id == space_id
                && s.scope == req.scope
                && s.workspace_id == req.workspace_id
                && s.name.eq_ignore_ascii_case(&name)
        });
        if duplicate {
            return Err(CoreError::InvalidInput(format!(
                "skill `{name}` already exists in this scope"
            )));
        }
        let space_count = guard
            .skills
            .iter()
            .filter(|s| s.space_id == space_id)
            .count();
        if space_count >= MAX_SKILLS_PER_SPACE {
            return Err(CoreError::LimitExceeded(format!(
                "maximum skills per space reached ({MAX_SKILLS_PER_SPACE})"
            )));
        }

        let tags: Vec<String> = req
            .tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty() && t.len() <= 32)
            .take(12)
            .collect();
        let now = now_ms();
        let enabled = req.enabled && req.lifecycle != SkillLifecycle::Archived;
        let record = SkillRecord {
            id: format!("skl_{}", uuid::Uuid::now_v7().simple()),
            space_id,
            scope: req.scope,
            workspace_id: req.workspace_id,
            name,
            version,
            description,
            tags,
            instructions,
            lifecycle: req.lifecycle,
            enabled,
            created_by: by,
            created_at: now,
            updated_at: now,
        };
        let mut next = guard.clone();
        next.skills.insert(0, record.clone());
        self.flush_snapshot(&next)?;
        *guard = next;
        Ok(record)
    }

    pub fn update_skill(
        &self,
        space_id: &SpaceId,
        id: &str,
        req: UpdateSkillRequest,
    ) -> CoreResult<SkillRecord> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = guard
            .skills
            .iter()
            .position(|s| &s.space_id == space_id && s.id == id)
            .ok_or_else(|| CoreError::NotFound(format!("skill {id}")))?;
        let mut updated = guard.skills[idx].clone();
        if let Some(version) = req.version {
            let v = version.trim().to_string();
            if v.is_empty() || v.len() > 32 {
                return Err(CoreError::InvalidInput(
                    "skill version must be 1-32 characters".into(),
                ));
            }
            updated.version = v;
        }
        if let Some(desc) = req.description {
            updated.description = validate_description(&desc)?;
        }
        if let Some(tags) = req.tags {
            updated.tags = tags
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty() && t.len() <= 32)
                .take(12)
                .collect();
        }
        if let Some(instr) = req.instructions {
            let trimmed = instr.trim().to_string();
            if trimmed.is_empty() || trimmed.len() > MAX_SKILL_INSTRUCTIONS_BYTES {
                return Err(CoreError::InvalidInput(format!(
                    "skill instructions must be non-empty and at most {MAX_SKILL_INSTRUCTIONS_BYTES} bytes"
                )));
            }
            updated.instructions = trimmed;
        }
        if let Some(lifecycle) = req.lifecycle {
            updated.lifecycle = lifecycle;
            if lifecycle == SkillLifecycle::Archived {
                updated.enabled = false;
            }
        }
        if let Some(enabled) = req.enabled {
            if enabled && updated.lifecycle == SkillLifecycle::Archived {
                return Err(CoreError::InvalidInput(
                    "cannot enable an archived skill; change lifecycle first".into(),
                ));
            }
            updated.enabled = enabled;
        }
        updated.updated_at = now_ms();
        let mut next = guard.clone();
        next.skills[idx] = updated.clone();
        self.flush_snapshot(&next)?;
        *guard = next;
        Ok(updated)
    }

    pub fn delete_skill(&self, space_id: &SpaceId, id: &str) -> CoreResult<SkillRecord> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = guard
            .skills
            .iter()
            .position(|s| &s.space_id == space_id && s.id == id)
            .ok_or_else(|| CoreError::NotFound(format!("skill {id}")))?;
        let mut next = guard.clone();
        let removed = next.skills.remove(idx);
        self.flush_snapshot(&next)?;
        *guard = next;
        Ok(removed)
    }

    /// Computes effective two-tier inheritance for `caller`:
    /// - Starts with Admin-approved `Space` MCP servers (`enabled && global_available`).
    /// - Overlays ONLY `caller`'s own `Workspace` MCP servers (strict member isolation).
    /// - Excludes both `Draft` and `Archived` skills until activated.
    pub fn effective(
        &self,
        space_id: &SpaceId,
        caller: &MemberId,
        workspace_id: Option<&WorkspaceId>,
    ) -> EffectiveHubResponse {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut merged_mcp: BTreeMap<String, EffectiveMcpServer> = BTreeMap::new();
        for s in guard.mcp_servers.iter().filter(|s| {
            &s.space_id == space_id
                && s.scope == HubScopeLevel::Space
                && s.enabled
                && s.global_available
        }) {
            merged_mcp.insert(
                s.name.to_ascii_lowercase(),
                EffectiveMcpServer {
                    record: s.clone(),
                    overridden_space_default: false,
                },
            );
        }
        if let Some(ws) = workspace_id {
            for s in guard.mcp_servers.iter().filter(|s| {
                &s.space_id == space_id
                    && s.scope == HubScopeLevel::Workspace
                    && s.workspace_id.as_ref() == Some(ws)
                    && s.effective_owner() == Some(caller)
            }) {
                let key = s.name.to_ascii_lowercase();
                let had_space = merged_mcp.contains_key(&key);
                if s.enabled {
                    merged_mcp.insert(
                        key,
                        EffectiveMcpServer {
                            record: s.clone(),
                            overridden_space_default: had_space,
                        },
                    );
                } else if had_space {
                    // Caller's workspace config explicitly disables an inherited global Space MCP.
                    merged_mcp.remove(&key);
                }
            }
        }

        let mut merged_skills: BTreeMap<String, EffectiveSkill> = BTreeMap::new();
        for sk in guard.skills.iter().filter(|sk| {
            &sk.space_id == space_id
                && sk.scope == HubScopeLevel::Space
                && sk.enabled
                && !matches!(sk.lifecycle, SkillLifecycle::Draft | SkillLifecycle::Archived)
        }) {
            merged_skills.insert(
                sk.name.to_ascii_lowercase(),
                EffectiveSkill {
                    record: sk.clone(),
                    overridden_space_default: false,
                },
            );
        }
        if let Some(ws) = workspace_id {
            for sk in guard.skills.iter().filter(|sk| {
                &sk.space_id == space_id
                    && sk.scope == HubScopeLevel::Workspace
                    && sk.workspace_id.as_ref() == Some(ws)
            }) {
                let key = sk.name.to_ascii_lowercase();
                let had_space = merged_skills.contains_key(&key);
                if sk.enabled
                    && !matches!(sk.lifecycle, SkillLifecycle::Draft | SkillLifecycle::Archived)
                {
                    merged_skills.insert(
                        key,
                        EffectiveSkill {
                            record: sk.clone(),
                            overridden_space_default: had_space,
                        },
                    );
                } else if had_space && sk.lifecycle != SkillLifecycle::Draft {
                    merged_skills.remove(&key);
                }
            }
        }

        let mcp_servers: Vec<EffectiveMcpServer> = merged_mcp.into_values().collect();
        let skills: Vec<EffectiveSkill> = merged_skills.into_values().collect();

        let mut client_servers = Map::new();
        for eff in &mcp_servers {
            let entry = match eff.record.transport {
                McpTransport::Stdio => json!({
                    "transport": "stdio",
                    "command": eff.record.command,
                    "args": eff.record.args,
                    "envKeys": eff.record.env_keys,
                }),
                McpTransport::Sse => json!({
                    "transport": "sse",
                    "url": eff.record.url,
                    "envKeys": eff.record.env_keys,
                }),
                McpTransport::StreamableHttp => json!({
                    "transport": "streamable_http",
                    "url": eff.record.url,
                    "envKeys": eff.record.env_keys,
                }),
            };
            client_servers.insert(eff.record.name.clone(), entry);
        }

        EffectiveHubResponse {
            space_id: space_id.clone(),
            workspace_id: workspace_id.cloned(),
            mcp_servers,
            skills,
            mcp_client_config: json!({ "mcpServers": client_servers }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_isolation_and_admin_global_availability_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub-registry.json");
        let store = HubStore::new(Some(path.clone()));
        let space = SpaceId::from("sp-1");
        let ws = WorkspaceId::from("ws-1");
        let admin = MemberId::from("m-admin");
        let alice = MemberId::from("m-alice");
        let bob = MemberId::from("m-bob");

        // Admin creates a global Space MCP server
        let space_mcp = store
            .create_mcp(
                space.clone(),
                admin.clone(),
                true,
                CreateMcpServerRequest {
                    scope: HubScopeLevel::Space,
                    workspace_id: None,
                    name: "github".into(),
                    transport: McpTransport::StreamableHttp,
                    command: None,
                    args: vec![],
                    url: Some("https://mcp.example.com/github".into()),
                    env_keys: vec!["GITHUB_TOKEN".into()],
                    description: "Global GitHub MCP".into(),
                    enabled: true,
                    global_available: Some(true),
                },
            )
            .unwrap();
        assert!(space_mcp.global_available);

        // Alice creates her own Workspace override for `github`
        let alice_mcp = store
            .create_mcp(
                space.clone(),
                alice.clone(),
                false,
                CreateMcpServerRequest {
                    scope: HubScopeLevel::Workspace,
                    workspace_id: Some(ws.clone()),
                    name: "github".into(),
                    transport: McpTransport::Stdio,
                    command: Some("/bin/sh".into()),
                    args: vec!["-c".into(), "true".into()],
                    url: None,
                    env_keys: vec!["REPO_TOKEN".into()],
                    description: "Alice Workspace GitHub MCP".into(),
                    enabled: true,
                    global_available: None,
                },
            )
            .unwrap();
        assert_eq!(alice_mcp.owner_member_id.as_ref(), Some(&alice));
        assert!(!alice_mcp.global_available);

        // Bob can create his OWN Workspace `github` MCP without colliding with Alice
        let bob_mcp = store
            .create_mcp(
                space.clone(),
                bob.clone(),
                false,
                CreateMcpServerRequest {
                    scope: HubScopeLevel::Workspace,
                    workspace_id: Some(ws.clone()),
                    name: "github".into(),
                    transport: McpTransport::StreamableHttp,
                    command: None,
                    args: vec![],
                    url: Some("https://bob-mcp.example.com/github".into()),
                    env_keys: vec!["BOB_GH_TOKEN".into()],
                    description: "Bob Workspace GitHub MCP".into(),
                    enabled: true,
                    global_available: None,
                },
            )
            .unwrap();
        assert_eq!(bob_mcp.owner_member_id.as_ref(), Some(&bob));

        // Bob cannot read, probe, update, or delete Alice's private Workspace MCP
        assert!(store.get_mcp(&space, &alice_mcp.id, &bob, false).is_err());
        assert!(store.probe_mcp(&space, &alice_mcp.id, &bob, false).is_err());
        assert!(
            store
                .update_mcp(
                    &space,
                    &alice_mcp.id,
                    &bob,
                    false,
                    UpdateMcpServerRequest {
                        enabled: Some(false),
                        ..Default::default()
                    },
                )
                .is_err()
        );
        assert!(store.delete_mcp(&space, &alice_mcp.id, &bob, false).is_err());

        // Sensitive env_keys (KEY=VALUE, OM_TOKEN, LD_PRELOAD, OPENAI_API_KEY) are rejected
        for bad_key in [
            "OPENAI_API_KEY=sk-secret",
            "OM_TOKEN",
            "LD_PRELOAD",
            "OPENAI_API_KEY",
            "PATH",
        ] {
            assert!(
                store
                    .create_mcp(
                        space.clone(),
                        admin.clone(),
                        true,
                        CreateMcpServerRequest {
                            scope: HubScopeLevel::Space,
                            workspace_id: None,
                            name: format!("bad-{bad_key}").replace('=', "-"),
                            transport: McpTransport::Stdio,
                            command: Some("npx".into()),
                            args: vec![],
                            url: None,
                            env_keys: vec![bad_key.into()],
                            description: "".into(),
                            enabled: true,
                            global_available: Some(true),
                        },
                    )
                    .is_err(),
                "expected {bad_key} to be rejected"
            );
        }

        // Draft skills are excluded from effective() until promoted to Active
        let skill = store
            .create_skill(
                space.clone(),
                admin.clone(),
                CreateSkillRequest {
                    scope: HubScopeLevel::Space,
                    workspace_id: None,
                    name: "rust-reviewer".into(),
                    version: "1.0.0".into(),
                    description: "Reviews Rust PRs".into(),
                    tags: vec!["rust".into()],
                    instructions: "# Rust Reviewer\nCheck unsafe blocks.".into(),
                    lifecycle: SkillLifecycle::Draft,
                    enabled: true,
                },
            )
            .unwrap();
        assert!(
            store
                .effective(&space, &alice, Some(&ws))
                .skills
                .is_empty(),
            "Draft skill must not appear in effective hub"
        );
        let active_skill = store
            .update_skill(
                &space,
                &skill.id,
                UpdateSkillRequest {
                    lifecycle: Some(SkillLifecycle::Active),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(active_skill.lifecycle, SkillLifecycle::Active);

        // Reload from disk and verify per-member effective views + probe
        let reloaded = HubStore::new(Some(path));
        let eff_alice = reloaded.effective(&space, &alice, Some(&ws));
        assert_eq!(eff_alice.mcp_servers.len(), 1);
        assert_eq!(eff_alice.mcp_servers[0].record.id, alice_mcp.id);
        assert!(eff_alice.mcp_servers[0].overridden_space_default);
        assert_eq!(eff_alice.skills.len(), 1);

        let eff_bob = reloaded.effective(&space, &bob, Some(&ws));
        assert_eq!(eff_bob.mcp_servers.len(), 1);
        assert_eq!(eff_bob.mcp_servers[0].record.id, bob_mcp.id);
        assert!(eff_bob.mcp_servers[0].overridden_space_default);

        // Admin disables global availability of Space `github` -> members no longer see it
        reloaded
            .update_mcp(
                &space,
                &space_mcp.id,
                &admin,
                true,
                UpdateMcpServerRequest {
                    global_available: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        let alice_list = reloaded.list_mcp(&space, &alice, false, None, None);
        assert!(
            alice_list.iter().all(|s| s.id != space_mcp.id),
            "unpublished Space MCP must be hidden from non-admins"
        );
        let admin_list = reloaded.list_mcp(&space, &admin, true, None, None);
        assert!(
            admin_list.iter().any(|s| s.id == space_mcp.id && !s.global_available),
            "admin must still see unpublished Space MCP to manage it"
        );
        let eff_alice_after = reloaded.effective(&space, &alice, Some(&ws));
        assert!(!eff_alice_after.mcp_servers[0].overridden_space_default);

        let probe = reloaded.probe_mcp(&space, &alice_mcp.id, &alice, false).unwrap();
        assert!(probe.reachable);
        assert!(
            !probe.detail.contains("/bin/sh"),
            "probe detail must not leak host filesystem path"
        );
    }
}
