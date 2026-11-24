//! Phase 2 full backend & security integration tests:
//! 1. W2-1 4-role RBAC enforcement matrix (Owner / Admin / Member / Viewer), device tokens & revocation
//! 2. Multi-member concurrent WebSocket real-time sync & attributed Queue & Steer
//! 3. W2-2 Host KeyProxy credential isolation (real key never in sandbox env, injected upstream, stats API)
//! 4. W2-4 PWA Web Push subscription CRUD + SSRF endpoint validation
//! 5. W2-5 External `om-gui-bridge` (`antigravity-gui`) end-to-end message forwarding & noVNC takeover

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::post;
use futures::{SinkExt, StreamExt};
use om_core::api::{
    AddMemberRequest, ChangeRoleRequest, ClientMessage, CreateSessionRequest, IssueTokenRequest,
    ListAuditResponse, ListMembersResponse, ListProvidersResponse, ListQueueResponse,
    ListTokensResponse, ProxyStats, ServerMessage,
};
use om_core::auth::{IssuedToken, MemberRecord, Principal, Role};
use om_core::event::EventPayload;
use om_core::proxy::ProviderKind;
use om_core::sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxMode, SandboxSpec};
use om_core::session::{SessionConfig, SessionStatus, SessionSummary};
use om_gui_bridge::{BridgeConfig, DriverKind, GuiBridge};
use om_server::api::{ApiConfig, ApiExtensions, AppState, build_router_with_extensions};
use om_server::auth::Auth;
use om_server::db::Db;
use om_server::harness::{
    GuiBridgeConfig, GuiBridgeHarness, HarnessRegistry, MockHarness, RemoteScreenRegistry,
};
use om_server::orchestrator::{Orchestrator, OrchestratorConfig};
use om_server::proxy::{KeyProxy, ProviderConfig, ProxyConfig, ProxyEnvSandbox};
use om_server::sandbox::BwrapSandbox;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
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

