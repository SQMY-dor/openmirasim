//! End-to-end integration tests for W0-5 Axum REST + `/ws/events` wired to real
//! `Orchestrator`, real `Db` (`rusqlite` WAL), `MockHarness`, and `om-fake-acp-agent`.

use axum::body::Body;
use futures::{SinkExt, StreamExt};
use http::{Request, StatusCode, header};
use om_core::api::{
    ApiError, ApiErrorCode, ClientMessage, CreateSessionRequest, ListApprovalsResponse,
    ListEventsResponse, ListSessionsResponse, ResolveApprovalRequest, ServerMessage,
    SubmitPromptRequest, SubmitPromptResponse,
};
use om_core::approval::{ApprovalDecision, ApprovalStatus};
use om_core::content::Prompt;
use om_core::event::EventPayload;
use om_core::sandbox::{CommandSpec, SandboxMode};
use om_core::session::{SessionConfig, SessionStatus, SessionSummary, StopReason};
use om_server::acp::AcpHarness;
use om_server::api::{ApiConfig, AppState, build_router};
use om_server::db::Db;
use om_server::harness::MockHarness;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt;

const TEST_TOKEN: &str = "test-static-token-phase0";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_om-fake-acp-agent");

async fn setup_server(
    db_path: PathBuf,
    static_dir: Option<PathBuf>,
    broadcast_capacity: usize,
) -> (Orchestrator, axum::Router) {
    let db = Db::open(&db_path).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::instant()));
    registry.register(Arc::new(AcpHarness::new(
        "fake-acp",
        CommandSpec::new(FAKE_AGENT),
    )));

    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            broadcast_capacity,
            ..Default::default()
        },
    );
    let _ = orchestrator.recover().await.unwrap();

    let state = AppState {
        orchestrator: orchestrator.clone(),
        config: ApiConfig {
            auth_token: TEST_TOKEN.into(),
            default_space_id: space_id,
            static_dir,
            allow_cors_origin: Some("http://127.0.0.1:5173".into()),
        },
    };
    let router = build_router(state);
    (orchestrator, router)
}

