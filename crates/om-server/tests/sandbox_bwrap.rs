//! Escape / isolation tests for `BwrapSandbox` against the real `bwrap`.
//!
//! Skipped (with a message) when bubblewrap is unusable on the host, unless
//! `OM_REQUIRE_BWRAP=1` is set (CI sets it so a broken sandbox fails loudly).

use om_core::content::{ContentBlock, Prompt};
use om_core::event::EventPayload;
use om_core::sandbox::{CommandSpec, Sandbox, SandboxMode, SandboxSpec};
use om_core::session::{SessionConfig, SessionStatus};
use om_server::acp::AcpHarness;
use om_server::db::Db;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig};
use om_server::sandbox::{BwrapSandbox, ScopedSandbox};
use om_server::workspace::{WorkspaceConfig, WorkspaceManager};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

async fn sandbox() -> Option<BwrapSandbox> {
    match BwrapSandbox::default().probe().await {
        Ok(sb) => Some(sb),
        Err(e) if std::env::var_os("OM_REQUIRE_BWRAP").is_some() => {
            panic!("OM_REQUIRE_BWRAP is set but bwrap is unusable: {e}")
        }
        Err(e) => {
            eprintln!("skipping bwrap test: {e}");
            None
        }
    }
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn run(sb: &dyn Sandbox, spec: &SandboxSpec, script: &str, cwd: Option<&Path>) -> Out {
    let mut cmd = CommandSpec::new("sh").arg("-c").arg(script);
    cmd.cwd = cwd.map(Path::to_path_buf);
    let w = sb.wrap(spec, cmd).expect("wrap");
    let mut c = tokio::process::Command::new(&w.program);
    c.args(&w.args).env_clear().envs(&w.env).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(20), c.output())
        .await
        .expect("sandboxed command timed out")
        .expect("spawn");
    Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn worktree_spec(dir: &Path) -> SandboxSpec {
    SandboxSpec {
        writable_paths: vec![dir.to_path_buf()],
        ..SandboxSpec::default()
    }
}

#[tokio::test]
async fn worktree_is_writable_and_host_files_are_invisible() {
    let Some(sb) = sandbox().await else { return };
    let worktree = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let secret = elsewhere.path().join("secret.txt");
    std::fs::write(&secret, "top secret").unwrap();

    let script = format!(
        "echo hi > out.txt && cat out.txt; \
         if cat '{s}' 2>/dev/null; then echo LEAK-READ; fi; \
         if echo pwn > '{e}/pwned' 2>/dev/null; then echo LEAK-WRITE; fi; \
         if echo pwn > /usr/pwned 2>/dev/null; then echo LEAK-USR; fi; \
         for p in /etc/shadow /root /home /var /srv /mnt; do [ -e \"$p\" ] && echo VISIBLE:$p; done; true",
        s = secret.display(),
        e = elsewhere.path().display(),
    );
    let out = run(
        &sb,
        &worktree_spec(worktree.path()),
        &script,
        Some(worktree.path()),
    )
    .await;
    assert!(out.ok, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.lines().next(), Some("hi"));
    assert!(!out.stdout.contains("LEAK"), "{}", out.stdout);
    assert!(!out.stdout.contains("VISIBLE"), "{}", out.stdout);
    assert_eq!(
        std::fs::read_to_string(worktree.path().join("out.txt")).unwrap(),
        "hi\n"
    );
    assert!(!elsewhere.path().join("pwned").exists());
}

#[tokio::test]
async fn server_secrets_do_not_leak_through_env() {
    let Some(sb) = sandbox().await else { return };
    // SAFETY: test-only variable, not read concurrently by other code.
    unsafe { std::env::set_var("OM_TEST_LEAKY_API_KEY", "sk-should-not-leak") };
    let dir = tempfile::tempdir().unwrap();
    let out = run(&sb, &worktree_spec(dir.path()), "env", None).await;
    assert!(out.ok, "{}", out.stderr);
    assert!(!out.stdout.contains("sk-should-not-leak"));
    assert!(out.stdout.contains("HOME=/tmp/home"));
}

#[tokio::test]
async fn namespaces_are_private() {
    let Some(sb) = sandbox().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let script = "hostname 2>/dev/null || cat /proc/sys/kernel/hostname; \
                  ls /proc | grep -c '^[0-9]'; \
                  grep -c ':' /proc/net/dev; \
                  if unshare -U true 2>/dev/null; then echo NESTED-USERNS; fi; true";
    let out = run(&sb, &worktree_spec(dir.path()), script, None).await;
    assert!(out.ok, "{}", out.stderr);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines[0], "om-sandbox");
    let pids: u32 = lines[1].trim().parse().unwrap();
    assert!(
        pids <= 5,
        "pid namespace should hide host processes, saw {pids}"
    );
    assert_eq!(
        lines[2].trim(),
        "1",
        "only loopback without network: {}",
        out.stdout
    );
    assert!(!out.stdout.contains("NESTED-USERNS"));
}

