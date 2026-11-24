//! Integration tests verifying `om-gui-bridge` profile isolation, concurrency quota,
//! idle reaper, message forwarding, and human-takeover (noVNC) link enforcement.

use om_gui_bridge::{
    BridgeConfig, BridgeHealth, CreateGuiSessionRequest, DriverKind, GuiBridge, GuiPromptRequest,
    GuiPromptResponse, GuiSessionInfo,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

async fn post_json<B: Serialize>(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    body: &B,
) -> reqwest::Response {
    let mut req = http
        .post(url)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(body).unwrap());
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    req.send().await.unwrap()
}

async fn read_json<T: DeserializeOwned>(res: reqwest::Response) -> T {
    let bytes = res.bytes().await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn per_member_profile_isolation_concurrency_cap_and_takeover_flow() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = BridgeConfig::new(dir.path().join("profiles"));
    cfg.driver = DriverKind::Simulated;
    cfg.max_sessions = 2;
    cfg.bridge_token = Some("bridge-secret-123".into());

    let (bridge, task) = GuiBridge::start(cfg, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let base = bridge.base_url();
    let http = reqwest::Client::new();
    let tok = Some("bridge-secret-123");

    // 1. Public healthz reports simulated driver & quota
    let health: BridgeHealth =
        read_json(http.get(format!("{base}/healthz")).send().await.unwrap()).await;
    assert_eq!(health.status, "ok");
    assert_eq!(health.driver, "simulated");
    assert_eq!(health.max_sessions, 2);

    // 2. Unauthenticated /v1 request is rejected
    let unauth = http
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 3. Path-traversal member_id is rejected
    let bad = post_json(
        &http,
        &format!("{base}/v1/sessions"),
        tok,
        &CreateGuiSessionRequest {
            om_session_id: "s0".into(),
            member_id: "../etc".into(),
            cwd: None,
        },
    )
    .await;
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // 4. Two different members get distinct 0700 isolated profiles & displays
    let s1: GuiSessionInfo = read_json(
        post_json(
            &http,
            &format!("{base}/v1/sessions"),
            tok,
            &CreateGuiSessionRequest {
                om_session_id: "om-s1".into(),
                member_id: "member-alice".into(),
                cwd: None,
            },
        )
        .await,
    )
    .await;

    let s2: GuiSessionInfo = read_json(
        post_json(
            &http,
            &format!("{base}/v1/sessions"),
            tok,
            &CreateGuiSessionRequest {
                om_session_id: "om-s2".into(),
                member_id: "member-bob".into(),
                cwd: None,
            },
        )
        .await,
    )
    .await;

    assert_ne!(s1.profile_dir, s2.profile_dir);
    assert_ne!(s1.display, s2.display);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&s1.profile_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    // 5. Third concurrent session hits max_sessions cap (429)
    let over = post_json(
        &http,
        &format!("{base}/v1/sessions"),
        tok,
        &CreateGuiSessionRequest {
            om_session_id: "om-s3".into(),
            member_id: "member-carol".into(),
            cwd: None,
        },
    )
    .await;
    assert_eq!(over.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    // 6. Normal prompt forwards message; CAPTCHA/login prompt triggers human takeover
    let normal: GuiPromptResponse = read_json(
        post_json(
            &http,
            &format!("{base}/v1/sessions/{}/prompt", s1.bridge_session_id),
            tok,
            &GuiPromptRequest {
                prompt: "重构登录组件".into(),
            },
        )
        .await,
    )
    .await;
    assert!(!normal.takeover_required);
    assert!(normal.reply.contains("重构登录组件"));

    let gated: GuiPromptResponse = read_json(
        post_json(
            &http,
            &format!("{base}/v1/sessions/{}/prompt", s1.bridge_session_id),
            tok,
            &GuiPromptRequest {
                prompt: "please solve captcha login".into(),
            },
        )
        .await,
    )
    .await;
    assert!(gated.takeover_required);
    assert_eq!(gated.status, "awaiting_human_takeover");
    assert!(
        !gated.reply.contains("?k="),
        "takeover secret token must not be embedded in shared reply text"
    );

    // 7. Takeover URL requires its secret token and serves interactive HTML with security headers
    let unauthorized_takeover = http
        .get(format!("{base}/takeover/{}", s1.bridge_session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthorized_takeover.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let takeover_resp = http.get(&s1.takeover_url).send().await.unwrap();
    assert_eq!(
        takeover_resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        takeover_resp
            .headers()
            .get("referrer-policy")
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    assert_eq!(
        takeover_resp
            .headers()
            .get("x-frame-options")
            .and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    let html = takeover_resp.text().await.unwrap();
    assert!(html.contains("Antigravity GUI 远程人工接管"));
    assert!(html.contains("member-alice"));

    // 8. Delete frees slot
    let del = http
        .delete(format!("{base}/v1/sessions/{}", s2.bridge_session_id))
        .bearer_auth("bridge-secret-123")
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(bridge.active_count(), 1);

    task.abort();
}

#[tokio::test]
async fn idle_reaper_evicts_inactive_gui_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = BridgeConfig::new(dir.path().join("profiles"));
    cfg.driver = DriverKind::Simulated;
    cfg.max_sessions = 2;
    cfg.idle_timeout = Duration::from_millis(120);
    cfg.reap_interval = Duration::from_millis(50);

    let (bridge, task) = GuiBridge::start(cfg, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let base = bridge.base_url();
    let http = reqwest::Client::new();

    let res = post_json(
        &http,
        &format!("{base}/v1/sessions"),
        None,
        &CreateGuiSessionRequest {
            om_session_id: "om-idle".into(),
            member_id: "member-idle".into(),
            cwd: None,
        },
    )
    .await;
    assert_eq!(res.status(), reqwest::StatusCode::CREATED);
    assert_eq!(bridge.active_count(), 1);

    // Wait for background idle reaper to reclaim the session
    let mut reclaimed = false;
    for _ in 0..20 {
        if bridge.active_count() == 0 {
            reclaimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        reclaimed,
        "expected idle reaper to reclaim inactive GUI session"
    );
    task.abort();
}

#[tokio::test]
async fn non_loopback_bind_requires_bridge_token() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = BridgeConfig::new(dir.path().join("profiles"));
    cfg.driver = DriverKind::Simulated;
    cfg.bridge_token = None;

    let res = GuiBridge::start(cfg, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).await;
    assert!(res.is_err(), "non-loopback bind without token must fail closed");
}
