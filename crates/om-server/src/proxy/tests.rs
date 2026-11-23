use super::*;
use axum::routing::{any as any_route, get};
use std::sync::Mutex as StdMutex;

const REAL_ANTHROPIC: &str = "sk-ant-REAL-KEY";
const REAL_DEEPSEEK: &str = "sk-ds-REAL-KEY";

/// (method, uri, headers, body)
type Recorded = (String, String, HeaderMap, Vec<u8>);

#[derive(Default, Clone)]
struct Seen {
    last: Arc<StdMutex<Option<Recorded>>>,
}

/// A fake upstream that records the last request and answers with a few routes.
async fn upstream() -> (String, Seen) {
    let seen = Seen::default();
    let record = seen.clone();
    let app = Router::new()
        .route(
            "/stream",
            get(|| async {
                let chunks = futures::stream::iter(
                    ["data: one\n\n", "data: two\n\n", "data: [DONE]\n\n"]
                        .into_iter()
                        .map(|c| Ok::<_, std::io::Error>(bytes::Bytes::from(c))),
                );
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(chunks),
                )
            }),
        )
        .route(
            "/redirect",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "http://127.0.0.1:9/steal")],
                )
            }),
        )
        .fallback(any_route(move |req: Request| {
            let record = record.clone();
            async move {
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let headers = req.headers().clone();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec();
                *record.last.lock().unwrap() = Some((method, uri, headers, body));
                (
                    StatusCode::CREATED,
                    [("set-cookie", "a=b"), ("x-upstream", "yes")],
                    "ok",
                )
            }
        }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

struct Fx {
    proxy: KeyProxy,
    seen: Seen,
    http: reqwest::Client,
    _task: JoinHandle<()>,
}

async fn setup_with(ttl: Duration, max_body: usize) -> Fx {
    let (up, seen) = upstream().await;
    let config = ProxyConfig {
        providers: vec![
            ProviderConfig::new("anthropic", ProviderKind::Anthropic, &up, REAL_ANTHROPIC),
            ProviderConfig::new(
                "deepseek",
                ProviderKind::OpenAi,
                format!("{up}/v1"),
                REAL_DEEPSEEK,
            ),
        ],
        token_ttl: ttl,
        max_body_bytes: max_body,
        use_system_proxy: false,
        ..ProxyConfig::default()
    };
    let (proxy, task) = KeyProxy::start(config, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    Fx {
        proxy,
        seen,
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        _task: task,
    }
}

async fn setup() -> Fx {
    setup_with(Duration::from_secs(60), 1024 * 1024).await
}

impl Fx {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.proxy.addr())
    }

    fn last(&self) -> Recorded {
        self.seen
            .last
            .lock()
            .unwrap()
            .clone()
            .expect("upstream saw a request")
    }
}

#[test]
fn config_is_validated() {
    let ok = |p: Vec<ProviderConfig>| {
        validate(&ProxyConfig {
            providers: p,
            ..ProxyConfig::default()
        })
    };
    let a = || ProviderConfig::new("a", ProviderKind::Anthropic, "https://x.test", "k");
    assert!(ok(vec![a()]).is_ok());
    assert!(
        ok(vec![ProviderConfig::new(
            "A/b",
            ProviderKind::Anthropic,
            "https://x",
            "k"
        )])
        .is_err()
    );
    assert!(
        ok(vec![ProviderConfig::new(
            "a",
            ProviderKind::Anthropic,
            "ftp://x",
            "k"
        )])
        .is_err()
    );
    assert!(
        ok(vec![ProviderConfig::new(
            "a",
            ProviderKind::Anthropic,
            "https://x?q=1",
            "k"
        )])
        .is_err()
    );
    assert!(
        ok(vec![ProviderConfig::new(
            "a",
            ProviderKind::Anthropic,
            "https://x",
            " "
        )])
        .is_err()
    );
    // Two OpenAI-kind providers need distinct env names.
    let o = |id: &str| ProviderConfig::new(id, ProviderKind::OpenAi, "https://x", "k");
    assert!(ok(vec![o("a"), o("b")]).is_err());
    assert!(ok(vec![o("a"), o("b").with_env("B_BASE_URL", "B_API_KEY")]).is_ok());
    assert!(
        ok(vec![a(), a().with_env("X", "Y")]).is_err(),
        "duplicate id"
    );
}

#[test]
fn keys_never_appear_in_debug_output() {
    let p = ProviderConfig::new("a", ProviderKind::Anthropic, "https://x", "sk-secret");
    assert!(!format!("{p:?}").contains("sk-secret"));
}

#[test]
fn unsafe_paths_are_rejected() {
    for ok in ["", "v1/messages", "v1/chat/completions", "a.b/c..d"] {
        assert!(safe_rest(ok), "{ok}");
    }
    for bad in [
        "..",
        "v1/../x",
        "v1/%2e%2e/x",
        "v1/%2E./x",
        "/abs",
        "a%2fb",
        "a\\b",
        "./x",
        ".%09.",
        "v1/.%0a./x",
        "v1/a%3fb",
        "v1/a%23b",
        "v1/a b",
        "v1/a\tb",
        "v1/%zz",
        "v1/%2",
    ] {
        assert!(!safe_rest(bad), "{bad}");
    }
}

#[tokio::test]
async fn refuses_non_loopback_bind() {
    let r = KeyProxy::start(ProxyConfig::default(), "0.0.0.0:0".parse().unwrap()).await;
    assert!(matches!(r, Err(CoreError::InvalidInput(_))));
}

#[tokio::test]
async fn grant_env_points_at_the_proxy_and_holds_no_real_key() {
    let fx = setup().await;
    let g = fx.proxy.grant(&SessionId::new()).unwrap();
    assert!(g.token.starts_with("ost_") && g.token.len() == 68);
    let base = format!("http://{}", fx.proxy.addr());
    assert_eq!(g.env["ANTHROPIC_BASE_URL"], format!("{base}/p/anthropic"));
    assert_eq!(g.env["OPENAI_BASE_URL"], format!("{base}/p/deepseek"));
    assert_eq!(g.env["ANTHROPIC_API_KEY"], g.token);
    assert_eq!(g.env["OPENAI_API_KEY"], g.token);
    let all = format!("{:?}", g.env);
    assert!(!all.contains(REAL_ANTHROPIC) && !all.contains(REAL_DEEPSEEK));
    assert!(!format!("{g:?}").contains(&g.token));
    assert_eq!(fx.proxy.providers().len(), 2);
}

#[tokio::test]
async fn anthropic_requests_get_the_real_key_and_lose_client_credentials() {
    let fx = setup().await;
    let sid = SessionId::new();
    let g = fx.proxy.grant(&sid).unwrap();
    let resp = fx
        .http
        .post(fx.url("/p/anthropic/v1/messages?beta=true"))
        .header("x-api-key", &g.token)
        .header("anthropic-version", "2023-06-01")
        .header("cookie", "session=1")
        .header("x-forwarded-for", "1.2.3.4")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.headers()["x-upstream"], "yes");
    assert!(resp.headers().get("set-cookie").is_none());
    assert_eq!(resp.text().await.unwrap(), "ok");

    let (method, uri, headers, body) = fx.last();
    assert_eq!(method, "POST");
    assert_eq!(uri, "/v1/messages?beta=true");
    assert_eq!(headers["x-api-key"], REAL_ANTHROPIC);
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("cookie").is_none());
    assert!(headers.get("x-forwarded-for").is_none());
    assert_eq!(body, br#"{"model":"m"}"#);
    assert_eq!(fx.proxy.stats(&sid).unwrap().requests, 1);
}

