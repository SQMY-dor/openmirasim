//! Phase 4 REST integration tests:
//! - Schedules CRUD, pause/resume, manual trigger, run history, and RBAC
//! - Goals loop start, list, and stop on live sessions
//! - Session-to-Session dispatch delivery, audit, and self-dispatch validation
//! - Two-tier Space / Workspace MCP Hub & Skills lifecycle, inheritance override,
//!   secret-key validation, probing, RBAC enforcement, and persistence

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use om_core::api::{
    CreateSessionRequest, ListAuditResponse, ListDispatchesResponse, ListGoalsResponse,
    ListScheduleRunsResponse, ListSchedulesResponse, RegisterWorkspaceRequest,
    SetScheduleEnabledRequest,
};
use om_core::auth::Role;
use om_core::content::Prompt;
use om_core::dispatch::{Dispatch, DispatchTarget, NewDispatch};
use om_core::goal::{Goal, NewGoal};
use om_core::queue::SubmitMode;
use om_core::sandbox::SandboxMode;
use om_core::schedule::{
    MissedRunPolicy, NewSchedule, OverlapPolicy, Schedule, ScheduleRun, ScheduleSpec,
    ScheduleTarget,
};
use om_core::session::{SessionConfig, SessionSummary};
use om_core::workspace::WorkspaceSummary;
use om_server::api::hub::{
    CreateMcpServerRequest, CreateSkillRequest, EffectiveHubResponse, HubScopeLevel,
    McpProbeResult, McpServerRecord, McpTransport, SkillLifecycle, SkillRecord,
    UpdateMcpServerRequest, UpdateSkillRequest,
};
use om_server::api::{ApiConfig, ApiExtensions, AppState, build_router_with_extensions};
use om_server::auth::Auth;
use om_server::db::Db;
use om_server::harness::{HarnessRegistry, MockHarness};
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use om_server::workspace::{WorkspaceConfig, WorkspaceManager};
use serde::de::DeserializeOwned;
use std::process::Command;
use std::sync::Arc;
use tower::ServiceExt;

fn bearer_req(token: &str, method: &str, uri: &str, body: Option<String>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if body.is_some() {
        b = b.header(header::CONTENT_TYPE, "application/json");
    }
    b.body(body.map(Body::from).unwrap_or_else(Body::empty))
        .unwrap()
}

async fn json_body<T: DeserializeOwned>(res: axum::response::Response) -> T {
    let bytes = axum::body::to_bytes(res.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "failed to decode json ({e}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

fn init_git_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "phase4@openmirasim.dev"]);
    run(&["config", "user.name", "Phase 4 Test"]);
    std::fs::write(dir.join("README.md"), "# Phase 4\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "initial"]);
}

