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
use om_server::sandbox::BwrapSandbox;
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

async fn run(sb: &BwrapSandbox, spec: &SandboxSpec, script: &str, cwd: Option<&Path>) -> Out {
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
    let orch =
        Orchestrator::new(db, registry, OrchestratorConfig::default()).with_bwrap_sandbox(sb);

    let config = SessionConfig {
        harness_id: "fake".into(),
        cwd: worktree.path().to_string_lossy().into_owned(),
        workspace_id: None,
        model: None,
        sandbox: SandboxMode::Bwrap,
        title: None,
    };
    let s = orch.create_session(space, config, None).await.unwrap();
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
