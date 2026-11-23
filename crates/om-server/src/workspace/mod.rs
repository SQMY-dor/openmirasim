//! Workspaces and per-session Git worktrees (W1-2).
//!
//! Layout under the manager's root (normally `$OM_DATA_DIR`):
//!
//! ```text
//! worktrees/<uuid>/   one checkout per session, on branch `om/<uuid>`
//! scratch/<uuid>/     empty working dir for sandboxed sessions without a workspace
//! homes/<member>/     private HOME per member, bound into the sandbox
//! ```
//!
//! Worktree creation is serialized per repository (git takes repository-wide locks).
//! Sessions keep their checkout after closing so their changes can still be reviewed;
//! [`WorkspaceManager::remove_worktree`] deletes it (the branch is kept).

mod git;
#[cfg(test)]
mod tests;

use crate::db::{Db, StoredWorkspace, StoredWorktree};
use git::{Git, split_z};
use om_core::ids::{MemberId, SessionId, SpaceId, WorkspaceId};
use om_core::workspace::{
    FileChange, FileChangeKind, WorkspaceSummary, WorktreeDiff, WorktreeEntry, WorktreeInfo,
};
use om_core::{CoreError, CoreResult, now_ms};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Settings for [`WorkspaceManager`].
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// Root directory for worktrees, scratch dirs and homes.
    pub root: PathBuf,
    pub git_bin: PathBuf,
    /// Maximum size of a diff patch returned to clients.
    pub max_patch_bytes: usize,
    /// Maximum size of a file returned by [`WorkspaceManager::read_file`].
    pub max_file_bytes: u64,
    /// Maximum number of entries returned by [`WorkspaceManager::list_files`].
    pub max_entries: usize,
}

impl WorkspaceConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            git_bin: PathBuf::from("git"),
            max_patch_bytes: 2 << 20,
            max_file_bytes: 1 << 20,
            max_entries: 20_000,
        }
    }
}

/// Paths the sandbox must expose for a session's working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPaths {
    pub writable: Vec<PathBuf>,
    /// Read-only overlays; may be nested inside `writable` paths.
    pub readonly: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

/// A checkout prepared before its session row exists.
#[derive(Debug, Clone)]
pub struct PreparedWorktree {
    pub(crate) stored: StoredWorktree,
    repo: Option<(PathBuf, PathBuf)>,
}

impl PreparedWorktree {
    /// The session's working directory.
    pub fn cwd(&self) -> &str {
        &self.stored.info.path
    }
}

/// Registers repositories and manages session checkouts.
#[derive(Clone)]
pub struct WorkspaceManager {
    db: Db,
    config: Arc<WorkspaceConfig>,
    repo_locks: Arc<Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>>,
}

fn io(context: &str, e: std::io::Error) -> CoreError {
    CoreError::Workspace(format!("{context}: {e}"))
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Member ids become directory names: keep them boring.
fn safe_component(s: &str) -> CoreResult<&str> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(s)
    } else {
        Err(CoreError::InvalidInput(format!(
            "unsafe identifier for a path: {s:?}"
        )))
    }
}

