//! Workspaces (repositories registered on the server) and per-session Git worktrees (W1-2).

use crate::ids::{SessionId, SpaceId, WorkspaceId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A Git repository registered on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub space_id: SpaceId,
    pub name: String,
    /// Absolute path of the repository's top-level directory on the server.
    pub repo_path: String,
    /// Branch new worktrees start from unless a base is given.
    pub default_branch: String,
    #[ts(type = "number")]
    pub created_at: i64,
}

/// The isolated checkout a session runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorktreeInfo {
    pub session_id: SessionId,
    /// `None` for a scratch directory (sandboxed session without a workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_id: Option<WorkspaceId>,
    /// Absolute path of the checkout; the session's `cwd`.
    pub path: String,
    /// Branch created for the session (e.g. `om/0192f…`); `None` for scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub branch: Option<String>,
    /// Commit the worktree was created from; diffs are taken against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub base_commit: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    /// Set once the checkout has been removed from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub removed_at: Option<i64>,
}

/// How a file differs from the worktree's base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Not tracked by Git yet (and not ignored).
    Untracked,
}

/// One changed path in a [`WorktreeDiff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileChange {
    /// Path relative to the worktree root.
    pub path: String,
    pub kind: FileChangeKind,
    /// Previous path, for renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub old_path: Option<String>,
}

/// Everything the session changed relative to its base commit, committed or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorktreeDiff {
    pub base_commit: String,
    pub files: Vec<FileChange>,
    /// Unified diff (untracked files included as new files).
    pub patch: String,
    /// `patch` was cut at the server's size limit.
    pub truncated: bool,
}

/// An entry in the worktree file tree (tracked + untracked, ignored files excluded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorktreeEntry {
    /// Path relative to the worktree root, `/`-separated.
    pub path: String,
    #[ts(type = "number")]
    pub size: u64,
}
