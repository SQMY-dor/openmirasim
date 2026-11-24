//! Space / Workspace two-tier MCP Hub & Skills lifecycle registry (Phase 4).
//!
//! Architecture & safety invariants:
//! - Two-tier scope model: `Space`-level entries act as team-wide defaults; `Workspace`-level
//!   entries can add repo-specific tools/skills or override a `Space`-level entry sharing the
//!   same slug `name`.
//! - Secret isolation: MCP server configs store ONLY environment variable key names
//!   (`env_keys`, e.g. `GITHUB_TOKEN`), never plaintext secret values.
//! - Persistence: stored atomically (temp-file + rename) under `<OM_DATA_DIR>/hub-registry.json`
//!   so restarts preserve every registered MCP server and Skill lifecycle state.

use om_core::ids::{MemberId, SpaceId, WorkspaceId};
use om_core::{CoreError, CoreResult, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MAX_MCP_SERVERS_PER_SPACE: usize = 100;
pub const MAX_SKILLS_PER_SPACE: usize = 200;
pub const MAX_SKILL_INSTRUCTIONS_BYTES: usize = 64 * 1024;

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
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateMcpServerRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
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

fn validate_env_keys(keys: &[String]) -> CoreResult<Vec<String>> {
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
        if !out.iter().any(|existing: &String| existing == k) {
            out.push(k.to_string());
        }
    }
    Ok(out)
}

fn validate_mcp_transport(
    transport: McpTransport,
    command: &Option<String>,
    url: &Option<String>,
) -> CoreResult<(Option<String>, Option<String>)> {
    match transport {
        McpTransport::Stdio => {
            let cmd = command
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
            Ok((Some(cmd.to_string()), None))
        }
        McpTransport::Sse | McpTransport::StreamableHttp => {
            let raw_url = url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CoreError::InvalidInput("HTTP/SSE MCP server requires a valid url".into())
                })?;
            let parsed = reqwest::Url::parse(raw_url)
                .map_err(|e| CoreError::InvalidInput(format!("invalid MCP url: {e}")))?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(CoreError::InvalidInput(
                    "MCP server url must use http or https".into(),
                ));
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(CoreError::InvalidInput(
                    "MCP server url must not contain embedded credentials".into(),
                ));
            }
            if command.as_deref().is_some_and(|c| !c.trim().is_empty()) {
                return Err(CoreError::InvalidInput(
                    "HTTP/SSE MCP server must not set command".into(),
                ));
            }
            Ok((None, Some(raw_url.to_string())))
        }
    }
}

fn resolve_command_on_path(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() || cmd.contains('/') {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(cmd))
        .find(|candidate| candidate.is_file())
}

/// Thread-safe, optionally file-backed two-tier MCP Hub & Skills registry.
#[derive(Clone)]
pub struct HubStore {
    inner: Arc<Mutex<PersistedHubState>>,
    persist_path: Option<PathBuf>,
}