impl WorkspaceManager {
    /// Creates the root layout. The root is canonicalized.
    pub fn new(db: Db, mut config: WorkspaceConfig) -> CoreResult<Self> {
        for sub in ["worktrees", "scratch", "homes"] {
            std::fs::create_dir_all(config.root.join(sub))
                .map_err(|e| io("creating workspace root", e))?;
        }
        config.root = config
            .root
            .canonicalize()
            .map_err(|e| io("resolving workspace root", e))?;
        Ok(Self {
            db,
            config: Arc::new(config),
            repo_locks: Arc::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.config.root
    }

    fn lock_for(&self, id: &WorkspaceId) -> Arc<tokio::sync::Mutex<()>> {
        self.repo_locks
            .lock()
            .expect("repo locks poisoned")
            .entry(id.clone())
            .or_default()
            .clone()
    }

    // ------------------------------------------------------------ workspaces

    /// Registers an existing non-bare Git repository by the path of its top level.
    pub async fn register(
        &self,
        space_id: SpaceId,
        name: &str,
        repo_path: &str,
    ) -> CoreResult<WorkspaceSummary> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 {
            return Err(CoreError::InvalidInput(
                "workspace name must be 1-100 characters".into(),
            ));
        }
        let path = Path::new(repo_path);
        if !path.is_absolute() {
            return Err(CoreError::InvalidInput(format!(
                "repository path must be absolute: {repo_path}"
            )));
        }
        let path = path
            .canonicalize()
            .map_err(|e| CoreError::InvalidInput(format!("repository path {repo_path}: {e}")))?;
        if path.starts_with(&self.config.root) || self.config.root.starts_with(&path) {
            return Err(CoreError::InvalidInput(
                "repository must not overlap the server's data directory".into(),
            ));
        }
        let git = Git::at(&self.config.git_bin, &path);
        let bare = git
            .line("rev-parse", ["rev-parse", "--is-bare-repository"])
            .await
            .map_err(|_| {
                CoreError::InvalidInput(format!("not a git repository: {}", path.display()))
            })?;
        if bare == "true" {
            return Err(CoreError::InvalidInput(
                "bare repositories are not supported".into(),
            ));
        }
        let top = git
            .line("rev-parse", ["rev-parse", "--show-toplevel"])
            .await?;
        if Path::new(&top) != path {
            return Err(CoreError::InvalidInput(format!(
                "{} is inside a repository; register its top level {top}",
                path.display()
            )));
        }
        let common = git
            .line(
                "rev-parse",
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?;
        let default_branch = git
            .line(
                "symbolic-ref",
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
            )
            .await
            .map_err(|_| CoreError::InvalidInput("repository HEAD is detached".into()))?;
        git.run(
            "rev-parse",
            ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        )
        .await
        .map_err(|_| CoreError::InvalidInput("repository has no commits yet".into()))?;
        // Make sure the read-only overlays have something to cover.
        let common_dir = PathBuf::from(&common);
        for d in ["hooks", "info"] {
            std::fs::create_dir_all(common_dir.join(d)).map_err(|e| io("preparing git dir", e))?;
        }
        let stored = StoredWorkspace {
            summary: WorkspaceSummary {
                id: WorkspaceId::new(),
                space_id,
                name: name.to_string(),
                repo_path: path_str(&path),
                default_branch,
                created_at: now_ms(),
            },
            git_common_dir: common,
        };
        Ok(self.db.create_workspace(stored).await?.summary)
    }

    pub async fn list(&self, space_id: &SpaceId) -> CoreResult<Vec<WorkspaceSummary>> {
        Ok(self
            .db
            .list_workspaces(space_id)
            .await?
            .into_iter()
            .map(|w| w.summary)
            .collect())
    }

    pub async fn get(&self, id: &WorkspaceId) -> CoreResult<WorkspaceSummary> {
        Ok(self.db.get_workspace(id).await?.summary)
    }

    // ------------------------------------------------------------ worktrees

    fn member_home(&self, member: Option<&MemberId>) -> CoreResult<PathBuf> {
        let name = match member {
            Some(m) => safe_component(m.as_str())?,
            None => "_anonymous",
        };
        let home = self.config.root.join("homes").join(name);
        std::fs::create_dir_all(&home).map_err(|e| io("creating member home", e))?;
        Ok(home)
    }

    /// Creates a checkout on a new branch `om/<uuid>` from `base` (default: the
    /// workspace's default branch), or a scratch dir when `workspace_id` is `None`.
    /// Attach it with [`WorkspaceManager::attach`] once the session row exists.
    pub async fn prepare(
        &self,
        workspace_id: Option<&WorkspaceId>,
        base: Option<&str>,
        member: Option<&MemberId>,
    ) -> CoreResult<PreparedWorktree> {
        let home = path_str(&self.member_home(member)?);
        let key = uuid::Uuid::now_v7().simple().to_string();
        let Some(ws_id) = workspace_id else {
            let dir = self.config.root.join("scratch").join(&key);
            std::fs::create_dir_all(&dir).map_err(|e| io("creating scratch dir", e))?;
            return Ok(PreparedWorktree {
                stored: StoredWorktree {
                    info: WorktreeInfo {
                        session_id: SessionId(String::new()),
                        workspace_id: None,
                        path: path_str(&dir),
                        branch: None,
                        base_commit: None,
                        created_at: now_ms(),
                        removed_at: None,
                    },
                    git_dir: None,
                    home_path: Some(home),
                },
                repo: None,
            });
        };
        let ws = self.db.get_workspace(ws_id).await?;
        let repo = PathBuf::from(&ws.summary.repo_path);
        let git = Git::at(&self.config.git_bin, &repo);
        let base = base.unwrap_or(&ws.summary.default_branch);
        if base.starts_with('-') {
            return Err(CoreError::InvalidInput(format!(
                "invalid base revision {base:?}"
            )));
        }
        let base_commit = git
            .line(
                "rev-parse",
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &format!("{base}^{{commit}}"),
                ],
            )
            .await
            .map_err(|_| CoreError::InvalidInput(format!("unknown base revision {base:?}")))?;
        let path = self.config.root.join("worktrees").join(&key);
        let branch = format!("om/{key}");

        let lock = self.lock_for(ws_id);
        let _guard = lock.lock().await;
        git.run(
            "worktree add",
            [
                "worktree",
                "add",
                "--quiet",
                "--no-track",
                "-b",
                &branch,
                &path_str(&path),
                &base_commit,
            ],
        )
        .await?;
        // Record the admin dir now, while the checkout's `.git` file is still ours.
        let git_dir = match Git::at(&self.config.git_bin, &path)
            .line(
                "rev-parse",
                ["rev-parse", "--path-format=absolute", "--git-dir"],
            )
            .await
        {
            Ok(d) => PathBuf::from(d),
            Err(e) => {
                let _ = git
                    .run(
                        "worktree remove",
                        ["worktree", "remove", "--force", &path_str(&path)],
                    )
                    .await;
                return Err(e);
            }
        };
        let expected_parent = Path::new(&ws.git_common_dir).join("worktrees");
        if git_dir.parent() != Some(expected_parent.as_path()) {
            let _ = git
                .run(
                    "worktree remove",
                    ["worktree", "remove", "--force", &path_str(&path)],
                )
                .await;
            return Err(CoreError::Workspace(format!(
                "unexpected worktree git dir {}",
                git_dir.display()
            )));
        }
        // Overlay target for repositories using extensions.worktreeConfig.
        let cfg = git_dir.join("config.worktree");
        if !cfg.exists() {
            std::fs::write(&cfg, b"").map_err(|e| io("preparing worktree config", e))?;
        }
        Ok(PreparedWorktree {
            stored: StoredWorktree {
                info: WorktreeInfo {
                    session_id: SessionId(String::new()),
                    workspace_id: Some(ws_id.clone()),
                    path: path_str(&path),
                    branch: Some(branch),
                    base_commit: Some(base_commit),
                    created_at: now_ms(),
                    removed_at: None,
                },
                git_dir: Some(path_str(&git_dir)),
                home_path: Some(home),
            },
            repo: Some((repo, PathBuf::from(ws.git_common_dir))),
        })
    }

    /// Records a prepared checkout for its session.
    pub async fn attach(
        &self,
        session_id: &SessionId,
        mut prepared: PreparedWorktree,
    ) -> CoreResult<WorktreeInfo> {
        prepared.stored.info.session_id = session_id.clone();
        let info = prepared.stored.info.clone();
        self.db.insert_worktree(prepared.stored).await?;
        Ok(info)
    }

    /// Deletes a prepared checkout that never got a session (e.g. the insert failed).
    pub async fn discard(&self, prepared: PreparedWorktree) {
        let path = PathBuf::from(&prepared.stored.info.path);
        match &prepared.repo {
            Some((repo, _)) => {
                let git = Git::at(&self.config.git_bin, repo);
                let _ = git
                    .run(
                        "worktree remove",
                        ["worktree", "remove", "--force", &path_str(&path)],
                    )
                    .await;
                if let Some(b) = &prepared.stored.info.branch {
                    let _ = git.run("branch", ["branch", "-D", "--", b]).await;
                }
            }
            None => {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    pub async fn worktree(&self, session_id: &SessionId) -> CoreResult<StoredWorktree> {
        self.db
            .get_worktree(session_id)
            .await?
            .filter(|w| w.info.removed_at.is_none())
            .ok_or_else(|| CoreError::NotFound(format!("worktree of session {session_id}")))
    }

    /// What the sandbox must expose for this checkout: the checkout and the repository's
    /// git dir read-write (so the agent can commit), with every file git reads config
    /// or redirections from overlaid read-only, plus the member's private HOME.
    pub async fn sandbox_paths(&self, wt: &StoredWorktree) -> CoreResult<SandboxPaths> {
        let mut paths = SandboxPaths {
            writable: vec![PathBuf::from(&wt.info.path)],
            readonly: Vec::new(),
            home: wt.home_path.as_ref().map(PathBuf::from),
        };
        if let (Some(ws_id), Some(git_dir)) = (&wt.info.workspace_id, &wt.git_dir) {
            let ws = self.db.get_workspace(ws_id).await?;
            let common = PathBuf::from(&ws.git_common_dir);
            let admin = PathBuf::from(git_dir);
            paths.writable.push(common.clone());
            for p in [
                common.join("config"),
                common.join("hooks"),
                common.join("info"),
                common.join("commondir"),
                admin.join("commondir"),
                admin.join("gitdir"),
                admin.join("config.worktree"),
            ] {
                if p.exists() {
                    paths.readonly.push(p);
                }
            }
        }
        Ok(paths)
    }

    fn checkout_git(&self, wt: &StoredWorktree) -> CoreResult<Git> {
        let git_dir = wt.git_dir.as_ref().ok_or_else(|| CoreError::Unsupported {
            harness: "workspace".into(),
            capability: "git operations on a scratch directory",
        })?;
        Ok(Git::worktree(&self.config.git_bin, git_dir, &wt.info.path))
    }

    /// Changes relative to the base commit: committed, staged, unstaged and untracked.
    pub async fn diff(&self, session_id: &SessionId) -> CoreResult<WorktreeDiff> {
        let wt = self.worktree(session_id).await?;
        let git = self.checkout_git(&wt)?;
        let base = wt.info.base_commit.clone().unwrap_or_default();
        let diff_opts = ["--no-ext-diff", "--no-textconv", "--no-color", "-M"];

        let mut args = vec!["diff", "--name-status", "-z"];
        args.extend(diff_opts);
        args.extend([base.as_str(), "--"]);
        let names = split_z(&git.run("diff --name-status", &args).await?);
        let mut files = Vec::new();
        let mut it = names.into_iter();
        while let Some(status) = it.next() {
            let kind = match status.chars().next() {
                Some('A') => FileChangeKind::Added,
                Some('D') => FileChangeKind::Deleted,
                Some('R') | Some('C') => FileChangeKind::Renamed,
                _ => FileChangeKind::Modified,
            };
            if kind == FileChangeKind::Renamed {
                let (Some(old), Some(new)) = (it.next(), it.next()) else {
                    break;
                };
                files.push(FileChange {
                    path: new,
                    kind,
                    old_path: Some(old),
                });
            } else if let Some(path) = it.next() {
                files.push(FileChange {
                    path,
                    kind,
                    old_path: None,
                });
            }
        }

        let mut args = vec!["diff"];
        args.extend(diff_opts);
        args.extend([base.as_str(), "--"]);
        let mut patch = git.run("diff", &args).await?;

        let untracked = split_z(
            &git.run(
                "ls-files",
                ["ls-files", "-z", "--others", "--exclude-standard"],
            )
            .await?,
        );
        for path in untracked {
            if patch.len() >= self.config.max_patch_bytes {
                files.push(FileChange {
                    path,
                    kind: FileChangeKind::Untracked,
                    old_path: None,
                });
                continue;
            }
            // `--no-index` exits 1 when files differ; that is the normal case here.
            let out = git
                .output([
                    "diff",
                    "--no-index",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--",
                    "/dev/null",
                    &path,
                ])
                .await?;
            if out.status.code().is_some_and(|c| c <= 1) {
                patch.extend_from_slice(&out.stdout);
            }
            files.push(FileChange {
                path,
                kind: FileChangeKind::Untracked,
                old_path: None,
            });
        }
        let truncated = patch.len() > self.config.max_patch_bytes;
        if truncated {
            patch.truncate(self.config.max_patch_bytes);
        }
        Ok(WorktreeDiff {
            base_commit: base,
            files,
            patch: String::from_utf8_lossy(&patch).into_owned(),
            truncated,
        })
    }

    /// Tracked and untracked (not ignored) files, sorted.
    pub async fn list_files(&self, session_id: &SessionId) -> CoreResult<Vec<WorktreeEntry>> {
        let wt = self.worktree(session_id).await?;
        let root = PathBuf::from(&wt.info.path);
        let paths = match self.checkout_git(&wt) {
            Ok(git) => split_z(
                &git.run(
                    "ls-files",
                    [
                        "ls-files",
                        "-z",
                        "--cached",
                        "--others",
                        "--exclude-standard",
                        "--deduplicate",
                    ],
                )
                .await?,
            ),
            Err(_) => walk_dir(&root, self.config.max_entries),
        };
        let mut entries: Vec<WorktreeEntry> = paths
            .into_iter()
            .filter_map(|p| {
                // `symlink_metadata`: never follow links out of the checkout.
                let meta = std::fs::symlink_metadata(root.join(&p)).ok()?;
                (!meta.is_dir()).then(|| WorktreeEntry {
                    path: p,
                    size: meta.len(),
                })
            })
            .take(self.config.max_entries)
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Reads a file inside the checkout. Rejects `..`, absolute paths, and symlinks that
    /// resolve outside the checkout.
    pub async fn read_file(&self, session_id: &SessionId, rel: &str) -> CoreResult<Vec<u8>> {
        let wt = self.worktree(session_id).await?;
        let root = Path::new(&wt.info.path)
            .canonicalize()
            .map_err(|e| io("resolving worktree", e))?;
        let rel_path = Path::new(rel);
        if rel.is_empty()
            || rel_path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(CoreError::InvalidInput(format!(
                "invalid relative path {rel:?}"
            )));
        }
        let full = root
            .join(rel_path)
            .canonicalize()
            .map_err(|_| CoreError::NotFound(format!("file {rel}")))?;
        if !full.starts_with(&root) || full.starts_with(root.join(".git")) {
            return Err(CoreError::InvalidInput(format!(
                "path escapes the worktree: {rel}"
            )));
        }
        let meta = std::fs::metadata(&full).map_err(|e| io("reading file", e))?;
        if !meta.is_file() {
            return Err(CoreError::InvalidInput(format!(
                "not a regular file: {rel}"
            )));
        }
        if meta.len() > self.config.max_file_bytes {
            return Err(CoreError::InvalidInput(format!(
                "file is larger than {} bytes",
                self.config.max_file_bytes
            )));
        }
        tokio::fs::read(&full)
            .await
            .map_err(|e| io("reading file", e))
    }

    /// Removes the checkout from disk (the branch and its commits are kept).
    pub async fn remove_worktree(&self, session_id: &SessionId) -> CoreResult<()> {
        let wt = self.worktree(session_id).await?;
        match (&wt.info.workspace_id, &wt.git_dir) {
            (Some(ws_id), Some(_)) => {
                let ws = self.db.get_workspace(ws_id).await?;
                let lock = self.lock_for(ws_id);
                let _guard = lock.lock().await;
                let git = Git::at(&self.config.git_bin, &ws.summary.repo_path);
                git.run(
                    "worktree remove",
                    ["worktree", "remove", "--force", &wt.info.path],
                )
                .await?;
                let _ = git.run("worktree prune", ["worktree", "prune"]).await;
            }
            _ => {
                let p = PathBuf::from(&wt.info.path);
                if p.starts_with(self.config.root.join("scratch")) {
                    std::fs::remove_dir_all(&p).map_err(|e| io("removing scratch dir", e))?;
                }
            }
        }
        self.db.mark_worktree_removed(session_id).await
    }
}

fn walk_dir(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            let r = rel.join(e.file_name());
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(r),
                Ok(_) => out.push(r.to_string_lossy().into_owned()),
                Err(_) => {}
            }
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}