#[tokio::test]
async fn network_flag_shares_host_network() {
    let Some(sb) = sandbox().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let host_ifaces = std::fs::read_to_string("/proc/net/dev")
        .unwrap()
        .lines()
        .filter(|l| l.contains(':'))
        .count();
    let spec = SandboxSpec {
        network: true,
        ..worktree_spec(dir.path())
    };
    let out = run(&sb, &spec, "grep -c ':' /proc/net/dev", None).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(out.stdout.trim().parse::<usize>().unwrap(), host_ifaces);
}

#[tokio::test]
async fn agent_runs_end_to_end_inside_bwrap() {
    let Some(sb) = sandbox().await else { return };
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let db = Db::open(data.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(AcpHarness::new(
        "fake",
        CommandSpec::new(env!("CARGO_BIN_EXE_om-fake-acp-agent")),
    )));
    let sb: Arc<dyn Sandbox> = Arc::new(sb);
    let mgr =
        WorkspaceManager::new(db.clone(), WorkspaceConfig::new(data.path().join("ws"))).unwrap();
    let scratch_root = mgr.root().join("scratch");
    let orch = Orchestrator::new(db, registry, OrchestratorConfig::default())
        .with_bwrap_sandbox(sb)
        .with_workspaces(mgr);

    let config = SessionConfig {
        harness_id: "fake".into(),
        cwd: worktree.path().to_string_lossy().into_owned(),
        workspace_id: None,
        model: None,
        sandbox: SandboxMode::Bwrap,
        title: None,
    };
    let s = orch.create_session(space, config, None).await.unwrap();
    // A sandboxed session never runs in the caller-chosen directory.
    assert!(
        Path::new(&s.config.cwd).starts_with(&scratch_root),
        "{}",
        s.config.cwd
    );
    let mut sub = orch.subscribe(&s.id, 0).await.unwrap();
    let mut text = String::new();
    let mut prompted = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), sub.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();
        match ev.payload {
            EventPayload::StatusChanged {
                to: SessionStatus::Idle,
                ..
            } if !prompted => {
                prompted = true;
                orch.submit_prompt(&s.id, None, Prompt::text("sandboxed"))
                    .await
                    .unwrap();
            }
            EventPayload::StatusChanged {
                to: SessionStatus::Idle,
                ..
            } => break,
            EventPayload::StatusChanged {
                to: SessionStatus::Failed,
                ..
            } => {
                panic!("sandboxed agent failed")
            }
            EventPayload::AgentMessageChunk {
                content: ContentBlock::Text { text: t },
                ..
            } => text.push_str(&t),
            _ => {}
        }
    }
    assert_eq!(text, "echo: sandboxed");
    orch.close_session(&s.id).await.unwrap();
}

