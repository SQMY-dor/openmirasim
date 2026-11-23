//! Workspace and worktree rows (schema v2, W1-2).

use super::{Db, storage};
use om_core::ids::{SessionId, SpaceId, WorkspaceId};
use om_core::workspace::{WorkspaceSummary, WorktreeInfo};
use om_core::{CoreError, CoreResult, now_ms};
use rusqlite::{OptionalExtension, params};

/// A workspace row plus server-internal fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWorkspace {
    pub summary: WorkspaceSummary,
    /// Absolute `--git-common-dir` (normally `<repo>/.git`), resolved at registration.
    pub git_common_dir: String,
}

/// A worktree row plus server-internal fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWorktree {
    pub info: WorktreeInfo,
    /// The worktree's private git admin dir (`<common>/worktrees/<name>`), recorded at
    /// creation so the server never trusts the agent-writable `.git` file in the checkout.
    pub git_dir: Option<String>,
    /// Private HOME bound into the sandbox.
    pub home_path: Option<String>,
}

const WORKSPACE_COLUMNS: &str = "SELECT id, space_id, name, repo_path, default_branch, created_at,
                                        git_common_dir FROM workspaces";
const WORKTREE_COLUMNS: &str = "SELECT session_id, workspace_id, path, branch, base_commit,
                                       home_path, created_at, removed_at, git_dir FROM worktrees";

fn row_to_workspace(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredWorkspace> {
    Ok(StoredWorkspace {
        summary: WorkspaceSummary {
            id: WorkspaceId(r.get(0)?),
            space_id: SpaceId(r.get(1)?),
            name: r.get(2)?,
            repo_path: r.get(3)?,
            default_branch: r.get(4)?,
            created_at: r.get(5)?,
        },
        git_common_dir: r.get(6)?,
    })
}

fn row_to_worktree(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredWorktree> {
    Ok(StoredWorktree {
        info: WorktreeInfo {
            session_id: SessionId(r.get(0)?),
            workspace_id: r.get::<_, Option<String>>(1)?.map(WorkspaceId),
            path: r.get(2)?,
            branch: r.get(3)?,
            base_commit: r.get(4)?,
            created_at: r.get(6)?,
            removed_at: r.get(7)?,
        },
        git_dir: r.get(8)?,
        home_path: r.get(5)?,
    })
}

impl Db {
    /// Registers a repository. Names are unique per space.
    pub async fn create_workspace(&self, ws: StoredWorkspace) -> CoreResult<StoredWorkspace> {
        self.write(move |conn| {
            let s = &ws.summary;
            conn.execute(
                "INSERT INTO workspaces (id, space_id, name, repo_path, git_common_dir,
                                         default_branch, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    s.id.as_str(),
                    s.space_id.as_str(),
                    s.name,
                    s.repo_path,
                    ws.git_common_dir,
                    s.default_branch,
                    s.created_at
                ],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CoreError::InvalidInput(format!(
                        "a workspace named `{}` already exists in this space",
                        ws.summary.name
                    ))
                }
                other => storage(other),
            })?;
            Ok(ws)
        })
        .await
    }

    pub async fn get_workspace(&self, id: &WorkspaceId) -> CoreResult<StoredWorkspace> {
        let id = id.clone();
        self.read(move |conn| {
            conn.query_row(
                &format!("{WORKSPACE_COLUMNS} WHERE id = ?1"),
                [id.as_str()],
                row_to_workspace,
            )
            .optional()
            .map_err(storage)?
            .ok_or_else(|| CoreError::NotFound(format!("workspace {id}")))
        })
        .await
    }

    /// Lists a space's workspaces by name.
    pub async fn list_workspaces(&self, space_id: &SpaceId) -> CoreResult<Vec<StoredWorkspace>> {
        let space_id = space_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{WORKSPACE_COLUMNS} WHERE space_id = ?1 ORDER BY name"
                ))
                .map_err(storage)?;
            stmt.query_map([space_id.as_str()], row_to_workspace)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    pub async fn insert_worktree(&self, wt: StoredWorktree) -> CoreResult<()> {
        self.write(move |conn| {
            let i = &wt.info;
            conn.execute(
                "INSERT INTO worktrees (session_id, workspace_id, path, branch, base_commit,
                                        home_path, created_at, removed_at, git_dir)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    i.session_id.as_str(),
                    i.workspace_id.as_ref().map(|w| w.as_str().to_owned()),
                    i.path,
                    i.branch,
                    i.base_commit,
                    wt.home_path,
                    i.created_at,
                    i.removed_at,
                    wt.git_dir
                ],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    /// The worktree of a session, if it has one.
    pub async fn get_worktree(&self, session_id: &SessionId) -> CoreResult<Option<StoredWorktree>> {
        let id = session_id.clone();
        self.read(move |conn| {
            conn.query_row(
                &format!("{WORKTREE_COLUMNS} WHERE session_id = ?1"),
                [id.as_str()],
                row_to_worktree,
            )
            .optional()
            .map_err(storage)
        })
        .await
    }

    /// Worktrees of a workspace that are still on disk.
    pub async fn list_live_worktrees(
        &self,
        workspace_id: &WorkspaceId,
    ) -> CoreResult<Vec<StoredWorktree>> {
        let id = workspace_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{WORKTREE_COLUMNS} WHERE workspace_id = ?1 AND removed_at IS NULL
                     ORDER BY created_at"
                ))
                .map_err(storage)?;
            stmt.query_map([id.as_str()], row_to_worktree)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    pub async fn mark_worktree_removed(&self, session_id: &SessionId) -> CoreResult<()> {
        let id = session_id.clone();
        self.write(move |conn| {
            conn.execute(
                "UPDATE worktrees SET removed_at = ?2 WHERE session_id = ?1 AND removed_at IS NULL",
                params![id.as_str(), now_ms()],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }
}