async fn read_json<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
    let bytes = axum::body::to_bytes(res.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_ws(addr: SocketAddr, token: &str) -> WsStream {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/events?token={token}"))
            .await
            .unwrap();
    ws.send(WsMessage::Text(
        serde_json::to_string(&ClientMessage::Hello {
            protocol_version: 1,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Text(txt) = frame else {
        panic!("expected welcome text frame");
    };
    let msg: ServerMessage = serde_json::from_str(&txt).unwrap();
    assert!(matches!(msg, ServerMessage::Welcome { .. }));
    ws
}

async fn ws_recv(ws: &mut WsStream) -> ServerMessage {
    let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("ws timeout")
        .expect("ws closed")
        .expect("ws error");
    let WsMessage::Text(txt) = frame else {
        panic!("expected text frame");
    };
    serde_json::from_str(&txt).unwrap()
}

#[tokio::test]
async fn rbac_four_role_matrix_device_tokens_and_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("rbac.db")).unwrap();
    let space_id = db.ensure_space("team").await.unwrap();
    let auth = Auth::new(db.clone());
    let (owner, owner_tok) = auth
        .bootstrap_owner(&space_id, "Alice Owner", "owner-laptop")
        .await
        .unwrap()
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

    // Pure device-token mode: auth_token is empty!
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
        ApiExtensions::default(),
    );

    // 1. Owner calls GET /api/v1/me
    let res = app
        .clone()
        .oneshot(bearer_req(&owner_tok.secret, "GET", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let me: Principal = read_json(res).await;
    assert_eq!(me.member_id, owner.id);
    assert_eq!(me.role, Role::Owner);

    // 2. Owner adds Admin, Member, and Viewer, and issues device tokens for each
    let add_and_issue = |name: &'static str, role: Role| {
        let app = app.clone();
        let owner_secret = owner_tok.secret.clone();
        async move {
            let res = app
                .clone()
                .oneshot(bearer_req(
                    &owner_secret,
                    "POST",
                    "/api/v1/members",
                    Some(
                        serde_json::to_string(&AddMemberRequest {
                            display_name: name.into(),
                            role,
                        })
                        .unwrap(),
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::CREATED);
            let m: MemberRecord = read_json(res).await;

            let res = app
                .oneshot(bearer_req(
                    &owner_secret,
                    "POST",
                    &format!("/api/v1/members/{}/tokens", m.id),
                    Some(
                        serde_json::to_string(&IssueTokenRequest {
                            label: format!("{name}-dev"),
                            ttl_secs: Some(3600),
                        })
                        .unwrap(),
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::CREATED);
            let tok: IssuedToken = read_json(res).await;
            (m, tok)
        }
    };

    let (admin, admin_tok) = add_and_issue("Bob Admin", Role::Admin).await;
    let (member, member_tok) = add_and_issue("Carol Member", Role::Member).await;
    let (viewer, viewer_tok) = add_and_issue("Dave Viewer", Role::Viewer).await;

    // 3. Viewer: can read /sessions & /members, forbidden from creating sessions, members, or viewing audit
    let res = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            "/api/v1/members",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let listed: ListMembersResponse = read_json(res).await;
    assert_eq!(listed.members.len(), 4);

    let create_session_body = serde_json::to_string(&CreateSessionRequest {
        config: SessionConfig {
            harness_id: "mock".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            workspace_id: None,
            model: None,
            sandbox: SandboxMode::Direct,
            title: Some("RBAC Test".into()),
        },
    })
    .unwrap();

    let viewer_create = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "POST",
            "/api/v1/sessions",
            Some(create_session_body.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(viewer_create.status(), StatusCode::FORBIDDEN);

    let viewer_audit = app
        .clone()
        .oneshot(bearer_req(&viewer_tok.secret, "GET", "/api/v1/audit", None))
        .await
        .unwrap();
    assert_eq!(viewer_audit.status(), StatusCode::FORBIDDEN);

    // 4. Member: can create session, list own tokens, but forbidden from adding members or reading audit
    let member_create = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "POST",
            "/api/v1/sessions",
            Some(create_session_body),
        ))
        .await
        .unwrap();
    assert_eq!(member_create.status(), StatusCode::CREATED);
    let sess: SessionSummary = read_json(member_create).await;
    assert!(!sess.id.as_str().is_empty());

    let member_own_tokens = app
        .clone()
        .oneshot(bearer_req(
            &member_tok.secret,
            "GET",
            &format!("/api/v1/members/{}/tokens", member.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(member_own_tokens.status(), StatusCode::OK);
    let own_toks: ListTokensResponse = read_json(member_own_tokens).await;
    assert_eq!(own_toks.tokens.len(), 1);

    let member_audit = app
        .clone()
        .oneshot(bearer_req(&member_tok.secret, "GET", "/api/v1/audit", None))
        .await
        .unwrap();
    assert_eq!(member_audit.status(), StatusCode::FORBIDDEN);

    // Viewer can read the session created by Member, but cannot send prompts
    let viewer_get = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "GET",
            &format!("/api/v1/sessions/{}", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(viewer_get.status(), StatusCode::OK);

    let viewer_prompt = app
        .clone()
        .oneshot(bearer_req(
            &viewer_tok.secret,
            "POST",
            &format!("/api/v1/sessions/{}/prompts", sess.id),
            Some(json!({"prompt": {"content": [{"type": "text", "text": "hi"}]}}).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(viewer_prompt.status(), StatusCode::FORBIDDEN);

    // 5. Admin: can read audit & change Viewer -> Member, but cannot promote to Admin
    let admin_audit = app
        .clone()
        .oneshot(bearer_req(&admin_tok.secret, "GET", "/api/v1/audit", None))
        .await
        .unwrap();
    assert_eq!(admin_audit.status(), StatusCode::OK);
    let audit: ListAuditResponse = read_json(admin_audit).await;
    assert!(!audit.entries.is_empty());

    let admin_promote_viewer = app
        .clone()
        .oneshot(bearer_req(
            &admin_tok.secret,
            "PATCH",
            &format!("/api/v1/members/{}", viewer.id),
            Some(serde_json::to_string(&ChangeRoleRequest { role: Role::Member }).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(admin_promote_viewer.status(), StatusCode::OK);

    let admin_escalate = app
        .clone()
        .oneshot(bearer_req(
            &admin_tok.secret,
            "PATCH",
            &format!("/api/v1/members/{}", viewer.id),
            Some(serde_json::to_string(&ChangeRoleRequest { role: Role::Admin }).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(admin_escalate.status(), StatusCode::FORBIDDEN);

    // 6. Revoking a token immediately invalidates it (401)
    let revoke = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "DELETE",
            &format!("/api/v1/tokens/{}", viewer_tok.token.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::OK);

    let after_revoke = app
        .clone()
        .oneshot(bearer_req(&viewer_tok.secret, "GET", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);

    // 7. Disabling Admin immediately revokes Admin's token
    let disable = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "DELETE",
            &format!("/api/v1/members/{}", admin.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(disable.status(), StatusCode::OK);

    let after_disable = app
        .oneshot(bearer_req(&admin_tok.secret, "GET", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(after_disable.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn multi_member_concurrent_ws_sync_and_queue_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("collab.db")).unwrap();
    let space_id = db.ensure_space("collab").await.unwrap();
    let auth = Auth::new(db.clone());
    let (owner, owner_tok) = auth
        .bootstrap_owner(&space_id, "Owner", "laptop")
        .await
        .unwrap()
        .unwrap();
    let owner_p = auth.authenticate(&owner_tok.secret).await.unwrap();
    let teammate = auth
        .add_member(&owner_p, "Teammate", Role::Member)
        .await
        .unwrap();
    let teammate_tok = auth
        .issue_token(&owner_p, &teammate.id, "phone", None)
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
        ApiExtensions::default(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_clone = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_clone).await;
    });

    // Owner creates session
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
                        cwd: dir.path().to_string_lossy().into_owned(),
                        workspace_id: None,
                        model: None,
                        sandbox: SandboxMode::Direct,
                        title: Some("Shared Session".into()),
                    },
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    let sess: SessionSummary = read_json(res).await;

    // Both Owner and Teammate subscribe concurrently to `/ws/events`
    let mut ws_owner = connect_ws(addr, &owner_tok.secret).await;
    let mut ws_teammate = connect_ws(addr, &teammate_tok.secret).await;
    let sub_frame = serde_json::to_string(&ClientMessage::Subscribe {
        session_id: sess.id.clone(),
        last_seq: 0,
    })
    .unwrap();
    ws_owner
        .send(WsMessage::Text(sub_frame.clone().into()))
        .await
        .unwrap();
    ws_teammate
        .send(WsMessage::Text(sub_frame.into()))
        .await
        .unwrap();
    assert!(matches!(
        ws_recv(&mut ws_owner).await,
        ServerMessage::Subscribed { .. }
    ));
    assert!(matches!(
        ws_recv(&mut ws_teammate).await,
        ServerMessage::Subscribed { .. }
    ));

    // Wait for Idle on both sockets
    for ws in [&mut ws_owner, &mut ws_teammate] {
        loop {
            if let ServerMessage::Event { event } = ws_recv(ws).await
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

    // Owner triggers approval-gated turn so session stays busy deterministically
    let res = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            &format!("/api/v1/sessions/{}/prompts", sess.id),
            Some(
                json!({
                    "prompt": {"content": [{"type": "text", "text": "please approve deploy"}]},
                    "mode": "queue"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    // Both sockets see the exact same ApprovalRequested event seq
    let owner_approval_seq = loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws_owner).await
            && matches!(event.payload, EventPayload::ApprovalRequested { .. })
        {
            break event.seq;
        }
    };
    let teammate_approval_seq = loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws_teammate).await
            && matches!(event.payload, EventPayload::ApprovalRequested { .. })
        {
            break event.seq;
        }
    };
    assert_eq!(owner_approval_seq, teammate_approval_seq);

    // Both Owner and Teammate enqueue follow-up prompts; queue preserves member attribution!
    let r1 = app
        .clone()
        .oneshot(bearer_req(
            &owner_tok.secret,
            "POST",
            &format!("/api/v1/sessions/{}/prompts", sess.id),
            Some(
                json!({
                    "prompt": {"content": [{"type": "text", "text": "from owner"}]},
                    "mode": "queue"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::ACCEPTED);

    let r2 = app
        .clone()
        .oneshot(bearer_req(
            &teammate_tok.secret,
            "POST",
            &format!("/api/v1/sessions/{}/prompts", sess.id),
            Some(
                json!({
                    "prompt": {"content": [{"type": "text", "text": "from teammate"}]},
                    "mode": "queue"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::ACCEPTED);

    let q_res = app
        .oneshot(bearer_req(
            &teammate_tok.secret,
            "GET",
            &format!("/api/v1/sessions/{}/queue", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(q_res.status(), StatusCode::OK);
    let queue: ListQueueResponse = read_json(q_res).await;
    assert_eq!(queue.items.len(), 2);
    assert_eq!(queue.items[0].member_id.as_ref(), Some(&owner.id));
    assert_eq!(queue.items[1].member_id.as_ref(), Some(&teammate.id));
}

#[tokio::test]
async fn key_proxy_isolates_real_api_key_from_sandbox_and_reports_stats() {
    const REAL_KEY: &str = "sk-ant-real-secret-key-never-in-sandbox";
    let received_key: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Mock upstream Anthropic server verifying it receives REAL_KEY
    let upstream_app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(slot): State<Arc<Mutex<Option<String>>>>, headers: HeaderMap| async move {
                    let key = headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(ToOwned::to_owned);
                    *slot.lock().unwrap() = key;
                    (StatusCode::OK, r#"{"id":"msg_ok"}"#)
                },
            ),
        )
        .with_state(received_key.clone());
    let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(up_listener, upstream_app).await;
    });

    let proxy_cfg = ProxyConfig {
        providers: vec![ProviderConfig::new(
            "anthropic",
            ProviderKind::Anthropic,
            format!("http://{up_addr}"),
            REAL_KEY,
        )],
        use_system_proxy: false,
        ..Default::default()
    };
    let (proxy, proxy_task) = KeyProxy::start(
        proxy_cfg,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
    )
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("proxy.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::instant()));
    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..Default::default()
        },
    )
    .with_key_proxy(proxy.clone());

    let app = build_router_with_extensions(
        AppState {
            orchestrator: orchestrator.clone(),
            config: ApiConfig {
                auth_token: "proxy-test-token".into(),
                default_space_id: space_id.clone(),
                static_dir: None,
                allow_cors_origin: None,
            },
        },
        ApiExtensions::default(),
    );

    // 1. GET /api/v1/providers lists anthropic without any key material
    let res = app
        .clone()
        .oneshot(bearer_req(
            "proxy-test-token",
            "GET",
            "/api/v1/providers",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let providers: ListProvidersResponse = read_json(res).await;
    assert_eq!(providers.providers.len(), 1);
    assert_eq!(providers.providers[0].id, "anthropic");

    // 2. Create a session and run a real child process under ProxyEnvSandbox (using bwrap if available)
    let created = orchestrator
        .create_session(
            space_id,
            SessionConfig {
                harness_id: "mock".into(),
                cwd: dir.path().to_string_lossy().into_owned(),
                workspace_id: None,
                model: None,
                sandbox: SandboxMode::Direct,
                title: Some("Proxy Isolation".into()),
            },
            None,
        )
        .await
        .unwrap();

    let grant = proxy.grant(&created.id).unwrap();
    let base_sandbox: Arc<dyn Sandbox> = match BwrapSandbox::default().probe().await {
        Ok(bwrap) => Arc::new(bwrap),
        Err(_) => Arc::new(DirectSandbox),
    };
    let sandbox = ProxyEnvSandbox::new(base_sandbox, &grant);

    let script = r#"
import os, urllib.request
env_dump = "\n".join(f"{k}={v}" for k, v in os.environ.items())
assert "sk-ant-real-secret-key-never-in-sandbox" not in env_dump, "LEAKED REAL KEY!"
assert os.environ["ANTHROPIC_API_KEY"].startswith("ost_"), "EXPECTED ost_ SESSION TOKEN!"
req = urllib.request.Request(
    os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages",
    data=b"{}",
    headers={"x-api-key": os.environ["ANTHROPIC_API_KEY"], "content-type": "application/json"},
    method="POST",
)
with urllib.request.urlopen(req, timeout=5) as resp:
    print(resp.read().decode())
"#;

    let mut cmd_spec = CommandSpec::new("python3");
    cmd_spec.args = vec!["-c".into(), script.into()];
    cmd_spec.clear_env = true;
    cmd_spec.env.insert("PATH".into(), "/usr/bin:/bin".into());

    let sb_spec = SandboxSpec {
        writable_paths: vec![dir.path().to_path_buf()],
        network: true,
        ..Default::default()
    };
    let wrapped = sandbox.wrap(&sb_spec, cmd_spec).unwrap();
    let mut cmd = tokio::process::Command::new(&wrapped.program);
    cmd.args(&wrapped.args);
    if wrapped.clear_env {
        cmd.env_clear();
    }
    cmd.envs(&wrapped.env);
    let out = cmd.output().await.unwrap();
    assert!(
        out.status.success(),
        "sandboxed proxy call failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("msg_ok"));
    assert_eq!(received_key.lock().unwrap().as_deref(), Some(REAL_KEY));

    // 3. Session proxy stats API reflects the forwarded request
    let stats_res = app
        .oneshot(bearer_req(
            "proxy-test-token",
            "GET",
            &format!("/api/v1/sessions/{}/proxy/stats", created.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(stats_res.status(), StatusCode::OK);
    let stats: ProxyStats = read_json(stats_res).await;
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.upstream_errors, 0);

    proxy_task.abort();
}

#[tokio::test]
async fn pwa_web_push_subscriptions_and_ssrf_validation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("push.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
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

    let app = build_router_with_extensions(
        AppState {
            orchestrator,
            config: ApiConfig {
                auth_token: "push-token".into(),
                default_space_id: space_id,
                static_dir: None,
                allow_cors_origin: None,
            },
        },
        ApiExtensions {
            push_store_path: Some(dir.path().join("push.json")),
            vapid_public_key: Some("BEl62iUYgUivxIkv69yViEuiBIa-Ib9-SkvMeAtA3LFg".into()),
            allow_loopback_push: false,
            ..Default::default()
        },
    );

    // 1. SSRF / non-HTTPS / IPv6 / IPv4-mapped endpoints are rejected
    for bad_endpoint in [
        "http://169.254.169.254/latest/meta-data",
        "http://10.0.0.5/push",
        "http://example.com/push",
        "https://user:pass@fcm.googleapis.com/fcm/send/abc",
        "https://[::1]/push",
        "https://[::ffff:169.254.169.254]/latest/meta-data",
        "https://[fe80::1]/push",
        "https://[fc00::1]/push",
        "https://100.64.0.1/push",
        "https://localhost./push",
        "https://sub.localhost/push",
    ] {
        let res = app
            .clone()
            .oneshot(bearer_req(
                "push-token",
                "POST",
                "/api/v1/push/subscriptions",
                Some(
                    json!({
                        "endpoint": bad_endpoint,
                        "keys": {"p256dh": "k1", "auth": "a1"}
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {bad_endpoint}"
        );
    }

    // 2. Valid HTTPS Web Push subscription succeeds, persists, tests, and deletes
    let res = app
        .clone()
        .oneshot(bearer_req(
            "push-token",
            "POST",
            "/api/v1/push/subscriptions",
            Some(
                json!({
                    "endpoint": "https://fcm.googleapis.com/fcm/send/device-1",
                    "keys": {"p256dh": "BNcRd...", "auth": "tBHI..."},
                    "user_agent": "Mobile PWA"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let sub: Value = read_json(res).await;
    let sub_id = sub["id"].as_str().unwrap().to_string();

    let cfg_res = app
        .clone()
        .oneshot(bearer_req("push-token", "GET", "/api/v1/push/config", None))
        .await
        .unwrap();
    let cfg: Value = read_json(cfg_res).await;
    assert_eq!(cfg["subscription_count"], 1);
    assert!(cfg["vapid_public_key"].as_str().is_some());

    let test_res = app
        .clone()
        .oneshot(bearer_req("push-token", "POST", "/api/v1/push/test", None))
        .await
        .unwrap();
    let tested: Value = read_json(test_res).await;
    assert_eq!(tested["delivered"], 0);
    assert_eq!(tested["registered"], 1);

    let del = app
        .oneshot(bearer_req(
            "push-token",
            "DELETE",
            &format!("/api/v1/push/subscriptions/{sub_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn antigravity_gui_bridge_end_to_end_forwarding_and_remote_screen_takeover() {
    let dir = tempfile::tempdir().unwrap();
    let mut bridge_cfg = BridgeConfig::new(dir.path().join("gui-profiles"));
    bridge_cfg.driver = DriverKind::Simulated;
    bridge_cfg.max_sessions = 2;
    bridge_cfg.bridge_token = Some("gui-secret".into());

    let (bridge, bridge_task) = GuiBridge::start(
        bridge_cfg,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
    )
    .await
    .unwrap();

    let db = Db::open(dir.path().join("gui.db")).unwrap();
    let space_id = db.ensure_space("default").await.unwrap();
    let screens = RemoteScreenRegistry::new();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(GuiBridgeHarness::new(
        GuiBridgeConfig::new(bridge.base_url(), Some("gui-secret".into())),
        screens.clone(),
    )));

    let orchestrator = Orchestrator::new(
        db,
        registry,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..Default::default()
        },
    );
    let app = build_router_with_extensions(
        AppState {
            orchestrator,
            config: ApiConfig {
                auth_token: "gui-owner-token".into(),
                default_space_id: space_id,
                static_dir: None,
                allow_cors_origin: None,
            },
        },
        ApiExtensions {
            remote_screens: screens,
            ..Default::default()
        },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_clone = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_clone).await;
    });

    // 1. Harness list advertises remote_screen=true, structured_tools=false
    let res = app
        .clone()
        .oneshot(bearer_req(
            "gui-owner-token",
            "GET",
            "/api/v1/harnesses",
            None,
        ))
        .await
        .unwrap();
    let harnesses: Value = read_json(res).await;
    let gui_h = harnesses["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == "antigravity-gui")
        .expect("antigravity-gui registered");
    assert_eq!(gui_h["capabilities"]["remote_screen"], true);
    assert_eq!(gui_h["capabilities"]["structured_tools"], false);

    // 2. Create session on `antigravity-gui`
    let res = app
        .clone()
        .oneshot(bearer_req(
            "gui-owner-token",
            "POST",
            "/api/v1/sessions",
            Some(
                serde_json::to_string(&CreateSessionRequest {
                    config: SessionConfig {
                        harness_id: "antigravity-gui".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        workspace_id: None,
                        model: None,
                        sandbox: SandboxMode::Direct,
                        title: Some("Antigravity GUI Session".into()),
                    },
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let sess: SessionSummary = read_json(res).await;

    let mut ws = connect_ws(addr, "gui-owner-token").await;
    ws.send(WsMessage::Text(
        serde_json::to_string(&ClientMessage::Subscribe {
            session_id: sess.id.clone(),
            last_seq: 0,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let _ = ws_recv(&mut ws).await;
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

    // 3. Remote screen endpoint returns isolated takeover URL & display
    let rs_res = app
        .clone()
        .oneshot(bearer_req(
            "gui-owner-token",
            "GET",
            &format!("/api/v1/sessions/{}/remote-screen", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rs_res.status(), StatusCode::OK);
    let rs: Value = read_json(rs_res).await;
    assert_eq!(rs["available"], true);
    let takeover_url = rs["screen"]["takeover_url"].as_str().unwrap().to_string();

    // 4. Send prompt -> bridge forwards reply back to session stream
    let res = app
        .clone()
        .oneshot(bearer_req(
            "gui-owner-token",
            "POST",
            &format!("/api/v1/sessions/{}/prompts", sess.id),
            Some(
                json!({"prompt": {"content": [{"type": "text", "text": "生成首页卡片"}]}})
                    .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let mut saw_reply = false;
    loop {
        if let ServerMessage::Event { event } = ws_recv(&mut ws).await {
            if let EventPayload::AgentMessageChunk {
                content: om_core::content::ContentBlock::Text { text },
                ..
            } = &event.payload
                && text.contains("生成首页卡片")
            {
                saw_reply = true;
            }
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
    assert!(saw_reply);

    // 5. Takeover URL serves live HTML viewer
    let html = reqwest::Client::new()
        .get(&takeover_url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Antigravity GUI 远程人工接管"));

    // 6. Closing session cleans up bridge slot
    let close_res = app
        .oneshot(bearer_req(
            "gui-owner-token",
            "DELETE",
            &format!("/api/v1/sessions/{}", sess.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(close_res.status(), StatusCode::NO_CONTENT);

    for _ in 0..30 {
        if bridge.active_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(bridge.active_count(), 0);

    bridge_task.abort();
}
