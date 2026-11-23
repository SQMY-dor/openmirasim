use super::*;
use std::process::Command;

fn sh_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fx {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    mgr: WorkspaceManager,
    db: Db,
    space: SpaceId,
}

async fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    sh_git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("old.txt"), "rename me\n").unwrap();
    std::fs::write(repo.join("gone.txt"), "delete me\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = repo.canonicalize().unwrap();
    let db = Db::open(tmp.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let mgr =
        WorkspaceManager::new(db.clone(), WorkspaceConfig::new(tmp.path().join("data"))).unwrap();
    Fx {
        _tmp: tmp,
        repo,
        mgr,
        db,
        space,
    }
}

/// Creates a session row so the worktree FK holds.
async fn session(f: &Fx) -> SessionId {
    let cfg = om_core::session::SessionConfig {
        harness_id: "x".into(),
        cwd: "/".into(),
        workspace_id: None,
        model: None,
        sandbox: om_core::sandbox::SandboxMode::Direct,
        title: None,
    };
    f.db.create_session(f.space.clone(), cfg, None)
        .await
        .unwrap()
        .0
        .id
}

async fn checkout(f: &Fx, ws: &WorkspaceSummary) -> (SessionId, WorktreeInfo) {
    let sid = session(f).await;
    let p = f
        .mgr
        .prepare(Some(&ws.id), None, Some(&MemberId::from("alice")))
        .await
        .unwrap();
    let info = f.mgr.attach(&sid, p).await.unwrap();
    (sid, info)
}

#[tokio::test]
async fn register_validates_repositories() {
    let f = fx().await;
    let repo = f.repo.to_string_lossy().into_owned();
    let ws = f.mgr.register(f.space.clone(), "app", &repo).await.unwrap();
    assert_eq!(ws.default_branch, "main");
    assert_eq!(ws.repo_path, repo);
    assert_eq!(f.mgr.list(&f.space).await.unwrap(), vec![ws.clone()]);

    let dup = f.mgr.register(f.space.clone(), "app", &repo).await;
    assert!(matches!(dup, Err(CoreError::InvalidInput(m)) if m.contains("already exists")));
    let sub = f.repo.join("sub");
    std::fs::create_dir(&sub).unwrap();
    assert!(
        f.mgr
            .register(f.space.clone(), "sub", &sub.to_string_lossy())
            .await
            .is_err()
    );
    assert!(
        f.mgr
            .register(f.space.clone(), "rel", "relative/path")
            .await
            .is_err()
    );
    let plain = tempfile::tempdir().unwrap();
    assert!(matches!(
        f.mgr.register(f.space.clone(), "plain", &plain.path().to_string_lossy()).await,
        Err(CoreError::InvalidInput(m)) if m.contains("not a git repository")
    ));
    assert!(
        f.mgr
            .register(f.space.clone(), "root", &f.mgr.root().to_string_lossy())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn worktree_isolation_and_diff() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let (_b, wb) = checkout(&f, &ws).await;
    assert_ne!(wa.path, wb.path);
    assert_ne!(wa.branch, wb.branch);
    assert!(wa.branch.as_deref().unwrap().starts_with("om/"));
    assert!(Path::new(&wa.path).starts_with(f.mgr.root().join("worktrees")));

    let pa = PathBuf::from(&wa.path);
    std::fs::write(pa.join("README.md"), "hello\nworld\n").unwrap();
    std::fs::write(pa.join("new.txt"), "brand new\n").unwrap();
    std::fs::write(pa.join("debug.log"), "ignored\n").unwrap();
    std::fs::remove_file(pa.join("gone.txt")).unwrap();
    std::fs::rename(pa.join("old.txt"), pa.join("renamed.txt")).unwrap();
    // A commit inside the session still shows in the diff against the base.
    sh_git(&pa, &["add", "renamed.txt", "old.txt"]);
    sh_git(&pa, &["commit", "-q", "-m", "rename"]);

    // The other checkout and the main repo are untouched.
    assert_eq!(
        std::fs::read_to_string(Path::new(&wb.path).join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("README.md")).unwrap(),
        "hello\n"
    );

    let d = f.mgr.diff(&a).await.unwrap();
    assert_eq!(Some(&d.base_commit), wa.base_commit.as_ref());
    let kinds: Vec<(String, FileChangeKind)> =
        d.files.iter().map(|c| (c.path.clone(), c.kind)).collect();
    assert!(kinds.contains(&("README.md".into(), FileChangeKind::Modified)));
    assert!(kinds.contains(&("gone.txt".into(), FileChangeKind::Deleted)));
    assert!(kinds.contains(&("renamed.txt".into(), FileChangeKind::Renamed)));
    assert!(kinds.contains(&("new.txt".into(), FileChangeKind::Untracked)));
    assert!(!kinds.iter().any(|(p, _)| p == "debug.log"));
    assert!(d.patch.contains("+world"));
    assert!(d.patch.contains("+brand new"));
    assert!(!d.truncated);

    let files: Vec<String> = f
        .mgr
        .list_files(&a)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(
        files,
        vec![".gitignore", "README.md", "new.txt", "renamed.txt"]
    );
}

#[tokio::test]
async fn read_file_rejects_escapes() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let pa = PathBuf::from(&wa.path);
    assert_eq!(f.mgr.read_file(&a, "README.md").await.unwrap(), b"hello\n");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "s").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), pa.join("link")).unwrap();
    for bad in [
        "../repo/README.md",
        "/etc/passwd",
        "link",
        "",
        "a/../README.md",
        ".git",
    ] {
        assert!(
            f.mgr.read_file(&a, bad).await.is_err(),
            "{bad} must be rejected"
        );
    }
    assert!(matches!(
        f.mgr.read_file(&a, "missing").await,
        Err(CoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn tampered_dot_git_file_does_not_redirect_server_git() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let pa = PathBuf::from(&wa.path);
    // The agent builds a hostile git dir with a clean filter that would run on the host.
    let evil = pa.join("evil");
    let marker = f.mgr.root().join("PWNED");
    std::fs::create_dir(&evil).unwrap();
    sh_git(&evil, &["init", "-q"]);
    sh_git(
        &evil,
        &[
            "config",
            "filter.x.clean",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(pa.join(".gitattributes"), "* filter=x\n").unwrap();
    std::fs::write(
        pa.join(".git"),
        format!("gitdir: {}\n", evil.join(".git").display()),
    )
    .unwrap();
    std::fs::write(pa.join("README.md"), "changed\n").unwrap();

    let d = f.mgr.diff(&a).await.unwrap();
    assert!(d.files.iter().any(|c| c.path == "README.md"));
    f.mgr.list_files(&a).await.unwrap();
    assert!(
        !marker.exists(),
        "server-side git executed an agent-controlled filter"
    );
}

#[tokio::test]
async fn sandbox_paths_overlay_git_config_read_only() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let wt = f.mgr.worktree(&a).await.unwrap();
    let p = f.mgr.sandbox_paths(&wt).await.unwrap();
    let common = f.repo.join(".git");
    let gd = PathBuf::from(wt.git_dir.clone().unwrap());
    assert!(gd.starts_with(f.mgr.root().join("gitdirs")));
    assert_eq!(p.writable, vec![PathBuf::from(&wa.path), gd.clone()]);
    assert!(
        !p.writable.iter().any(|w| w.starts_with(&common)),
        "the repository's .git is never writable"
    );
    for must in [
        "config",
        "hooks",
        "info",
        "commondir",
        "objects/info/alternates",
    ] {
        assert!(
            p.readonly.contains(&gd.join(must)),
            "missing ro overlay {must}"
        );
    }
    assert!(p.readonly.contains(&common.join("objects")));
    assert!(
        !p.readonly
            .iter()
            .any(|r| r.starts_with(&common) && *r != common.join("objects")),
        "nothing else of the repository's .git is exposed: {:?}",
        p.readonly
    );
    let home = p.home.unwrap();
    assert!(home.ends_with(format!("homes/{}/alice", f.space)));
    let _ = a;
}

#[tokio::test]
async fn concurrent_checkouts_and_removal() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let mut sids = Vec::new();
    for _ in 0..8 {
        sids.push(session(&f).await);
    }
    let tasks: Vec<_> = sids
        .iter()
        .map(|sid| {
            let mgr = f.mgr.clone();
            let ws_id = ws.id.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                let p = mgr.prepare(Some(&ws_id), None, None).await?;
                mgr.attach(&sid, p).await
            })
        })
        .collect();
    let mut paths = std::collections::HashSet::new();
    for t in tasks {
        paths.insert(t.await.unwrap().unwrap().path);
    }
    assert_eq!(paths.len(), 8);
    assert_eq!(f.db.list_live_worktrees(&ws.id).await.unwrap().len(), 8);

    let first = &sids[0];
    let wt = f.mgr.worktree(first).await.unwrap();
    f.mgr.remove_worktree(first).await.unwrap();
    assert!(!Path::new(&wt.info.path).exists());
    assert!(matches!(
        f.mgr.worktree(first).await,
        Err(CoreError::NotFound(_))
    ));
    assert_eq!(f.db.list_live_worktrees(&ws.id).await.unwrap().len(), 7);
    // The branch survives so the work can still be merged.
    sh_git(
        &f.repo,
        &["rev-parse", "--verify", wt.info.branch.as_deref().unwrap()],
    );
}

