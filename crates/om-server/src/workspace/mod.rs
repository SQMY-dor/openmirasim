//! Workspaces and per-session Git worktrees (W1-2).
//!
//! Layout under the manager's root (normally `$OM_DATA_DIR`):
//!
//! ```text
//! worktrees/<uuid>/          one checkout per session, on branch `om/<uuid>`
//! gitdirs/<uuid>/            the session's own repository (its `.git`)
//! scratch/<uuid>/            empty working dir for sandboxed sessions without a workspace
//! homes/<space>/<member>/<uuid>/   per-session private HOME, isolated by space, member & session
//! homes/_anonymous/<uuid>/         per-session HOME when no member is known
//! ```
//!
//! Isolation (review P0-1): a session never sees the registered repository's `.git`.
//! Each checkout is a separate clone (`git clone --shared --separate-git-dir`) whose
//! objects borrow the repository's object store read-only, so sessions cannot touch
//! the repository's refs, index, config or each other. The session's repository is
//! writable except for the files git reads configuration or redirections from
//! (`config`, `hooks/`, `info/`, `commondir`, `objects/info/alternates`), which are
//! mounted read-only. Work comes back with [`WorkspaceManager::sync_branch`], a
//! hardened server-side `git fetch` of the session branch into the repository.
//!
//! Operations are serialized per repository git dir. Sessions keep their checkout after
//! closing so their changes can still be reviewed; [`WorkspaceManager::remove_worktree`]
//! copies the branch back and deletes the checkout.

