//! Phase 3 native PTY integration tests:
//! - Independent `/ws/pty/{id}` Binary stream + resize + minimal env isolation
//! - Bounded scrollback ring-buffer catch-up on reconnect + Viewer read-only enforcement
//! - asciicast v2 (`.cast`) recording creation, download, and JSON validation
//! - Zero blocking of `/ws/events` and clean child termination on `DELETE /pty` / `close_session`

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use om_core::api::{ClientMessage, CreateSessionRequest, ServerMessage};
use om_core::auth::Role;
use om_core::event::EventPayload;
use om_core::sandbox::SandboxMode;
use om_core::session::{SessionConfig, SessionStatus, SessionSummary};
use om_server::api::{ApiConfig, ApiExtensions, AppState, build_router_with_extensions};
use om_server::auth::Auth;
use om_server::db::Db;
use om_server::harness::{HarnessRegistry, MockHarness};
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use om_server::pty::{PtyConfig, PtyManager, PtyStatus};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
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

#[tokio::test]
async fn pty_binary_ws_ring_catchup_viewer_readonly_cast_recording_and_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("work");
    let cast_dir = dir.path().join("pty");
    std::fs::create_dir_all(&work_dir).unwrap();

    let db = Db::open(dir.path().join("pty.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let auth = Auth::new(db.clone());
    let (_owner, owner_tok) = auth
        .bootstrap_owner(&space_id, "Owner", "cli")
        .await
        .unwrap()
        .unwrap();
    let owner_p = auth.authenticate(&owner_tok.secret).await.unwrap();
    let viewer = auth
        .add_member(&owner_p, "Viewer", Role::Viewer)
        .await
        .unwrap();
    let viewer_tok = auth
        .issue_token(&owner_p, &viewer.id, "viewer-term", None)
        .await
        .unwrap();

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::instant()));
    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..Default::default()
        },
    );
    let pty = PtyManager::new(PtyConfig {
        recordings_dir: Some(cast_dir),
        ring_capacity: 64 * 1024,
        ..Default::default()
    });

    let app = build_router_with_extensions(
        AppState {
            orchestrator,
            config: ApiConfig {
                auth_token: "super-secret-server-token".into(),
                default_space_id: space_id,
                static_dir: None,
                allow_cors_origin: None,
            },
        },
        ApiExtensions {
            pty: Some(pty.clone()),
            ..Default::default()
        },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_clone = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_clone).await;
    });

    // 1. Create a session via REST
    let res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            "/api/v1/sessions",
            Some(
                serde_json::to_string(&CreateSessionRequest {
                    config: SessionConfig {
                        harness_id: "mock".into(),
                        cwd: work_dir.to_string_lossy().into_owned(),
                        workspace_id: None,
                        model: None,
                        sandbox: SandboxMode::Direct,
                        title: Some("PTY Phase 3 Test".into()),
                    },
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sess: SessionSummary = serde_json::from_slice(&bytes).unwrap();

    // 2. Owner connects to /ws/pty/{id} -> spawns real PTY shell and receives Ready frame
    let (mut pty_ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws/pty/{}?token={}&cols=90&rows=28",
        sess.id, owner_tok.secret
    ))
    .await
    .unwrap();

    let ready_frame = tokio::time::timeout(Duration::from_secs(5), pty_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Text(ready_txt) = ready_frame else {
        panic!("expected ready text frame, got {ready_frame:?}");
    };
    let ready: Value = serde_json::from_str(&ready_txt).unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["writable"], true);
    assert_eq!(ready["recording"], true);
    assert_eq!(ready["cols"], 90);
    assert_eq!(ready["rows"], 28);

    // 3. Resize + send command over binary frames, verifying output AND cleared server env
    pty_ws
        .send(WsMessage::Text(
            json!({"type": "resize", "cols": 120, "rows": 36})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    pty_ws
        .send(WsMessage::Binary(Bytes::from_static(
            b"echo PTY_OK_999 && printf 'TOK=%s\\n' \"$OM_TOKEN\"\n",
        )))
        .await
        .unwrap();

    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(frame))) =
            tokio::time::timeout(Duration::from_millis(400), pty_ws.next()).await
            && let WsMessage::Binary(chunk) = frame
        {
            collected.push_str(&String::from_utf8_lossy(&chunk));
            if collected.contains("PTY_OK_999") && collected.contains("TOK=") {
                break;
            }
        }
    }
    assert!(
        collected.contains("PTY_OK_999"),
        "missing PTY output, got: {collected:?}"
    );
    assert!(
        !collected.contains("super-secret-server-token"),
        "OM_TOKEN must never leak into PTY env!"
    );

    // 4. Viewer attaches read-only -> immediately receives ring-buffer catch-up snapshot & stdin is rejected
    let (mut viewer_ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws/pty/{}?token={}",
        sess.id, viewer_tok.secret
    ))
    .await
    .unwrap();

    let v_ready = tokio::time::timeout(Duration::from_secs(5), viewer_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Text(v_txt) = v_ready else {
        panic!("expected viewer ready frame");
    };
    let v_json: Value = serde_json::from_str(&v_txt).unwrap();
    assert_eq!(v_json["type"], "ready");
    assert_eq!(v_json["writable"], false);

    let snap_frame = tokio::time::timeout(Duration::from_secs(5), viewer_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Binary(snap) = snap_frame else {
        panic!("expected scrollback snapshot binary frame");
    };
    assert!(
        String::from_utf8_lossy(&snap).contains("PTY_OK_999"),
        "viewer catch-up snapshot should contain previous terminal output"
    );

    // Viewer attempting to write stdin receives a read-only error control frame
    viewer_ws
        .send(WsMessage::Binary(Bytes::from_static(b"echo forbidden\n")))
        .await
        .unwrap();
    let err_frame = tokio::time::timeout(Duration::from_secs(5), viewer_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Text(err_txt) = err_frame else {
        panic!("expected read-only error frame, got {err_frame:?}");
    };
    let err_json: Value = serde_json::from_str(&err_txt).unwrap();
    assert_eq!(err_json["type"], "error");

    // 5. Independent /ws/events still streams smoothly without being blocked by PTY
    let (mut ev_ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws/events?token={}",
        owner_tok.secret
    ))
    .await
    .unwrap();
    ev_ws
        .send(WsMessage::Text(
            serde_json::to_string(&ClientMessage::Hello {
                protocol_version: 1,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _ = ev_ws.next().await; // Welcome
    ev_ws
        .send(WsMessage::Text(
            serde_json::to_string(&ClientMessage::Subscribe {
                session_id: sess.id.clone(),
                last_seq: 0,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _ = ev_ws.next().await; // Subscribed
    loop {
        if let Some(Ok(WsMessage::Text(t))) =
            tokio::time::timeout(Duration::from_secs(5), ev_ws.next())
                .await
                .unwrap()
        {
            let m: ServerMessage = serde_json::from_str(&t).unwrap();
            if let ServerMessage::Event { event } = m
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

    // 6. PTY status & asciicast v2 (.cast) recording endpoint
    let status_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "GET",
            &format!("/api/v1/sessions/{}/pty", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(status_res.status(), StatusCode::OK);
    let status_bytes = axum::body::to_bytes(status_res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let status: PtyStatus = serde_json::from_slice(&status_bytes).unwrap();
    assert!(status.active);
    assert!(status.has_recording);
    assert!(status.recording_bytes > 0);

    let cast_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "GET",
            &format!("/api/v1/sessions/{}/pty/recording", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(cast_res.status(), StatusCode::OK);
    let cast_bytes = axum::body::to_bytes(cast_res.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let cast_str = String::from_utf8(cast_bytes.to_vec()).unwrap();
    let first_line: Value = serde_json::from_str(cast_str.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["version"], 2);
    assert!(cast_str.contains("PTY_OK_999"));

    // 7. DELETE /pty kills the child process cleanly while preserving .cast recording
    let kill_res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "DELETE",
            &format!("/api/v1/sessions/{}/pty", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(kill_res.status(), StatusCode::NO_CONTENT);

    let after_kill = pty.status(&sess.id);
    assert!(!after_kill.active);
    assert!(after_kill.has_recording);

    // 8. Viewer cannot DELETE /pty (403 Forbidden)
    let forbidden_kill = app
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "DELETE",
            &format!("/api/v1/sessions/{}/pty", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(forbidden_kill.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pty_bwrap_session_runs_inside_sandbox_and_isolates_host_files() {
    use om_server::sandbox::BwrapSandbox;
    use om_server::workspace::{WorkspaceConfig, WorkspaceManager};

    let bwrap = match BwrapSandbox::default().probe().await {
        Ok(b) => b,
        Err(e) => {
            if std::env::var("OM_REQUIRE_BWRAP").as_deref() == Ok("1") {
                panic!("OM_REQUIRE_BWRAP=1 set but bwrap probe failed: {e}");
            }
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("work");
    let data_dir = dir.path().join("data");
    let secret_dir = dir.path().join("host-secrets");
    std::fs::create_dir_all(&work_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&secret_dir).unwrap();
    let secret_file = secret_dir.join("secret.txt");
    std::fs::write(&secret_file, "TOP_SECRET_HOST_DATA").unwrap();

    let db = Db::open(dir.path().join("pty-bwrap.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let auth = Auth::new(db.clone());
    let (_owner, owner_tok) = auth
        .bootstrap_owner(&space_id, "Owner", "cli")
        .await
        .unwrap()
        .unwrap();

    let workspaces = WorkspaceManager::new(db.clone(), WorkspaceConfig::new(&data_dir)).unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::instant()));
    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: false,
            ..Default::default()
        },
    )
    .with_workspaces(workspaces)
    .with_bwrap_sandbox(Arc::new(bwrap));
    let pty = PtyManager::new(PtyConfig::default());

    let app = build_router_with_extensions(
        AppState {
            orchestrator,
            config: ApiConfig {
                auth_token: "bwrap-pty-token".into(),
                default_space_id: space_id,
                static_dir: None,
                allow_cors_origin: None,
            },
        },
        ApiExtensions {
            pty: Some(pty.clone()),
            ..Default::default()
        },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_clone = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_clone).await;
    });

    let res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            "/api/v1/sessions",
            Some(
                serde_json::to_string(&CreateSessionRequest {
                    config: SessionConfig {
                        harness_id: "mock".into(),
                        cwd: work_dir.to_string_lossy().into_owned(),
                        workspace_id: None,
                        model: None,
                        sandbox: SandboxMode::Bwrap,
                        title: Some("Bwrap PTY Test".into()),
                    },
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sess: SessionSummary = serde_json::from_slice(&bytes).unwrap();

    let (mut pty_ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws/pty/{}?token={}",
        sess.id, owner_tok.secret
    ))
    .await
    .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), pty_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    // Split marker literals so terminal local-echo does not trigger the assertion early.
    let cmd = format!(
        "cat {} 2>&1 || echo BLOCKED\"_\"SECRET; echo DONE\"_\"BWRAP_CHECK\n",
        secret_file.display()
    );
    pty_ws
        .send(WsMessage::Binary(Bytes::from(cmd.into_bytes())))
        .await
        .unwrap();

    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(frame))) =
            tokio::time::timeout(Duration::from_millis(400), pty_ws.next()).await
            && let WsMessage::Binary(chunk) = frame
        {
            collected.push_str(&String::from_utf8_lossy(&chunk));
            if collected.contains("DONE_BWRAP_CHECK") {
                break;
            }
        }
    }
    assert!(
        collected.contains("BLOCKED_SECRET"),
        "expected sandboxed PTY to be blocked from reading host secret, got: {collected:?}"
    );
    assert!(
        !collected.contains("TOP_SECRET_HOST_DATA"),
        "bwrap PTY must NOT read host files outside work_dir!"
    );
    pty.kill(&sess.id);
}

use std::sync::Arc;