impl HubStore {
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let initial = persist_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<PersistedHubState>(&bytes).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(initial)),
            persist_path,
        }
    }

    fn flush_locked(&self, state: &PersistedHubState) -> CoreResult<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| CoreError::Storage(format!("serialize hub registry: {e}")))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn list_mcp(
        &self,
        space_id: &SpaceId,
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
            .filter(|s| scope.is_none_or(|sc| s.scope == sc))
            .filter(|s| workspace_id.is_none_or(|w| s.workspace_id.as_ref() == Some(w)))
            .cloned()
            .collect()
    }

    pub fn get_mcp(&self, space_id: &SpaceId, id: &str) -> CoreResult<McpServerRecord> {
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
        req: CreateMcpServerRequest,
    ) -> CoreResult<McpServerRecord> {
        validate_scope(req.scope, &req.workspace_id)?;
        let name = validate_slug("mcp server", &req.name)?;
        let (command, url) = validate_mcp_transport(req.transport, &req.command, &req.url)?;
        let env_keys = validate_env_keys(&req.env_keys)?;

        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let space_count = guard
            .mcp_servers
            .iter()
            .filter(|s| s.space_id == space_id)
            .count();
        let duplicate = guard.mcp_servers.iter().any(|s| {
            s.space_id == space_id
                && s.scope == req.scope
                && s.workspace_id == req.workspace_id
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

        let now = now_ms();
        let record = McpServerRecord {
            id: format!("mcp_{}", uuid::Uuid::now_v7().simple()),
            space_id,
            scope: req.scope,
            workspace_id: req.workspace_id,
            name,
            transport: req.transport,
            command,
            args: req.args,
            url,
            env_keys,
            description: req.description.trim().to_string(),
            enabled: req.enabled,
            created_by: by,
            created_at: now,
            updated_at: now,
        };
        guard.mcp_servers.insert(0, record.clone());
        self.flush_locked(&guard)?;
        Ok(record)
    }

    pub fn update_mcp(
        &self,
        space_id: &SpaceId,
        id: &str,
        req: UpdateMcpServerRequest,
    ) -> CoreResult<McpServerRecord> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = guard
            .mcp_servers
            .iter()
            .position(|s| &s.space_id == space_id && s.id == id)
            .ok_or_else(|| CoreError::NotFound(format!("mcp server {id}")))?;
        let mut updated = guard.mcp_servers[idx].clone();
        if let Some(enabled) = req.enabled {
            updated.enabled = enabled;
        }
        if let Some(desc) = req.description {
            updated.description = desc.trim().to_string();
        }
        if let Some(args) = req.args {
            updated.args = args;
        }
        if let Some(keys) = req.env_keys {
            updated.env_keys = validate_env_keys(&keys)?;
        }
        let next_cmd = req.command.or(updated.command.clone());
        let next_url = req.url.or(updated.url.clone());
        let (cmd, url) = validate_mcp_transport(updated.transport, &next_cmd, &next_url)?;
        updated.command = cmd;
        updated.url = url;
        updated.updated_at = now_ms();
        guard.mcp_servers[idx] = updated.clone();
        self.flush_locked(&guard)?;
        Ok(updated)
    }

    pub fn delete_mcp(&self, space_id: &SpaceId, id: &str) -> CoreResult<McpServerRecord> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = guard
            .mcp_servers
            .iter()
            .position(|s| &s.space_id == space_id && s.id == id)
            .ok_or_else(|| CoreError::NotFound(format!("mcp server {id}")))?;
        let removed = guard.mcp_servers.remove(idx);
        self.flush_locked(&guard)?;
        Ok(removed)
    }

    pub fn probe_mcp(&self, space_id: &SpaceId, id: &str) -> CoreResult<McpProbeResult> {
        let rec = self.get_mcp(space_id, id)?;
        let (reachable, detail) = match rec.transport {
            McpTransport::Stdio => {
                let cmd = rec.command.as_deref().unwrap_or_default();
                if let Some(resolved) = resolve_command_on_path(cmd) {
                    (true, format!("executable ready at {}", resolved.display()))
                } else {
                    (false, format!("command `{cmd}` not found in server PATH"))
                }
            }
            McpTransport::Sse | McpTransport::StreamableHttp => {
                let url = rec.url.as_deref().unwrap_or_default();
                match reqwest::Url::parse(url) {
                    Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => (
                        true,
                        format!(
                            "validated {} endpoint {}",
                            u.scheme(),
                            u.host_str().unwrap_or_default()
                        ),
                    ),
                    _ => (false, format!("invalid endpoint URL `{url}`")),
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
            description: req.description.trim().to_string(),
            tags,
            instructions,
            lifecycle: req.lifecycle,
            enabled,
            created_by: by,
            created_at: now,
            updated_at: now,
        };
        guard.skills.insert(0, record.clone());
        self.flush_locked(&guard)?;
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
            updated.description = desc.trim().to_string();
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
        guard.skills[idx] = updated.clone();
        self.flush_locked(&guard)?;
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
        let removed = guard.skills.remove(idx);
        self.flush_locked(&guard)?;
        Ok(removed)
    }

    /// Computes effective two-tier inheritance (Space defaults overridden by matching Workspace entries).
    pub fn effective(
        &self,
        space_id: &SpaceId,
        workspace_id: Option<&WorkspaceId>,
    ) -> EffectiveHubResponse {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut merged_mcp: BTreeMap<String, EffectiveMcpServer> = BTreeMap::new();
        for s in guard
            .mcp_servers
            .iter()
            .filter(|s| &s.space_id == space_id && s.scope == HubScopeLevel::Space && s.enabled)
        {
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
                    // Workspace explicitly disables an inherited Space MCP server of the same name.
                    merged_mcp.remove(&key);
                }
            }
        }

        let mut merged_skills: BTreeMap<String, EffectiveSkill> = BTreeMap::new();
        for sk in guard.skills.iter().filter(|sk| {
            &sk.space_id == space_id
                && sk.scope == HubScopeLevel::Space
                && sk.enabled
                && sk.lifecycle != SkillLifecycle::Archived
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
                if sk.enabled && sk.lifecycle != SkillLifecycle::Archived {
                    merged_skills.insert(
                        key,
                        EffectiveSkill {
                            record: sk.clone(),
                            overridden_space_default: had_space,
                        },
                    );
                } else if had_space {
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
    fn two_tier_inheritance_overrides_space_defaults_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub-registry.json");
        let store = HubStore::new(Some(path.clone()));
        let space = SpaceId::from("sp-1");
        let ws = WorkspaceId::from("ws-1");
        let owner = MemberId::from("m-owner");

        // Space default MCP server + Skill
        store
            .create_mcp(
                space.clone(),
                owner.clone(),
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
                },
            )
            .unwrap();

        // Workspace overrides `github` with local stdio server
        let ws_mcp = store
            .create_mcp(
                space.clone(),
                owner.clone(),
                CreateMcpServerRequest {
                    scope: HubScopeLevel::Workspace,
                    workspace_id: Some(ws.clone()),
                    name: "github".into(),
                    transport: McpTransport::Stdio,
                    command: Some("/bin/sh".into()),
                    args: vec!["-c".into(), "true".into()],
                    url: None,
                    env_keys: vec!["REPO_TOKEN".into()],
                    description: "Workspace GitHub MCP".into(),
                    enabled: true,
                },
            )
            .unwrap();

        // Secret value in env_keys must be rejected
        assert!(
            store
                .create_mcp(
                    space.clone(),
                    owner.clone(),
                    CreateMcpServerRequest {
                        scope: HubScopeLevel::Space,
                        workspace_id: None,
                        name: "bad-secret".into(),
                        transport: McpTransport::Stdio,
                        command: Some("npx".into()),
                        args: vec![],
                        url: None,
                        env_keys: vec!["OPENAI_API_KEY=sk-secret".into()],
                        description: "".into(),
                        enabled: true,
                    },
                )
                .is_err()
        );

        let skill = store
            .create_skill(
                space.clone(),
                owner.clone(),
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

        // Reload from disk and verify effective inheritance + probe
        let reloaded = HubStore::new(Some(path));
        let eff = reloaded.effective(&space, Some(&ws));
        assert_eq!(eff.mcp_servers.len(), 1);
        assert_eq!(eff.mcp_servers[0].record.id, ws_mcp.id);
        assert!(eff.mcp_servers[0].overridden_space_default);
        assert_eq!(eff.skills.len(), 1);
        assert_eq!(eff.skills[0].record.name, "rust-reviewer");

        let probe = reloaded.probe_mcp(&space, &ws_mcp.id).unwrap();
        assert!(probe.reachable);
    }
}