mod git;
mod nofollow;
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
    /// Keyed by canonical git common dir, so every path to one repository shares a lock.
    repo_locks: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
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
        for sub in ["worktrees", "gitdirs", "scratch", "homes"] {
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

    fn lock_for(&self, git_common_dir: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = Path::new(git_common_dir)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(git_common_dir));
        self.repo_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default()
            .clone()
    }

    /// `true` when `p` is a direct child of `<root>/<sub>`.
    fn owned(&self, sub: &str, p: &Path) -> bool {
        p.parent() == Some(self.config.root.join(sub).as_path())
            && p.file_name()
                .is_some_and(|n| safe_component(&n.to_string_lossy()).is_ok())
    }

    /// Deletes a directory this manager created, refusing anything outside its layout.
    fn remove_owned(&self, sub: &str, p: &Path) -> CoreResult<()> {
        if !self.owned(sub, p) {
            return Err(CoreError::Workspace(format!(
                "refusing to delete {} outside {sub}/",
                p.display()
            )));
        }
        match std::fs::remove_dir_all(p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("removing directory", e)),
        }
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
        let common = path_str(
            &Path::new(&common)
                .canonicalize()
                .map_err(|e| io("resolving git dir", e))?,
        );
        if Path::new(&common).starts_with(&self.config.root)
            || self.config.root.starts_with(&common)
        {
            return Err(CoreError::InvalidInput(
                "repository git dir must not overlap the server's data directory".into(),
            ));
        }
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

    /// Each session gets its own private HOME, scoped under `(space, member, session_key)`
    /// (or `_anonymous/<session_key>`), so different spaces, members, and concurrent
    /// sessions/workspaces never share mutable dotfiles or agent state.
    async fn member_home(
        &self,
        space: Option<&SpaceId>,
        member: Option<&MemberId>,
        key: &str,
    ) -> CoreResult<PathBuf> {
        let home = match member {
            Some(m) => {
                let space = match space {
                    Some(s) => s.clone(),
                    None => self.db.get_member(m).await?.space_id,
                };
                self.config
                    .root
                    .join("homes")
                    .join(safe_component(space.as_str())?)
                    .join(safe_component(m.as_str())?)
                    .join(key)
            }
            None => self.config.root.join("homes").join("_anonymous").join(key),
        };
        std::fs::create_dir_all(&home).map_err(|e| io("creating session home", e))?;
        Ok(home)
    }

    fn remove_session_home(&self, home: Option<&str>) {
        let Some(h) = home else {
            return;
        };
        let path = Path::new(h);
        let homes_root = self.config.root.join("homes");
        let Ok(rel) = path.strip_prefix(&homes_root) else {
            return;
        };
        let comps: Vec<_> = rel.components().collect();
        let is_per_session = matches!(
            comps.as_slice(),
            [Component::Normal(first), Component::Normal(_)] if *first == "_anonymous"
        ) || matches!(
            comps.as_slice(),
            [
                Component::Normal(_),
                Component::Normal(_),
                Component::Normal(_)
            ]
        );
        if !is_per_session {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), error = %e, "failed to remove session home");
        }
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
        let key = uuid::Uuid::now_v7().simple().to_string();
        let Some(ws_id) = workspace_id else {
            let home = path_str(&self.member_home(None, member, &key).await?);
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
        let home = path_str(
            &self
                .member_home(Some(&ws.summary.space_id), member, &key)
                .await?,
        );
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
        let git_dir = self.config.root.join("gitdirs").join(&key);
        let branch = format!("om/{key}");

        let lock = self.lock_for(&ws.git_common_dir);
        let _guard = lock.lock().await;
        let made = self
            .clone_checkout(&repo, &git_dir, &path, &branch, &base_commit)
            .await;
        if let Err(e) = made {
            let _ = self.remove_owned("worktrees", &path);
            let _ = self.remove_owned("gitdirs", &git_dir);
            return Err(e);
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

    /// Clones `repo` into a session repository at `git_dir` with its checkout at `path`,
    /// on `branch` at `base_commit`, then pins the files git reads configuration from.
    async fn clone_checkout(
        &self,
        repo: &Path,
        git_dir: &Path,
        path: &Path,
        branch: &str,
        base_commit: &str,
    ) -> CoreResult<()> {
        Git::at(&self.config.git_bin, &self.config.root)
            .with_config("protocol.file.allow", "always")
            .run(
                "clone",
                [
                    "clone",
                    "--quiet",
                    "--shared",
                    "--no-checkout",
                    "--no-tags",
                    "--separate-git-dir",
                    &path_str(git_dir),
                    "--",
                    &path_str(repo),
                    &path_str(path),
                ],
            )
            .await?;
        // Pin the common dir to the repository itself (an agent-created `commondir`
        // would otherwise redirect every git call to a directory it controls) and make
        // sure each read-only overlay exists before the sandbox binds it.
        std::fs::write(git_dir.join("commondir"), b".\n")
            .map_err(|e| io("preparing git dir", e))?;
        for d in ["hooks", "info"] {
            std::fs::create_dir_all(git_dir.join(d)).map_err(|e| io("preparing git dir", e))?;
        }
        for f in ["objects/info/alternates", "info/exclude"] {
            let f = git_dir.join(f);
            if !f.exists() {
                std::fs::write(&f, b"").map_err(|e| io("preparing git dir", e))?;
            }
        }
        let session_git = Git::worktree(&self.config.git_bin, git_dir, path);
        // The clone's remote points at the host repository, which the sandbox cannot
        // reach anyway; drop it so nothing tries.
        let _ = session_git
            .run("remote remove", ["remote", "remove", "origin"])
            .await;
        session_git
            .run(
                "checkout",
                [
                    "checkout",
                    "--quiet",
                    "--no-track",
                    "-b",
                    branch,
                    base_commit,
                ],
            )
            .await?;
        Ok(())
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
        let _guard = match &prepared.repo {
            Some((_, common)) => Some(self.lock_for(&path_str(common)).lock_owned().await),
            None => None,
        };
        let sub = if prepared.repo.is_some() {
            "worktrees"
        } else {
            "scratch"
        };
        if let Err(e) = self.remove_owned(sub, &path) {
            tracing::warn!(error = %e, "failed to discard checkout");
        }
        if let Some(gd) = &prepared.stored.git_dir
            && let Err(e) = self.remove_owned("gitdirs", Path::new(gd))
        {
            tracing::warn!(error = %e, "failed to discard session git dir");
        }
        self.remove_session_home(prepared.stored.home_path.as_deref());
    }

    pub async fn worktree(&self, session_id: &SessionId) -> CoreResult<StoredWorktree> {
        self.db
            .get_worktree(session_id)
            .await?
            .filter(|w| w.info.removed_at.is_none())
            .ok_or_else(|| CoreError::NotFound(format!("worktree of session {session_id}")))
    }

    /// What the sandbox must expose for this checkout: the checkout and the session's own
    /// git dir read-write (so the agent can commit), the files git reads configuration or
    /// redirections from read-only, the repository's object store read-only (the clone
    /// borrows its objects), plus the member's private HOME. The registered repository's
    /// `.git` is never exposed.
    pub async fn sandbox_paths(&self, wt: &StoredWorktree) -> CoreResult<SandboxPaths> {
        let mut paths = SandboxPaths {
            writable: vec![PathBuf::from(&wt.info.path)],
            readonly: Vec::new(),
            home: wt.home_path.as_ref().map(PathBuf::from),
        };
        if let Some(git_dir) = &wt.git_dir {
            let gd = PathBuf::from(git_dir);
            if !self.owned("gitdirs", &gd) {
                return Err(CoreError::Workspace(
                    "this checkout predates per-session repositories and cannot be sandboxed \
                     safely; start a new session"
                        .into(),
                ));
            }
            // Bind both `<gd>` and `<gd>/objects` read-write as mountpoints so Linux
            // `vfs_rename` refuses (`EBUSY`) renaming `<gd>/objects` to displace the
            // read-only `<gd>/objects/info` overlay.
            nofollow::verify_no_symlinks_under(&gd, Path::new("objects"))?;
            paths.writable.push(gd.clone());
            paths.writable.push(gd.join("objects"));
            for rel in [
                "config",
                "hooks",
                "info",
                "commondir",
                "objects/info",
                "objects/info/alternates",
            ] {
                nofollow::verify_no_symlinks_under(&gd, Path::new(rel))?;
                paths.readonly.push(gd.join(rel));
            }
            if let Some(ws_id) = &wt.info.workspace_id {
                let ws = self.db.get_workspace(ws_id).await?;
                let objects = Path::new(&ws.git_common_dir).join("objects");
                if objects.is_dir() {
                    paths.readonly.push(objects);
                }
            }
        }
        Ok(paths)
    }

    /// Copies the session branch from the session's repository into the registered
    /// repository (`refs/heads/om/<uuid>`), so its commits can be reviewed and merged
    /// there. The session repository is treated as hostile: only that one ref is
    /// fetched, objects are checked, and nothing from it is executed.
    pub async fn sync_branch(&self, session_id: &SessionId) -> CoreResult<()> {
        let wt = self.worktree(session_id).await?;
        self.sync_stored(&wt).await
    }

    async fn sync_stored(&self, wt: &StoredWorktree) -> CoreResult<()> {
        let (Some(ws_id), Some(git_dir), Some(branch)) =
            (&wt.info.workspace_id, &wt.git_dir, &wt.info.branch)
        else {
            return Err(CoreError::Unsupported {
                harness: "workspace".into(),
                capability: "syncing a checkout without a repository",
            });
        };
        if !self.owned("gitdirs", Path::new(git_dir)) {
            // Legacy worktree: the branch already lives in the repository.
            return Ok(());
        }
        let ws = self.db.get_workspace(ws_id).await?;
        self.verify_git_alternates(Path::new(git_dir), Some(&ws.git_common_dir))?;
        let lock = self.lock_for(&ws.git_common_dir);
        let _guard = lock.lock().await;
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        Git::at(&self.config.git_bin, &ws.summary.repo_path)
            .with_config("protocol.file.allow", "always")
            .with_config("core.alternateRefsCommand", "")
            .with_config("transfer.fsckObjects", "true")
            .with_config("fetch.fsckObjects", "true")
            .run(
                "fetch",
                [
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--no-write-fetch-head",
                    "--",
                    git_dir,
                    &refspec,
                ],
            )
            .await?;
        Ok(())
    }

    fn verify_git_alternates(
        &self,
        git_dir: &Path,
        expected_common_dir: Option<&str>,
    ) -> CoreResult<()> {
        if !self.owned("gitdirs", git_dir) {
            return Ok(());
        }
        let bytes = nofollow::read(
            git_dir,
            Path::new("objects/info/alternates"),
            4096,
            "objects/info/alternates",
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| CoreError::Workspace("corrupt session git alternates file".into()))?;
        let expected_objects = expected_common_dir.map(|c| Path::new(c).join("objects"));
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            if let Some(ref exp) = expected_objects {
                let p = Path::new(line);
                let canon_line = p.canonicalize().ok();
                let canon_exp = exp.canonicalize().ok();
                if p != exp && (canon_line.is_none() || canon_line != canon_exp) {
                    return Err(CoreError::Workspace(
                        "session git alternates points outside its registered repository".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn checkout_git(&self, wt: &StoredWorktree) -> CoreResult<Git> {
        let git_dir = wt.git_dir.as_ref().ok_or_else(|| CoreError::Unsupported {
            harness: "workspace".into(),
            capability: "git operations on a scratch directory",
        })?;
        Ok(Git::worktree(&self.config.git_bin, git_dir, &wt.info.path)
            .with_config("core.alternateRefsCommand", ""))
    }

    /// Changes relative to the base commit: committed, staged, unstaged and untracked.
    pub async fn diff(&self, session_id: &SessionId) -> CoreResult<WorktreeDiff> {
        let wt = self.worktree(session_id).await?;
        if let Some(git_dir) = &wt.git_dir {
            let expected_common = match &wt.info.workspace_id {
                Some(ws_id) => self
                    .db
                    .get_workspace(ws_id)
                    .await
                    .ok()
                    .map(|w| w.git_common_dir),
                None => None,
            };
            self.verify_git_alternates(Path::new(git_dir), expected_common.as_deref())?;
        }
        let git = self.checkout_git(&wt)?;
        let base = wt.info.base_commit.clone().unwrap_or_default();
        let diff_opts = ["--no-ext-diff", "--no-textconv", "--no-color", "-M"];

        let mut args = vec!["diff", "--name-status", "-z"];
        args.extend(diff_opts);
        args.extend([base.as_str(), "--"]);
        // Cap name list memory strictly (16 MiB ceiling).
        let name_cap = self
            .config
            .max_entries
            .saturating_mul(1024)
            .clamp(1 << 20, 16 << 20);
        let (names, _) = git
            .run_capped("diff --name-status", &args, name_cap, &[])
            .await?;
        let names = split_z(&names);
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

        let max = self.config.max_patch_bytes;
        let mut args = vec!["diff"];
        args.extend(diff_opts);
        args.extend([base.as_str(), "--"]);
        let (mut patch, mut truncated) = git.run_capped("diff", &args, max, &[]).await?;

        let (untracked, _) = git
            .run_capped(
                "ls-files",
                ["ls-files", "-z", "--others", "--exclude-standard"],
                name_cap,
                &[],
            )
            .await?;
        let untracked: Vec<String> = split_z(&untracked)
            .into_iter()
            .take(self.config.max_entries)
            .collect();
        let root = PathBuf::from(&wt.info.path);
        // Never invoke unsandboxed `git diff --no-index` on untracked paths (which follows
        // intermediate directory symlinks and can block on FIFOs). Read each file strictly
        // via component-wise `O_NOFOLLOW` + `O_NONBLOCK` and render the unified diff in-process.
        let initial_len = patch.len();
        let (extra_patch, extra_truncated, extra_files) = tokio::task::spawn_blocking(move || {
            let mut out_patch = Vec::new();
            let mut out_truncated = truncated;
            let mut out_files = Vec::with_capacity(untracked.len());
            let mut used = initial_len;
            for path in untracked {
                if out_truncated || used >= max {
                    if used >= max {
                        out_truncated = true;
                    }
                    out_files.push(FileChange {
                        path,
                        kind: FileChangeKind::Untracked,
                        old_path: None,
                    });
                    continue;
                }
                let room = max - used;
                if let Some((bytes, file_trunc)) =
                    nofollow::read_truncated(&root, Path::new(&path), room as u64)
                {
                    let formatted = format_untracked_patch(&path, &bytes, file_trunc);
                    if formatted.len() > room {
                        out_patch.extend_from_slice(&formatted[..room]);
                        used = max;
                        out_truncated = true;
                    } else {
                        out_patch.extend_from_slice(&formatted);
                        used += formatted.len();
                        out_truncated |= file_trunc;
                    }
                }
                out_files.push(FileChange {
                    path,
                    kind: FileChangeKind::Untracked,
                    old_path: None,
                });
            }
            (out_patch, out_truncated, out_files)
        })
        .await
        .map_err(|e| CoreError::Workspace(format!("diff untracked task failed: {e}")))?;

        patch.extend_from_slice(&extra_patch);
        truncated = extra_truncated;
        files.extend(extra_files);

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
        let name_cap = self
            .config
            .max_entries
            .saturating_mul(1024)
            .clamp(1 << 20, 16 << 20);
        let max_entries = self.config.max_entries;
        let paths = match self.checkout_git(&wt) {
            Ok(git) => {
                if let Some(git_dir) = &wt.git_dir {
                    let expected_common = match &wt.info.workspace_id {
                        Some(ws_id) => self
                            .db
                            .get_workspace(ws_id)
                            .await
                            .ok()
                            .map(|w| w.git_common_dir),
                        None => None,
                    };
                    self.verify_git_alternates(Path::new(git_dir), expected_common.as_deref())?;
                }
                let (raw, _) = git
                    .run_capped(
                        "ls-files",
                        [
                            "ls-files",
                            "-z",
                            "--cached",
                            "--others",
                            "--exclude-standard",
                            "--deduplicate",
                        ],
                        name_cap,
                        &[],
                    )
                    .await?;
                split_z(&raw)
            }
            Err(_) => {
                let walk_root = root.clone();
                tokio::task::spawn_blocking(move || walk_dir(&walk_root, max_entries))
                    .await
                    .map_err(|e| CoreError::Workspace(format!("walk task failed: {e}")))?
            }
        };
        let entries = tokio::task::spawn_blocking(move || {
            let mut entries: Vec<WorktreeEntry> = paths
                .into_iter()
                .filter_map(|p| {
                    let size = nofollow::stat_non_dir(&root, Path::new(&p))?;
                    Some(WorktreeEntry { path: p, size })
                })
                .take(max_entries)
                .collect();
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            entries
        })
        .await
        .map_err(|e| CoreError::Workspace(format!("stat task failed: {e}")))?;
        Ok(entries)
    }

    /// Reads a file inside the checkout. Rejects `..`, absolute paths, `.git`, symlinks
    /// anywhere on the path and anything but a regular file, opening each component
    /// without following links so a concurrent swap cannot escape the checkout.
    pub async fn read_file(&self, session_id: &SessionId, rel: &str) -> CoreResult<Vec<u8>> {
        let wt = self.worktree(session_id).await?;
        let rel_path = Path::new(rel);
        if rel.is_empty()
            || rel_path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || rel_path.components().any(|c| c.as_os_str() == ".git")
        {
            return Err(CoreError::InvalidInput(format!(
                "invalid relative path {rel:?}"
            )));
        }
        let root = PathBuf::from(&wt.info.path);
        let rel_path = rel_path.to_path_buf();
        let max = self.config.max_file_bytes;
        let rel = rel.to_owned();
        tokio::task::spawn_blocking(move || nofollow::read(&root, &rel_path, max, &rel))
            .await
            .map_err(|e| CoreError::Workspace(format!("read task failed: {e}")))?
    }

    /// Copies the session branch back into the repository (see
    /// [`WorkspaceManager::sync_branch`]), then deletes the checkout and the session's
    /// repository. Fails, deleting nothing, when the branch cannot be copied; use
    /// [`WorkspaceManager::remove_worktree_discarding`] to delete anyway.
    pub async fn remove_worktree(&self, session_id: &SessionId) -> CoreResult<()> {
        let wt = self.worktree(session_id).await?;
        if wt.info.workspace_id.is_some() && wt.git_dir.is_some() {
            self.sync_stored(&wt).await?;
        }
        self.delete_checkout(&wt).await?;
        self.db.mark_worktree_removed(session_id).await
    }

    /// Deletes the checkout and the session's repository without copying the branch
    /// back (e.g. an agent broke its repository). Unsynced commits are lost.
    pub async fn remove_worktree_discarding(&self, session_id: &SessionId) -> CoreResult<()> {
        let wt = self.worktree(session_id).await?;
        self.delete_checkout(&wt).await?;
        self.db.mark_worktree_removed(session_id).await
    }

    async fn delete_checkout(&self, wt: &StoredWorktree) -> CoreResult<()> {
        let path = PathBuf::from(&wt.info.path);
        let res = match &wt.git_dir {
            None => {
                if path.starts_with(self.config.root.join("scratch")) {
                    self.remove_owned("scratch", &path)?;
                }
                Ok(())
            }
            Some(git_dir) => {
                let gd = PathBuf::from(git_dir);
                if self.owned("gitdirs", &gd) {
                    // Our own directories: deleting them needs no git and works even when the
                    // workspace row is gone or the agent wrecked its repository.
                    self.remove_owned("worktrees", &path)?;
                    self.remove_owned("gitdirs", &gd)
                } else {
                    // Legacy linked worktree inside the repository's git dir.
                    if let Some(ws_id) = &wt.info.workspace_id
                        && let Ok(ws) = self.db.get_workspace(ws_id).await
                    {
                        let lock = self.lock_for(&ws.git_common_dir);
                        let _guard = lock.lock().await;
                        let git = Git::at(&self.config.git_bin, &ws.summary.repo_path);
                        let _ = git
                            .run(
                                "worktree remove",
                                ["worktree", "remove", "--force", &wt.info.path],
                            )
                            .await;
                        let _ = git.run("worktree prune", ["worktree", "prune"]).await;
                    }
                    self.remove_owned("worktrees", &path)
                }
            }
        };
        self.remove_session_home(wt.home_path.as_deref());
        res
    }
}

fn format_untracked_patch(path: &str, bytes: &[u8], file_truncated: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        format!("diff --git a/{path} b/{path}\nnew file mode 100644\n").as_bytes(),
    );
    if bytes.is_empty() {
        return out;
    }
    if bytes.contains(&0) {
        out.extend_from_slice(format!("Binary files /dev/null and b/{path} differ\n").as_bytes());
        return out;
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let count = lines.len();
    let range = if count == 1 {
        "1".to_string()
    } else {
        format!("1,{count}")
    };
    out.extend_from_slice(format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +{range} @@\n").as_bytes());
    for line in &lines {
        out.push(b'+');
        out.extend_from_slice(line.as_bytes());
        if !line.ends_with('\n') {
            out.push(b'\n');
            if !file_truncated {
                out.extend_from_slice(b"\\ No newline at end of file\n");
            }
        }
    }
    out
}

fn walk_dir(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    let max_visits = limit.saturating_mul(4).max(4096);
    let mut visits = 0usize;
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            visits += 1;
            if visits > max_visits {
                return out;
            }
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