async fn spawn_tcp_server(router: axum::Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

async fn read_body_json<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn authed_req(method: &str, uri: &str, body: Option<String>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"));
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    builder
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .unwrap()
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_send(ws: &mut WsStream, msg: &ClientMessage) {
    ws.send(WsMessage::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .unwrap();
}

async fn ws_recv(ws: &mut WsStream) -> ServerMessage {
    let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("timed out waiting for WS frame")
        .expect("WS closed unexpectedly")
        .expect("WS error");
    match frame {
        WsMessage::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text frame, got {other:?}"),
    }
}

async fn connect_ws(addr: SocketAddr) -> WsStream {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/events?token={TEST_TOKEN}"))
            .await
            .unwrap();
    ws_send(
        &mut ws,
        &ClientMessage::Hello {
            protocol_version: 1,
        },
    )
    .await;
    assert!(matches!(
        ws_recv(&mut ws).await,
        ServerMessage::Welcome {
            protocol_version: 1,
            ..
        }
    ));
    ws
}

#[tokio::test]
async fn full_rest_ws_approval_and_resume_with_real_fake_acp_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (_orch, app) = setup_server(dir.path().join("acp.db"), None, 64).await;
    let addr = spawn_tcp_server(app.clone()).await;

    // 1. /healthz is public; /api/v1 requires Bearer token; /ws/events rejects wrong token
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    let err: ApiError = read_body_json(unauth).await;
    assert_eq!(err.code, ApiErrorCode::Unauthorized);

    assert!(
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/events?token=wrong"))
            .await
            .is_err()
    );

    // 2. Create session backed by real `om-fake-acp-agent` subprocess
    let create_req = CreateSessionRequest {
        config: SessionConfig {
            harness_id: "fake-acp".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            workspace_id: None,
            model: None,
            sandbox: SandboxMode::Direct,
            title: Some("ACP Full-Stack Test".into()),
        },
    };
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            "/api/v1/sessions",
            Some(serde_json::to_string(&create_req).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: SessionSummary = read_body_json(res).await;

    // 3. Subscribe over WS and wait for HarnessReady -> Idle
    let mut ws = connect_ws(addr).await;
    ws_send(
        &mut ws,
        &ClientMessage::Subscribe {
            session_id: created.id.clone(),
            last_seq: 0,
        },
    )
    .await;
    assert!(matches!(
        ws_recv(&mut ws).await,
        ServerMessage::Subscribed { .. }
    ));

    let mut last_seq = 0u64;
    loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws).await {
            assert_eq!(event.seq, last_seq + 1);
            last_seq = event.seq;
            if matches!(
                event.payload,
                EventPayload::StatusChanged {
                    to: SessionStatus::Idle,
                    ..
                }
            ) {
                break;
            }
        }
    }

    // 4. Submit approval-triggering prompt ("tool") to real `om-fake-acp-agent`
    let prompt_req = SubmitPromptRequest {
        prompt: Prompt::text("tool"),
    };
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{}/prompts", created.id),
            Some(serde_json::to_string(&prompt_req).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let _: SubmitPromptResponse = read_body_json(res).await;

    let (approval_id, allow_option_id) = loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws).await {
            assert_eq!(event.seq, last_seq + 1);
            last_seq = event.seq;
            if let EventPayload::ApprovalRequested { approval, .. } = event.payload {
                let opt = approval
                    .options
                    .iter()
                    .find(|o| o.kind.is_allow())
                    .expect("expected allow option")
                    .option_id
                    .clone();
                break (approval.id, opt);
            }
        }
    };

    // 5. Resolve approval via REST; second resolve returns 409 Conflict
    let resolve_body = serde_json::to_string(&ResolveApprovalRequest {
        decision: ApprovalDecision::Selected {
            option_id: allow_option_id,
        },
    })
    .unwrap();
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/approvals/{approval_id}/resolve"),
            Some(resolve_body.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let dup = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/approvals/{approval_id}/resolve"),
            Some(resolve_body),
        ))
        .await
        .unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);

    // Wait for turn completion and Idle
    let mut saw_end_turn = false;
    loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws).await {
            assert_eq!(event.seq, last_seq + 1);
            last_seq = event.seq;
            if matches!(
                event.payload,
                EventPayload::TurnEnded {
                    stop_reason: StopReason::EndTurn,
                    ..
                }
            ) {
                saw_end_turn = true;
            }
            if saw_end_turn
                && matches!(
                    event.payload,
                    EventPayload::StatusChanged {
                        to: SessionStatus::Idle,
                        ..
                    }
                )
            {
                break;
            }
        }
    }

    // 6. Disconnect WS, run another turn while offline, reconnect from `last_seq` -> gapless catchup
    ws.close(None).await.unwrap();
    let offline_seq = last_seq;

    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{}/prompts", created.id),
            Some(
                serde_json::to_string(&SubmitPromptRequest {
                    prompt: Prompt::text("offline turn"),
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let mut ws2 = connect_ws(addr).await;
    ws_send(
        &mut ws2,
        &ClientMessage::Subscribe {
            session_id: created.id.clone(),
            last_seq: offline_seq,
        },
    )
    .await;
    assert!(matches!(
        ws_recv(&mut ws2).await,
        ServerMessage::Subscribed { .. }
    ));

    let mut caught_up_seq = offline_seq;
    loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws2).await {
            assert_eq!(event.seq, caught_up_seq + 1);
            caught_up_seq = event.seq;
            if matches!(
                event.payload,
                EventPayload::StatusChanged {
                    to: SessionStatus::Idle,
                    ..
                }
            ) {
                break;
            }
        }
    }
    assert!(caught_up_seq > offline_seq);

    // 7. REST events pagination
    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{}/events?after_seq=0&limit=3", created.id),
            None,
        ))
        .await
        .unwrap();
    let page: ListEventsResponse = read_body_json(res).await;
    assert_eq!(page.events.len(), 3);
    assert!(page.has_more);
    assert_eq!(page.head_seq, caught_up_seq);
}

