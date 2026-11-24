//! End-to-end integration test driving the compiled `om-cli ctl` binary against a live
//! `om-server` HTTP router backed by `Orchestrator`, `WorkspaceManager`, and `rusqlite` WAL.

use om_server::api::{ApiConfig, AppState, build_router};
use om_server::db::Db;
use om_server::harness::{HarnessRegistry, MockHarness};
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use om_server::workspace::{WorkspaceConfig, WorkspaceManager};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

const OM_CLI: &str = env!("CARGO_BIN_EXE_om-cli");
const TEST_TOKEN: &str = "ctl-e2e-token-phase1";

fn init_sample_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be installed");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@openmirasim.dev"]);
    run(&["config", "user.name", "OpenMirasim Test"]);
    std::fs::write(dir.join("README.md"), "# OpenMirasim Sample\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "init"]);
}

async fn run_ctl(base_url: &str, args: &[&str]) -> Value {
    let out = tokio::process::Command::new(OM_CLI)
        .arg("ctl")
        .args(["--server", base_url, "--token", TEST_TOKEN])
        .args(args)
        .output()
        .await
        .expect("failed to execute om-cli");
    assert!(
        out.status.success(),
        "om-cli {:?} failed (status {:?})\nstdout: {}\nstderr: {}",
        args,
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("om-cli stdout must be valid JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctl_drives_workspaces_sessions_prompts_approvals_diff_and_queue() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("sample-repo");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    init_sample_git_repo(&repo_dir);

    let db = Db::open(data_dir.join("openmirasim.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let workspaces = WorkspaceManager::new(db.clone(), WorkspaceConfig::new(&data_dir)).unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::instant()));

    let orch = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..Default::default()
        },
    )
    .with_workspaces(workspaces);

    let router = build_router(AppState {
        orchestrator: orch,
        config: ApiConfig {
            auth_token: TEST_TOKEN.into(),
            default_space_id: space_id,
            static_dir: None,
            allow_cors_origin: None,
        },
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    // 1. Health & harnesses
    let health = run_ctl(&base_url, &["health"]).await;
    assert_eq!(health["status"], "ok");

    let harnesses = run_ctl(&base_url, &["harnesses"]).await;
    assert_eq!(harnesses["harnesses"][0]["id"], "mock");

    // 2. Register Git workspace & list
    let ws = run_ctl(
        &base_url,
        &[
            "workspaces",
            "register",
            "--name",
            "sample",
            "--path",
            repo_dir.to_str().unwrap(),
        ],
    )
    .await;
    let ws_id = ws["id"].as_str().unwrap().to_string();
    let ws_list = run_ctl(&base_url, &["workspaces", "list"]).await;
    assert_eq!(ws_list["workspaces"].as_array().unwrap().len(), 1);

    // 3. Create session bound to workspace -> auto-creates isolated Git worktree
    let sess = run_ctl(
        &base_url,
        &[
            "sessions",
            "create",
            "--harness",
            "mock",
            "--workspace",
            &ws_id,
            "--sandbox",
            "direct",
            "--title",
            "CLI E2E Session",
        ],
    )
    .await;
    let sid = sess["id"].as_str().unwrap().to_string();

    // Wait briefly until session reaches `idle`
    for _ in 0..40 {
        let cur = run_ctl(&base_url, &["sessions", "get", &sid]).await;
        if cur["status"] == "idle" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // 4. Inspect worktree files, modify a file inside worktree, and verify live Git diff
    let wt = run_ctl(&base_url, &["sessions", "worktree", &sid]).await;
    let wt_path = Path::new(wt["path"].as_str().unwrap());
    let files = run_ctl(&base_url, &["sessions", "files", &sid]).await;
    assert!(
        files["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["path"] == "README.md")
    );
    std::fs::write(wt_path.join("README.md"), "# OpenMirasim Sample\nEdited!\n").unwrap();
    let diff = run_ctl(&base_url, &["sessions", "diff", &sid]).await;
    assert!(diff["patch"].as_str().unwrap().contains("+Edited!"));

    let content = run_ctl(&base_url, &["sessions", "file", &sid, "README.md"]).await;
    assert_eq!(content["utf8"], true);
    assert!(content["content"].as_str().unwrap().contains("Edited!"));

    // 5. Submit approval-gated prompt, list pending approvals, resolve via CLI
    let outcome = run_ctl(
        &base_url,
        &["prompt", &sid, "please approve deploy", "--mode", "queue"],
    )
    .await;
    assert_eq!(outcome["outcome"], "started");

    let mut approval_id = None;
    for _ in 0..40 {
        let list = run_ctl(&base_url, &["approvals", "list", &sid, "--pending"]).await;
        if let Some(first) = list["approvals"].as_array().and_then(|a| a.first()) {
            approval_id = first["id"].as_str().map(ToOwned::to_owned);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let approval_id = approval_id.expect("expected pending approval");

    let resolved = run_ctl(
        &base_url,
        &[
            "approvals",
            "resolve",
            &approval_id,
            "--option",
            "allow_once",
        ],
    )
    .await;
    assert_eq!(resolved["status"], "resolved");

    // 6. Phase 4: MCP Hub, Skills lifecycle, Effective merge & Schedules via `om-cli ctl`
    let mcp = run_ctl(
        &base_url,
        &[
            "mcp",
            "add",
            "--name",
            "github",
            "--scope",
            "space",
            "--transport",
            "stdio",
            "--command",
            "/bin/sh",
            "--env-key",
            "GITHUB_TOKEN",
        ],
    )
    .await;
    let mcp_id = mcp["id"].as_str().unwrap().to_string();
    let probe = run_ctl(&base_url, &["mcp", "probe", &mcp_id]).await;
    assert_eq!(probe["reachable"], true);

    let skill = run_ctl(
        &base_url,
        &[
            "skills",
            "install",
            "--name",
            "pr-reviewer",
            "--scope",
            "space",
            "--instructions",
            "# Reviewer",
        ],
    )
    .await;
    assert_eq!(skill["name"], "pr-reviewer");

    let eff = run_ctl(&base_url, &["hub", "effective", "--workspace", &ws_id]).await;
    assert_eq!(eff["mcp_servers"].as_array().unwrap().len(), 1);
    assert_eq!(eff["skills"].as_array().unwrap().len(), 1);

    let sched = run_ctl(
        &base_url,
        &[
            "schedules",
            "create",
            "--name",
            "CLI Schedule",
            "--interval-secs",
            "3600",
            "--session",
            &sid,
            "--prompt",
            "ping",
        ],
    )
    .await;
    assert_eq!(sched["name"], "CLI Schedule");

    // 7. Query audit logs & close session
    let audit = run_ctl(&base_url, &["audit", "--limit", "10"]).await;
    assert!(audit["entries"].is_array());

    let closed = run_ctl(&base_url, &["sessions", "close", &sid]).await;
    assert_eq!(closed["ok"], true);
}