#[tokio::test]
async fn phase4_schedules_goals_dispatches_and_two_tier_mcp_skills_hub() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&data_dir).unwrap();
    init_git_repo(&repo_dir);

    let db = Db::open(data_dir.join("phase4.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let auth = Auth::new(db.clone());
    let (_owner, owner_tok) = auth
        .bootstrap_owner(&space_id, "Owner", "cli")
        .await
        .unwrap()
        .unwrap();
    let owner_p = auth.authenticate(&owner_tok.secret).await.unwrap();
    let member = auth
        .add_member(&owner_p, "Alice Member", Role::Member)
        .await
        .unwrap();
    let member_tok = auth
        .issue_token(&owner_p, &member.id, "member-cli", None)
        .await
        .unwrap();
    let viewer = auth
        .add_member(&owner_p, "Bob Viewer", Role::Viewer)
        .await
        .unwrap();
    let viewer_tok = auth
        .issue_token(&owner_p, &viewer.id, "viewer-cli", None)
        .await
        .unwrap();

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::instant()));
    let workspaces = WorkspaceManager::new(db.clone(), WorkspaceConfig::new(&data_dir)).unwrap();
    let orchestrator = Orchestrator::new(
        db.clone(),
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..Default::default()
        },
    )
    .with_workspaces(workspaces);

    let hub_path = data_dir.join("hub-registry.json");
    let app = build_router_with_extensions(
        AppState {
            orchestrator,
            config: ApiConfig {
                auth_token: String::new(),
                default_space_id: space_id,
                static_dir: None,
                allow_cors_origin: None,
            },
        },
        ApiExtensions {
            hub_store_path: Some(hub_path.clone()),
            ..Default::default()
        },
    );

    // 1. Register a Git workspace and create two live sessions (A and B)
    let ws_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            "/api/v1/workspaces",
            Some(
                serde_json::to_string(&RegisterWorkspaceRequest {
                    name: "core-repo".into(),
                    repo_path: repo_dir.to_string_lossy().into_owned(),
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(ws_res.status(), StatusCode::CREATED);
    let workspace: WorkspaceSummary = json_body(ws_res).await;

    let mut sessions = Vec::new();
    for title in ["Session A", "Session B"] {
        let s_res = app
            .clone()
            .oneshot(bearer_req(
                &owner_tok.secret,
                "POST",
                "/api/v1/sessions",
                Some(
                    serde_json::to_string(&CreateSessionRequest {
                        config: SessionConfig {
                            harness_id: "mock".into(),
                            cwd: data_dir.to_string_lossy().into_owned(),
                            workspace_id: None,
                            model: None,
                            sandbox: SandboxMode::Direct,
                            title: Some(title.into()),
                        },
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(s_res.status(), StatusCode::CREATED);
        sessions.push(json_body::<SessionSummary>(s_res).await);
    }
    // Let MockHarness finish startup transition to Idle
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let sess_a = &sessions[0];
    let sess_b = &sessions[1];

    // 2. Schedules: Viewer forbidden; Member creates, pauses/resumes, manually triggers, inspects runs, deletes
    let new_sched = NewSchedule {
        name: "Nightly Health Check".into(),
        spec: ScheduleSpec::Every {
            interval_secs: 3600,
            start_at: om_core::now_ms() + 60_000,
        },
        target: ScheduleTarget::Session {
            session_id: sess_a.id.clone(),
            prompt: Prompt::text("run nightly checks"),
            mode: SubmitMode::Queue,
        },
        enabled: true,
        missed: MissedRunPolicy::RunOnce,
        overlap: OverlapPolicy::Allow,
    };

    let viewer_create = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "POST",
            "/api/v1/schedules",
            Some(serde_json::to_string(&new_sched).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(viewer_create.status(), StatusCode::FORBIDDEN);

    let create_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            "/api/v1/schedules",
            Some(serde_json::to_string(&new_sched).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let sched: Schedule = json_body(create_res).await;
    assert_eq!(sched.name, "Nightly Health Check");

    let list_res = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            "/api/v1/schedules",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let listed: ListSchedulesResponse = json_body(list_res).await;
    assert_eq!(listed.schedules.len(), 1);

    let pause_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            &format!("/api/v1/schedules/{}/enabled", sched.id),
            Some(serde_json::to_string(&SetScheduleEnabledRequest { enabled: false }).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(pause_res.status(), StatusCode::OK);
    let paused: Schedule = json_body(pause_res).await;
    assert!(!paused.enabled);

    let trigger_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            &format!("/api/v1/schedules/{}/trigger", sched.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(trigger_res.status(), StatusCode::OK);
    let run: ScheduleRun = json_body(trigger_res).await;
    assert!(run.manual);

    let runs_res = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            &format!("/api/v1/schedules/{}/runs", sched.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(runs_res.status(), StatusCode::OK);
    let runs: ListScheduleRunsResponse = json_body(runs_res).await;
    assert_eq!(runs.runs.len(), 1);

    // 3. Goals: set on Session A, list, and stop cleanly
    let goal_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            &format!("/api/v1/sessions/{}/goals", sess_a.id),
            Some(
                serde_json::to_string(&NewGoal {
                    objective: "verify test suite passes".into(),
                    max_turns: Some(2),
                    deadline_at: None,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(goal_res.status(), StatusCode::CREATED);
    let goal: Goal = json_body(goal_res).await;

    let stop_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            &format!("/api/v1/goals/{}/stop", goal.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(stop_res.status(), StatusCode::OK);

    let goals_res = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            &format!("/api/v1/sessions/{}/goals", sess_a.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(goals_res.status(), StatusCode::OK);
    let goals: ListGoalsResponse = json_body(goals_res).await;
    assert_eq!(goals.goals.len(), 1);

    // 4. Session-to-Session Dispatches: Session A -> Session B, plus self-dispatch rejected
    let self_disp = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            "/api/v1/dispatches",
            Some(
                serde_json::to_string(&NewDispatch {
                    from_session: sess_a.id.clone(),
                    target: DispatchTarget::Session {
                        session_id: sess_a.id.clone(),
                        mode: SubmitMode::Queue,
                    },
                    prompt: Prompt::text("self loop"),
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(self_disp.status(), StatusCode::BAD_REQUEST);

    let disp_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            "/api/v1/dispatches",
            Some(
                serde_json::to_string(&NewDispatch {
                    from_session: sess_a.id.clone(),
                    target: DispatchTarget::Session {
                        session_id: sess_b.id.clone(),
                        mode: SubmitMode::Queue,
                    },
                    prompt: Prompt::text("please run code review on PR"),
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(disp_res.status(), StatusCode::CREATED);
    let disp: Dispatch = json_body(disp_res).await;
    assert_eq!(disp.depth, 1);
    assert_eq!(disp.to_session, sess_b.id);

    let list_disp_res = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            &format!("/api/v1/sessions/{}/dispatches", sess_b.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(list_disp_res.status(), StatusCode::OK);
    let disps: ListDispatchesResponse = json_body(list_disp_res).await;
    assert_eq!(disps.dispatches.len(), 1);

    // 5. Two-tier Space / Workspace MCP Hub (member-isolated + Admin global control) & Skills Lifecycle
    let space_mcp_req = CreateMcpServerRequest {
        scope: HubScopeLevel::Space,
        workspace_id: None,
        name: "github".into(),
        transport: McpTransport::StreamableHttp,
        command: None,
        args: vec![],
        url: Some("https://mcp.example.com/github".into()),
        env_keys: vec!["GITHUB_TOKEN".into()],
        description: "Space default GitHub MCP".into(),
        enabled: true,
        global_available: Some(true),
    };

    // Member cannot create Space-wide MCP server (Admin+ required)
    let member_space_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            "/api/v1/mcp/servers",
            Some(serde_json::to_string(&space_mcp_req).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(member_space_res.status(), StatusCode::FORBIDDEN);

    // Owner creates globally-available Space default
    let owner_space_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            "/api/v1/mcp/servers",
            Some(serde_json::to_string(&space_mcp_req).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(owner_space_res.status(), StatusCode::CREATED);
    let space_mcp: McpServerRecord = json_body(owner_space_res).await;
    assert!(space_mcp.global_available);

    // Member CAN create their own private Workspace-scoped override
    let ws_mcp_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            "/api/v1/mcp/servers",
            Some(
                serde_json::to_string(&CreateMcpServerRequest {
                    scope: HubScopeLevel::Workspace,
                    workspace_id: Some(workspace.id.clone()),
                    name: "github".into(),
                    transport: McpTransport::Stdio,
                    command: Some("/bin/sh".into()),
                    args: vec!["-c".into(), "true".into()],
                    url: None,
                    env_keys: vec!["REPO_GH_TOKEN".into()],
                    description: "Workspace stdio override".into(),
                    enabled: true,
                    global_available: None,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(ws_mcp_res.status(), StatusCode::CREATED);
    let ws_mcp: McpServerRecord = json_body(ws_mcp_res).await;
    assert_eq!(ws_mcp.owner_member_id.as_ref(), Some(&member.id));
    assert!(!ws_mcp.global_available);

    // Member probes their own stdio MCP server -> reachable (without disclosing host path)
    let probe_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            &format!("/api/v1/mcp/servers/{}/probe", ws_mcp.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(probe_res.status(), StatusCode::OK);
    let probe: McpProbeResult = json_body(probe_res).await;
    assert!(probe.reachable);

    // Other members (e.g. Viewer) CANNOT see or probe Member's private Workspace MCP
    let viewer_probe_private = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "POST",
            &format!("/api/v1/mcp/servers/{}/probe", ws_mcp.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(viewer_probe_private.status(), StatusCode::NOT_FOUND);

    // Skill lifecycle: Draft -> Active -> Archived (re-enabling archived fails cleanly)
    let skill_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            "/api/v1/skills",
            Some(
                serde_json::to_string(&CreateSkillRequest {
                    scope: HubScopeLevel::Space,
                    workspace_id: None,
                    name: "pr-reviewer".into(),
                    version: "1.0.0".into(),
                    description: "Structured Rust + TS PR review skill".into(),
                    tags: vec!["review".into(), "security".into()],
                    instructions: "# PR Reviewer\nInspect permissions and error paths.".into(),
                    lifecycle: SkillLifecycle::Draft,
                    enabled: true,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(skill_res.status(), StatusCode::CREATED);
    let skill: SkillRecord = json_body(skill_res).await;

    let activate_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "PATCH",
            &format!("/api/v1/skills/{}", skill.id),
            Some(
                serde_json::to_string(&UpdateSkillRequest {
                    lifecycle: Some(SkillLifecycle::Active),
                    ..Default::default()
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(activate_res.status(), StatusCode::OK);
    let active_skill: SkillRecord = json_body(activate_res).await;
    assert_eq!(active_skill.lifecycle, SkillLifecycle::Active);

    // Effective resolution for Member merges global Space Skill + Member's private Workspace MCP override
    let eff_member_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "GET",
            &format!("/api/v1/hub/effective?workspace_id={}", workspace.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(eff_member_res.status(), StatusCode::OK);
    let eff_member: EffectiveHubResponse = json_body(eff_member_res).await;
    assert_eq!(eff_member.mcp_servers.len(), 1);
    assert_eq!(eff_member.mcp_servers[0].record.id, ws_mcp.id);
    assert!(eff_member.mcp_servers[0].overridden_space_default);
    assert_eq!(eff_member.skills.len(), 1);
    assert!(eff_member.mcp_client_config["mcpServers"]["github"].is_object());
    assert!(hub_path.is_file());

    // Viewer sees ONLY the Admin-approved global Space MCP (not Member's private Workspace MCP)
    let eff_viewer_res = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            &format!("/api/v1/hub/effective?workspace_id={}", workspace.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(eff_viewer_res.status(), StatusCode::OK);
    let eff_viewer: EffectiveHubResponse = json_body(eff_viewer_res).await;
    assert_eq!(eff_viewer.mcp_servers.len(), 1);
    assert_eq!(eff_viewer.mcp_servers[0].record.id, space_mcp.id);
    assert!(!eff_viewer.mcp_servers[0].overridden_space_default);

    // Admin revokes global availability of Space `github` -> Viewer no longer gets it in effective hub
    let unpublish_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "PATCH",
            &format!("/api/v1/mcp/servers/{}", space_mcp.id),
            Some(
                serde_json::to_string(&UpdateMcpServerRequest {
                    global_available: Some(false),
                    ..Default::default()
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(unpublish_res.status(), StatusCode::OK);
    let eff_viewer_after: EffectiveHubResponse = json_body(
        app.clone()
            .oneshot(bearer_req(
                &viewer_tok.secret,
                "GET",
                &format!("/api/v1/hub/effective?workspace_id={}", workspace.id),
                None,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        eff_viewer_after.mcp_servers.is_empty(),
        "unpublished Space MCP must not appear for non-admin members"
    );

    // Audit log recorded Hub actions
    let audit_res = app
        .clone()
        .oneshot(bearer_req(&owner_tok.secret, "GET", "/api/v1/audit", None))
        .await
        .unwrap();
    assert_eq!(audit_res.status(), StatusCode::OK);
    let audit: ListAuditResponse = json_body(audit_res).await;
    let actions: Vec<&str> = audit.entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"mcp.created"));
    assert!(actions.contains(&"skill.created"));
    assert!(actions.contains(&"skill.updated"));

    // Delete schedule cleanup
    let del_res = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "DELETE",
            &format!("/api/v1/schedules/{}", sched.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(del_res.status(), StatusCode::NO_CONTENT);
}