#[tokio::test]
async fn server_restart_recovery_and_resume_over_rest_and_ws() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("restart.db");

    // Lifetime 1: create MockHarness session and leave it waiting on an approval.
    let session_id = {
        let (_orch, app) = setup_server(db_path.clone(), None, 64).await;
        let addr = spawn_tcp_server(app.clone()).await;

        let res = app
            .clone()
            .oneshot(authed_req(
                "POST",
                "/api/v1/sessions",
                Some(
                    serde_json::to_string(&CreateSessionRequest {
                        config: SessionConfig {
                            harness_id: "mock".into(),
                            cwd: dir.path().to_string_lossy().into_owned(),
                            workspace_id: None,
                            model: None,
                            sandbox: SandboxMode::Direct,
                            title: Some("Survives Restart".into()),
                        },
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        let created: SessionSummary = read_body_json(res).await;

        let mut ws = connect_ws(addr).await;
        ws_send(
            &mut ws,
            &ClientMessage::Subscribe {
                session_id: created.id.clone(),
                last_seq: 0,
            },
        )
        .await;
        let _ = ws_recv(&mut ws).await; // Subscribed
        loop {
            if let ServerMessage::Event { event } = ws_recv(&mut ws).await
                && matches!(
                    event.payload,
                    EventPayload::StatusChanged {
                        to: SessionStatus::Idle,
                        ..
                    }
                )
            {
                break;
            }
        }

        let res = app
            .clone()
            .oneshot(authed_req(
                "POST",
                &format!("/api/v1/sessions/{}/prompts", created.id),
                Some(
                    serde_json::to_string(&SubmitPromptRequest {
                        prompt: Prompt::text("please approve deploy"),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        loop {
            if let ServerMessage::Event { event } = ws_recv(&mut ws).await
                && matches!(event.payload, EventPayload::ApprovalRequested { .. })
            {
                break;
            }
        }
        created.id
    };

    // Lifetime 2: reopen same SQLite file -> recover_after_restart() runs inside setup_server.
    let (_orch2, app2) = setup_server(db_path, None, 64).await;

    let res = app2
        .clone()
        .oneshot(authed_req("GET", "/api/v1/sessions", None))
        .await
        .unwrap();
    let list: ListSessionsResponse = read_body_json(res).await;
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(list.sessions[0].id, session_id);
    assert_eq!(list.sessions[0].status, SessionStatus::Failed);

    let res = app2
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{session_id}/approvals"),
            None,
        ))
        .await
        .unwrap();
    let approvals: ListApprovalsResponse = read_body_json(res).await;
    assert_eq!(approvals.approvals.len(), 1);
    assert_eq!(approvals.approvals[0].status, ApprovalStatus::Cancelled);

    // Resume via REST -> session returns to Idle and accepts a new prompt.
    let res = app2
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{session_id}/resume"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let addr2 = spawn_tcp_server(app2.clone()).await;
    let mut ws2 = connect_ws(addr2).await;
    ws_send(
        &mut ws2,
        &ClientMessage::Subscribe {
            session_id: session_id.clone(),
            last_seq: 0,
        },
    )
    .await;
    let _ = ws_recv(&mut ws2).await; // Subscribed

    let mut seq = 0u64;
    let mut saw_error = false;
    loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws2).await {
            assert_eq!(event.seq, seq + 1);
            seq = event.seq;
            if matches!(event.payload, EventPayload::Error { .. }) {
                saw_error = true;
            }
            if saw_error
                && matches!(
                    event.payload,
                    EventPayload::StatusChanged {
                        to: SessionStatus::Idle,
                        ..
                    }
                )
            {
                break;
            }
        }
    }
    assert!(saw_error);
}

#[tokio::test]
async fn static_spa_fallback_serves_index_without_swallowing_api_routes() {
    let dir = tempfile::tempdir().unwrap();
    let web_dir = dir.path().join("dist");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>OpenMirasim SPA</html>").unwrap();

    let (_orch, app) = setup_server(dir.path().join("spa.db"), Some(web_dir), 16).await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sessions/abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("OpenMirasim SPA"));

    let api_res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api_res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn phase1_workspaces_worktree_queue_rules_probe_and_audit_routes() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("sample-repo");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Initialize a real temporary Git repository
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo_dir)
            .output()
            .expect("git must be installed");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@openmirasim.dev"]);
    git(&["config", "user.name", "OpenMirasim Test"]);
    std::fs::write(repo_dir.join("hello.txt"), "initial\n").unwrap();
    git(&["add", "hello.txt"]);
    git(&["commit", "-m", "initial commit"]);

    let db = Db::open(data_dir.join("phase1.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let workspaces = om_server::workspace::WorkspaceManager::new(
        db.clone(),
        om_server::workspace::WorkspaceConfig::new(&data_dir),
    )
    .unwrap();
    let registry = om_server::harness::HarnessRegistry::with_phase1_defaults(false);
    // Replace mock with instant mock for deterministic test speed
    registry.register(Arc::new(MockHarness::instant()));

    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..Default::default()
        },
    )
    .with_workspaces(workspaces);

    let app = build_router(AppState {
        orchestrator,
        config: ApiConfig {
            auth_token: TEST_TOKEN.into(),
            default_space_id: space_id,
            static_dir: None,
            allow_cors_origin: None,
        },
    });
    let addr = spawn_tcp_server(app.clone()).await;

    // 1. Harness probe & provider routes
    let res = app
        .clone()
        .oneshot(authed_req("GET", "/api/v1/harnesses/probe", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let probed: serde_json::Value = read_body_json(res).await;
    let probe_ids: Vec<&str> = probed["probes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    for expected in ["mock", "opencode", "dsh", "cc", "codexcli"] {
        assert!(
            probe_ids.contains(&expected),
            "missing probe for {expected}"
        );
    }

    let res = app
        .clone()
        .oneshot(authed_req("GET", "/api/v1/providers", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // 2. Register & list Git workspace
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            "/api/v1/workspaces",
            Some(
                serde_json::json!({
                    "name": "demo-repo",
                    "repo_path": repo_dir.to_string_lossy(),
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let ws: serde_json::Value = read_body_json(res).await;
    let ws_id = ws["id"].as_str().unwrap().to_string();

    let res = app
        .clone()
        .oneshot(authed_req("GET", "/api/v1/workspaces", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ws_list: serde_json::Value = read_body_json(res).await;
    assert_eq!(ws_list["workspaces"].as_array().unwrap().len(), 1);

    // 3. Create session bound to workspace -> auto-provisions worktree
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            "/api/v1/sessions",
            Some(
                serde_json::to_string(&CreateSessionRequest {
                    config: SessionConfig {
                        harness_id: "mock".into(),
                        cwd: String::new(),
                        workspace_id: Some(ws_id.clone().into()),
                        model: None,
                        sandbox: SandboxMode::Direct,
                        title: Some("Phase 1 Route Test".into()),
                    },
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: SessionSummary = read_body_json(res).await;
    let sid = created.id.as_str().to_string();

    // Wait for Idle via WS
    let mut ws_conn = connect_ws(addr).await;
    ws_send(
        &mut ws_conn,
        &ClientMessage::Subscribe {
            session_id: created.id.clone(),
            last_seq: 0,
        },
    )
    .await;
    let _ = ws_recv(&mut ws_conn).await;
    loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws_conn).await
            && matches!(
                event.payload,
                EventPayload::StatusChanged {
                    to: SessionStatus::Idle,
                    ..
                }
            )
        {
            break;
        }
    }

    // 4. Worktree inspection, file read, live diff
    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{sid}/worktree"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let wt: serde_json::Value = read_body_json(res).await;
    let wt_path = PathBuf::from(wt["path"].as_str().unwrap());

    std::fs::write(wt_path.join("hello.txt"), "initial\nupdated in worktree\n").unwrap();

    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{sid}/files"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let files: serde_json::Value = read_body_json(res).await;
    assert!(
        files["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["path"] == "hello.txt")
    );

    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{sid}/file?path=hello.txt"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let file_body: serde_json::Value = read_body_json(res).await;
    assert!(
        file_body["content"]
            .as_str()
            .unwrap()
            .contains("updated in worktree")
    );

    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{sid}/diff"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let diff: serde_json::Value = read_body_json(res).await;
    assert!(
        diff["patch"]
            .as_str()
            .unwrap()
            .contains("+updated in worktree")
    );

    // 5. W2-3 Queue: start an approval-gated turn so the session stays busy deterministically
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{sid}/prompts"),
            Some(
                serde_json::json!({
                    "prompt": { "content": [{ "type": "text", "text": "please approve deploy" }] },
                    "mode": "queue",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let approval_id = loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws_conn).await
            && let EventPayload::ApprovalRequested { approval, .. } = event.payload
        {
            break approval.id;
        }
    };

    // Enqueue two follow-up prompts while session is WaitingApproval
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{sid}/prompts"),
            Some(
                serde_json::json!({
                    "prompt": { "content": [{ "type": "text", "text": "second" }] },
                    "mode": "queue",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let q1: serde_json::Value = read_body_json(res).await;
    assert_eq!(q1["outcome"], "queued");
    let q1_id = q1["queued_id"].as_str().unwrap().to_string();

    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{sid}/prompts"),
            Some(
                serde_json::json!({
                    "prompt": { "content": [{ "type": "text", "text": "third-next" }] },
                    "mode": "next",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let q2: serde_json::Value = read_body_json(res).await;
    assert_eq!(q2["outcome"], "queued");
    let q2_id = q2["queued_id"].as_str().unwrap().to_string();

    // List queue -> `next` prompt is at index 0
    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{sid}/queue"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let queue: serde_json::Value = read_body_json(res).await;
    let items = queue["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], q2_id);
    assert_eq!(items[1]["id"], q1_id);

    // Move q1 to front (index 0) and withdraw q2
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/sessions/{sid}/queue/{q1_id}/move"),
            Some(serde_json::json!({ "position": 0 }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let moved: serde_json::Value = read_body_json(res).await;
    assert_eq!(moved["items"][0]["id"], q1_id);

    let res = app
        .clone()
        .oneshot(authed_req(
            "DELETE",
            &format!("/api/v1/sessions/{sid}/queue/{q2_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // 6. Resolve approval with `allow_always` -> verify remembered approval rule + clear
    let res = app
        .clone()
        .oneshot(authed_req(
            "POST",
            &format!("/api/v1/approvals/{approval_id}/resolve"),
            Some(
                serde_json::to_string(&ResolveApprovalRequest {
                    decision: ApprovalDecision::Selected {
                        option_id: "allow_always".into(),
                    },
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .clone()
        .oneshot(authed_req(
            "GET",
            &format!("/api/v1/sessions/{sid}/approval-rules"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let rules: serde_json::Value = read_body_json(res).await;
    assert_eq!(rules["rules"].as_array().unwrap().len(), 1);

    let res = app
        .clone()
        .oneshot(authed_req(
            "DELETE",
            &format!("/api/v1/sessions/{sid}/approval-rules"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cleared: serde_json::Value = read_body_json(res).await;
    assert_eq!(cleared["cleared"], 1);

    // 7. Audit log route returns structured entries
    let res = app
        .oneshot(authed_req("GET", "/api/v1/audit?limit=20", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let audit: serde_json::Value = read_body_json(res).await;
    assert!(
        !audit["entries"].as_array().unwrap().is_empty(),
        "expected audit entries after approval resolution"
    );
}