#[tokio::test]
async fn openai_requests_get_a_bearer_key_under_the_base_path() {
    let fx = setup().await;
    let g = fx.proxy.grant(&SessionId::new()).unwrap();
    // An agent that also sends its own x-api-key still authenticates with the session
    // token, and its own key never reaches upstream.
    let resp = fx
        .http
        .post(fx.url("/p/deepseek/chat/completions"))
        .bearer_auth(&g.token)
        .header("x-api-key", "sk-agent-supplied")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let (_, uri, headers, _) = fx.last();
    assert_eq!(uri, "/v1/chat/completions");
    assert_eq!(headers["authorization"], format!("Bearer {REAL_DEEPSEEK}"));
    assert!(headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn bad_tokens_unknown_providers_and_escapes_are_refused() {
    let fx = setup().await;
    let g = fx.proxy.grant(&SessionId::new()).unwrap();
    let status = |r: reqwest::Response| r.status().as_u16();

    let r = fx
        .http
        .get(fx.url("/p/anthropic/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(status(r), 401);
    let r = fx
        .http
        .get(fx.url("/p/anthropic/v1/models"))
        .header("x-api-key", REAL_ANTHROPIC)
        .send()
        .await
        .unwrap();
    assert_eq!(status(r), 401, "a real key is not a session token");
    let r = fx
        .http
        .get(fx.url("/p/nope/v1/models"))
        .header("x-api-key", &g.token)
        .send()
        .await
        .unwrap();
    assert_eq!(status(r), 404);
    let r = fx
        .http
        .get(fx.url("/p/deepseek/v1%2f..%2fadmin"))
        .bearer_auth(&g.token)
        .send()
        .await
        .unwrap();
    assert_eq!(status(r), 400);
    // The router decodes `%64` to `d`; the raw path no longer starts with the
    // provider, so the request is refused instead of using the decoded rest.
    for path in [
        "/p/%64eepseek/.%09./admin",
        "/p/deepseek/.%09./admin",
        "/p/deepseek/v1%3fx=1",
        "/p/deepseek/v1%23frag",
    ] {
        let r = fx
            .http
            .get(fx.url(path))
            .bearer_auth(&g.token)
            .send()
            .await
            .unwrap();
        assert_eq!(status(r), 400, "{path}");
    }
    let r = fx
        .http
        .request(reqwest::Method::TRACE, fx.url("/p/deepseek/x"))
        .bearer_auth(&g.token)
        .send()
        .await
        .unwrap();
    assert_eq!(status(r), 405);
    let r = fx.http.get(fx.url("/elsewhere")).send().await.unwrap();
    assert_eq!(status(r), 404);
    assert!(
        fx.seen.last.lock().unwrap().is_none(),
        "nothing reached upstream"
    );
}

#[tokio::test]
async fn tokens_die_with_revocation_regrant_and_ttl() {
    let fx = setup_with(Duration::from_millis(100), 1024).await;
    let sid = SessionId::new();
    let call = |token: String| {
        let req = fx
            .http
            .get(fx.url("/p/anthropic/v1/models"))
            .header("x-api-key", token);
        async move { req.send().await.unwrap().status().as_u16() }
    };

    let first = fx.proxy.grant(&sid).unwrap();
    assert_eq!(call(first.token.clone()).await, 201);
    let second = fx.proxy.grant(&sid).unwrap();
    assert_eq!(
        call(first.token.clone()).await,
        401,
        "regrant revokes the old token"
    );
    assert_eq!(call(second.token.clone()).await, 201);
    fx.proxy.revoke_session(&sid);
    assert_eq!(call(second.token.clone()).await, 401);
    assert!(fx.proxy.stats(&sid).is_none());

    let third = fx.proxy.grant(&sid).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(call(third.token).await, 401, "expired");
}

#[tokio::test]
async fn streams_pass_through_and_redirects_are_not_followed() {
    let fx = setup().await;
    let g = fx.proxy.grant(&SessionId::new()).unwrap();
    let r = fx
        .http
        .get(fx.url("/p/anthropic/stream"))
        .header("x-api-key", &g.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(
        r.text().await.unwrap(),
        "data: one\n\ndata: two\n\ndata: [DONE]\n\n"
    );

    let r = fx
        .http
        .get(fx.url("/p/anthropic/redirect"))
        .header("x-api-key", &g.token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        302,
        "the redirect is returned, not followed with the key"
    );
}

#[tokio::test]
async fn oversized_bodies_and_dead_upstreams_fail_cleanly() {
    let (proxy, _task) = KeyProxy::start(
        ProxyConfig {
            providers: vec![ProviderConfig::new(
                "dead",
                ProviderKind::OpenAi,
                "http://127.0.0.1:9",
                "k",
            )],
            max_body_bytes: 16,
            use_system_proxy: false,
            ..ProxyConfig::default()
        },
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let sid = SessionId::new();
    let g = proxy.grant(&sid).unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{}/p/dead/x", proxy.addr());
    let r = http
        .post(&url)
        .bearer_auth(&g.token)
        .body(vec![b'a'; 64])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let r = http.post(&url).bearer_auth(&g.token).send().await.unwrap();
    assert_eq!(r.status(), 502);
    let body = r.text().await.unwrap();
    assert!(!body.contains("Bearer k"));
    assert_eq!(proxy.stats(&sid).unwrap().upstream_errors, 1);
}

#[test]
fn proxy_env_sandbox_sets_env_and_network() {
    struct Capture;
    impl Sandbox for Capture {
        fn mode(&self) -> SandboxMode {
            SandboxMode::Bwrap
        }
        fn wrap(&self, spec: &SandboxSpec, mut cmd: CommandSpec) -> CoreResult<CommandSpec> {
            cmd.args.push(format!("network={}", spec.network));
            Ok(cmd)
        }
    }
    let grant = SessionGrant {
        token: "ost_x".into(),
        env: BTreeMap::from([("ANTHROPIC_API_KEY".to_string(), "ost_x".to_string())]),
    };
    let sb = ProxyEnvSandbox::new(Arc::new(Capture), &grant);
    let out = sb
        .wrap(
            &SandboxSpec::default(),
            CommandSpec::new("agent").env("ANTHROPIC_API_KEY", "sk-from-harness"),
        )
        .unwrap();
    assert_eq!(out.env["ANTHROPIC_API_KEY"], "ost_x");
    assert_eq!(out.args, ["network=true"]);
    assert_eq!(sb.mode(), SandboxMode::Bwrap);
}

#[test]
fn upstream_url_stays_under_the_base_path() {
    let base = reqwest::Url::parse("https://api.example.com/v1").unwrap();
    let u = upstream_url(&base, "chat/completions", Some("a=1")).unwrap();
    assert_eq!(
        u.as_str(),
        "https://api.example.com/v1/chat/completions?a=1"
    );
    assert!(upstream_url(&base, "", None).is_some());
    assert!(upstream_url(&base, "../x", None).is_none());
    assert!(upstream_url(&base, "x", Some("a#b")).is_none());
    let other = reqwest::Url::parse("https://api.example.com/v1x").unwrap();
    assert!(
        upstream_url(&base, "", None)
            .unwrap()
            .path()
            .starts_with(base.path())
    );
    let _ = other;
}

#[tokio::test]
async fn config_rejects_bad_upstreams_and_keys() {
    for (up, key) in [
        ("https://u:p@api.example.com", "k"),
        ("https://api.example.com/#x", "k"),
        ("https://api.example.com", "bad\nkey"),
    ] {
        let config = ProxyConfig {
            providers: vec![ProviderConfig::new("x", ProviderKind::OpenAi, up, key)],
            ..ProxyConfig::default()
        };
        let r = KeyProxy::start(config, "127.0.0.1:0".parse().unwrap()).await;
        assert!(matches!(r, Err(CoreError::InvalidInput(_))), "{up}");
    }
}

#[tokio::test]
async fn concurrent_grant_revoke_and_stats_do_not_deadlock() {
    let fx = setup().await;
    let p = fx.proxy.clone();
    let sessions: Vec<SessionId> = (0..8).map(|_| SessionId::new()).collect();
    let done = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::task::spawn_blocking(move || {
            std::thread::scope(|sc| {
                for i in 0..4 {
                    let p = p.clone();
                    let sessions = sessions.clone();
                    sc.spawn(move || {
                        for n in 0..2000 {
                            let s = &sessions[(n + i) % sessions.len()];
                            match (n + i) % 3 {
                                0 => {
                                    p.grant(s).unwrap();
                                }
                                1 => p.revoke_session(s),
                                _ => {
                                    let _ = p.stats(s);
                                }
                            }
                        }
                    });
                }
            });
        }),
    )
    .await;
    assert!(done.is_ok(), "grant/revoke/stats deadlocked");
}

#[tokio::test]
async fn revoke_grant_only_revokes_the_current_token() {
    let fx = setup().await;
    let s = SessionId::new();
    let old = fx.proxy.grant(&s).unwrap();
    let new = fx.proxy.grant(&s).unwrap();
    fx.proxy.revoke_grant(&s, &old);
    assert!(fx.proxy.stats(&s).is_some(), "the newer token survives");
    fx.proxy.revoke_grant(&s, &new);
    assert!(fx.proxy.stats(&s).is_none());
}

#[tokio::test]
async fn expired_tokens_leave_no_session_entry() {
    let fx = setup_with(Duration::from_millis(20), 1024).await;
    let a = SessionId::new();
    fx.proxy.grant(&a).unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    fx.proxy.grant(&SessionId::new()).unwrap();
    assert!(fx.proxy.stats(&a).is_none());
    assert!(!fx.proxy.store().by_session.contains_key(&a));
}