#[tokio::test]
async fn scratch_dirs_and_bad_inputs() {
    let f = fx().await;
    let sid = session(&f).await;
    let p = f.mgr.prepare(None, None, None).await.unwrap();
    let info = f.mgr.attach(&sid, p).await.unwrap();
    assert!(Path::new(&info.path).starts_with(f.mgr.root().join("scratch")));
    assert!(info.branch.is_none());
    std::fs::write(Path::new(&info.path).join("a.txt"), "x").unwrap();
    assert_eq!(f.mgr.list_files(&sid).await.unwrap()[0].path, "a.txt");
    assert!(matches!(
        f.mgr.diff(&sid).await,
        Err(CoreError::Unsupported { .. })
    ));
    f.mgr.remove_worktree(&sid).await.unwrap();
    assert!(!Path::new(&info.path).exists());

    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    assert!(
        f.mgr
            .prepare(Some(&ws.id), Some("--upload-pack=x"), None)
            .await
            .is_err()
    );
    assert!(
        f.mgr
            .prepare(Some(&ws.id), Some("no-such-branch"), None)
            .await
            .is_err()
    );
    assert!(
        f.mgr
            .prepare(Some(&ws.id), None, Some(&MemberId::from("../root")))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn session_commits_stay_out_of_the_repository_until_synced() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let pa = PathBuf::from(&wa.path);
    let branch = wa.branch.clone().unwrap();
    std::fs::write(pa.join("feature.txt"), "x\n").unwrap();
    sh_git(&pa, &["add", "feature.txt"]);
    sh_git(&pa, &["commit", "-q", "-m", "feature"]);
    // The session's commit and branch are not in the repository yet.
    let host_has = |r: &str| {
        Command::new("git")
            .arg("-C")
            .arg(&f.repo)
            .args(["rev-parse", "--verify", "--quiet", r])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(!host_has(&branch));
    // Tampering with the session's HEAD or refs does not touch the repository.
    sh_git(&pa, &["update-ref", "refs/heads/main", "HEAD"]);
    let host_main = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&f.repo)
            .args(["rev-parse", "main"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(host_main.trim(), wa.base_commit.as_deref().unwrap());

    f.mgr.sync_branch(&a).await.unwrap();
    assert!(host_has(&branch));
    assert!(host_has(&format!("{branch}:feature.txt")));
}

#[tokio::test]
async fn a_repository_registers_once_across_spaces() {
    let f = fx().await;
    f.mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let other = f.db.ensure_space("other").await.unwrap();
    assert!(matches!(
        f.mgr
            .register(other, "app", &f.repo.to_string_lossy())
            .await,
        Err(CoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn removal_works_after_the_agent_wrecks_its_repository() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let wt = f.mgr.worktree(&a).await.unwrap();
    let gd = PathBuf::from(wt.git_dir.unwrap());
    std::fs::remove_dir_all(gd.join("refs")).unwrap();
    std::fs::remove_file(PathBuf::from(&wa.path).join(".git")).unwrap();
    // Copying the branch back fails, so the safe removal refuses...
    assert!(f.mgr.remove_worktree(&a).await.is_err());
    assert!(Path::new(&wa.path).exists());
    // ...and the discarding removal still cleans up.
    f.mgr.remove_worktree_discarding(&a).await.unwrap();
    assert!(!Path::new(&wa.path).exists());
    assert!(!gd.exists());
}

#[tokio::test]
async fn reads_and_diffs_never_follow_links_or_block_on_fifos() {
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let pa = PathBuf::from(&wa.path);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "s").unwrap();
    std::os::unix::fs::symlink(outside.path(), pa.join("dirlink")).unwrap();
    assert!(f.mgr.read_file(&a, "dirlink/secret").await.is_err());
    let fifo = pa.join("pipe");
    let ok = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.mgr.read_file(&a, "pipe"),
    )
    .await
    .expect("reading a FIFO must not block");
    assert!(matches!(r, Err(CoreError::InvalidInput(_))));
    let d = tokio::time::timeout(std::time::Duration::from_secs(20), f.mgr.diff(&a))
        .await
        .expect("diff must not block on a FIFO")
        .unwrap();
    assert!(!d.patch.contains("pipe"));
}

#[tokio::test]
async fn diff_truncation_is_reported_at_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let f = fx().await;
    let mut cfg = WorkspaceConfig::new(tmp.path().join("data2"));
    cfg.max_patch_bytes = 200;
    let mgr = WorkspaceManager::new(f.db.clone(), cfg).unwrap();
    let ws = mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let sid = session(&f).await;
    let p = mgr.prepare(Some(&ws.id), None, None).await.unwrap();
    let info = mgr.attach(&sid, p).await.unwrap();
    std::fs::write(Path::new(&info.path).join("README.md"), "x\n".repeat(500)).unwrap();
    let d = mgr.diff(&sid).await.unwrap();
    assert!(d.truncated);
    assert!(d.patch.len() <= 200);
}

#[tokio::test]
async fn sandboxed_agent_cannot_reach_the_repository_git_dir() {
    let Ok(sb) = crate::sandbox::BwrapSandbox::default().probe().await else {
        if std::env::var_os("OM_REQUIRE_BWRAP").is_some() {
            panic!("bwrap required");
        }
        return;
    };
    let f = fx().await;
    let ws = f
        .mgr
        .register(f.space.clone(), "app", &f.repo.to_string_lossy())
        .await
        .unwrap();
    let (a, wa) = checkout(&f, &ws).await;
    let wt = f.mgr.worktree(&a).await.unwrap();
    let paths = f.mgr.sandbox_paths(&wt).await.unwrap();
    let gd = wt.git_dir.clone().unwrap();
    let common = f.repo.join(".git");
    let script = format!(
        "cd {wt}; \
         git -c user.name=a -c user.email=a@b commit -q --allow-empty -m agent && echo COMMIT-OK; \
         echo x > {gd}/commondir 2>/dev/null && echo WROTE-COMMONDIR; \
         echo x >> {gd}/config 2>/dev/null && echo WROTE-CONFIG; \
         touch {gd}/hooks/pre-commit 2>/dev/null && echo WROTE-HOOK; \
         echo x > {gd}/objects/info/alternates 2>/dev/null && echo WROTE-ALTERNATES; \
         ls {common}/HEAD >/dev/null 2>&1 && echo SEES-REPO-HEAD; \
         touch {common}/objects/x 2>/dev/null && echo WROTE-REPO-OBJECTS; true",
        wt = wa.path,
        common = common.display(),
    );
    let spec = om_core::sandbox::SandboxSpec {
        writable_paths: paths.writable.clone(),
        readonly_paths: paths.readonly.clone(),
        home: paths.home.clone(),
        network: false,
        ..Default::default()
    };
    let cmd = om_core::sandbox::Sandbox::wrap(
        &sb,
        &spec,
        om_core::sandbox::CommandSpec::new("/bin/sh")
            .arg("-c")
            .arg(script),
    )
    .unwrap();
    let out = tokio::process::Command::new(&cmd.program)
        .args(&cmd.args)
        .env_clear()
        .envs(&cmd.env)
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("COMMIT-OK"),
        "agent can commit: {stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for bad in [
        "WROTE-COMMONDIR",
        "WROTE-CONFIG",
        "WROTE-HOOK",
        "WROTE-ALTERNATES",
        "SEES-REPO-HEAD",
        "WROTE-REPO-OBJECTS",
    ] {
        assert!(!stdout.contains(bad), "{bad}: {stdout}");
    }
}