fn host_git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn agent_commits_in_worktree_but_cannot_touch_git_config_or_repo() {
    let Some(sb) = sandbox().await else { return };
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    host_git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "main checkout\n").unwrap();
    host_git(&repo, &["add", "-A"]);
    host_git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = repo.canonicalize().unwrap();

    let db = Db::open(tmp.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let mgr =
        WorkspaceManager::new(db.clone(), WorkspaceConfig::new(tmp.path().join("data"))).unwrap();
    let ws = mgr
        .register(space.clone(), "app", &repo.to_string_lossy())
        .await
        .unwrap();
    let cfg = SessionConfig {
        harness_id: "x".into(),
        cwd: "/".into(),
        workspace_id: Some(ws.id.clone()),
        model: None,
        sandbox: SandboxMode::Bwrap,
        title: None,
    };
    let sid = db.create_session(space, cfg, None).await.unwrap().0.id;
    let member = om_core::ids::MemberId::from("alice");
    let p = mgr
        .prepare(Some(&ws.id), None, Some(&member))
        .await
        .unwrap();
    let info = mgr.attach(&sid, p).await.unwrap();
    let wt = mgr.worktree(&sid).await.unwrap();
    let paths = mgr.sandbox_paths(&wt).await.unwrap();
    let home = paths.home.clone().unwrap();
    let scoped = ScopedSandbox::new(Arc::new(sb), paths.writable, paths.readonly, paths.home);

    let common = repo.join(".git");
    let admin = wt.git_dir.clone().unwrap();
    let script = format!(
        "set -u; echo \"HOME=$HOME\"; \
         echo hi > note.txt && git add note.txt && git -c user.name=a -c user.email=a@a commit -qm agent && echo COMMITTED; \
         git config --global user.name agent && echo GLOBAL-OK; \
         for f in '{c}/config' '{c}/hooks/post-checkout' '{c}/info/attributes' '{a}/commondir' '{a}/gitdir' '{a}/config.worktree'; do \
           if echo pwn >> \"$f\" 2>/dev/null; then echo LEAK-WRITE:$f; fi; done; \
         if cat '{r}/README.md' 2>/dev/null; then echo LEAK-REPO; fi; \
         true",
        c = common.display(),
        a = admin,
        r = repo.display(),
    );
    let out = run(
        &scoped,
        &SandboxSpec::default(),
        &script,
        Some(Path::new(&info.path)),
    )
    .await;
    assert!(out.ok, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(&format!("HOME={}", home.display())),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("COMMITTED"),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(out.stdout.contains("GLOBAL-OK"), "{}", out.stdout);
    assert!(!out.stdout.contains("LEAK"), "{}", out.stdout);

    // The commit landed on the session branch in the real repository.
    let branch = info.branch.unwrap();
    assert_eq!(
        host_git(&repo, &["log", "-1", "--format=%s", &branch]),
        "agent"
    );
    assert!(home.join(".gitconfig").exists());
    assert!(!common.join("hooks/post-checkout").exists());
    let d = mgr.diff(&sid).await.unwrap();
    assert!(d.files.iter().any(|f| f.path == "note.txt"));
}

/// W1-3: inside bwrap, a read of a worktree path is auto-allowed by policy while a
/// command still waits for a member.
#[tokio::test]
async fn sandboxed_reads_auto_allow_but_commands_wait_for_members() {
    let Some(sb) = sandbox().await else { return };
    let data = tempfile::tempdir().unwrap();
    let db = Db::open(data.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(AcpHarness::new(
        "fake",
        CommandSpec::new(env!("CARGO_BIN_EXE_om-fake-acp-agent")),
    )));
    let mgr =
        WorkspaceManager::new(db.clone(), WorkspaceConfig::new(data.path().join("ws"))).unwrap();
    let orch = Orchestrator::new(db, registry, OrchestratorConfig::default())
        .with_bwrap_sandbox(Arc::new(sb))
        .with_workspaces(mgr);
    let config = SessionConfig {
        harness_id: "fake".into(),
        cwd: "/".into(),
        workspace_id: None,
        model: None,
        sandbox: SandboxMode::Bwrap,
        title: None,
    };
    let s = orch.create_session(space, config, None).await.unwrap();
    let mut sub = orch.subscribe(&s.id, 0).await.unwrap();
    let idle = |p: &EventPayload| {
        matches!(
            p,
            EventPayload::StatusChanged {
                to: SessionStatus::Idle,
                ..
            }
        )
    };
    while !idle(&next(&mut sub).await.payload) {}

    // Read inside the scratch cwd: resolved by policy, no WaitingApproval.
    orch.submit_prompt(&s.id, None, Prompt::text("read"))
        .await
        .unwrap();
    let mut rule = None;
    loop {
        let ev = next(&mut sub).await;
        match &ev.payload {
            EventPayload::StatusChanged {
                to: SessionStatus::WaitingApproval,
                ..
            } => panic!("read inside the sandbox should not wait for members"),
            EventPayload::ApprovalResolved { auto_rule, .. } => rule = auto_rule.clone(),
            p if idle(p) => break,
            _ => {}
        }
    }
    assert_eq!(rule.as_deref(), Some("read_in_worktree"));

    // A command still asks.
    orch.submit_prompt(&s.id, None, Prompt::text("tool"))
        .await
        .unwrap();
    let approval = loop {
        if let EventPayload::ApprovalRequested { approval, .. } = next(&mut sub).await.payload {
            break approval;
        }
    };
    assert_eq!(approval.tool_kind, Some(om_core::event::ToolKind::Execute));
    loop {
        if let EventPayload::StatusChanged {
            to: SessionStatus::WaitingApproval,
            ..
        } = next(&mut sub).await.payload
        {
            break;
        }
    }
    orch.resolve_approval(
        &approval.id,
        om_core::approval::ApprovalDecision::Selected {
            option_id: "reject".into(),
        },
        None,
    )
    .await
    .unwrap();
    while !idle(&next(&mut sub).await.payload) {}
    orch.close_session(&s.id).await.unwrap();
}

async fn next(sub: &mut om_server::orchestrator::Subscription) -> om_core::event::SessionEvent {
    tokio::time::timeout(Duration::from_secs(20), sub.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap()
}
