//! Host key proxy (W2-2).
//!
//! Real upstream API keys stay in the server process. A sandboxed session gets:
//!
//! - a session token (`ost_` + 64 hex chars), valid only for that session, until it ends
//!   or its TTL passes, and
//! - for each configured provider, a loopback base URL (`http://127.0.0.1:<port>/p/<id>`)
//!   in the environment variable the agent already reads (`ANTHROPIC_BASE_URL`, ...),
//!   with the session token in the matching key variable (`ANTHROPIC_API_KEY`, ...).
//!
//! The proxy accepts the session token in `x-api-key`, `api-key` or
//! `Authorization: Bearer`, drops every client credential and hop-by-hop header,
//! injects the real key the way the provider expects it, and streams the upstream
//! response (SSE included) back unchanged. It never follows redirects, so a key cannot
//! be replayed to another host, and it only listens on loopback.
//!
//! Inside `bwrap`, reaching the host loopback needs a shared network namespace, so
//! [`ProxyEnvSandbox`] turns networking on for proxied sessions. The agent still holds
//! no key; restricting other egress (nftables / slirp4netns) is a separate step.

#[cfg(test)]
mod tests;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use om_core::ids::SessionId;
use om_core::proxy::{ProviderInfo, ProviderKind};
use om_core::sandbox::{CommandSpec, Sandbox, SandboxMode, SandboxSpec};
use om_core::{CoreError, CoreResult};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Prefix of session tokens handed to agents.
pub const SESSION_TOKEN_PREFIX: &str = "ost_";

/// One upstream provider and its real key. `Debug` never prints the key.
#[derive(Clone)]
pub struct ProviderConfig {
    /// Path segment (`[a-z0-9_-]+`), e.g. `anthropic`, `deepseek`.
    pub id: String,
    pub kind: ProviderKind,
    /// Upstream base URL, including any version prefix the SDK expects in its base URL
    /// (`https://api.anthropic.com`, `https://api.openai.com/v1`,
    /// `https://api.deepseek.com`).
    pub upstream: String,
    api_key: String,
    /// Overrides the kind's default environment variable names.
    pub base_url_env: Option<String>,
    pub api_key_env: Option<String>,
}

impl ProviderConfig {
    pub fn new(
        id: impl Into<String>,
        kind: ProviderKind,
        upstream: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            upstream: upstream.into(),
            api_key: api_key.into(),
            base_url_env: None,
            api_key_env: None,
        }
    }

    /// Uses custom environment variable names (e.g. `DEEPSEEK_BASE_URL`).
    pub fn with_env(mut self, base_url_env: &str, api_key_env: &str) -> Self {
        self.base_url_env = Some(base_url_env.into());
        self.api_key_env = Some(api_key_env.into());
        self
    }

    fn env_names(&self) -> (String, String) {
        let (b, k) = self.kind.default_env();
        (
            self.base_url_env.clone().unwrap_or_else(|| b.into()),
            self.api_key_env.clone().unwrap_or_else(|| k.into()),
        )
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("upstream", &self.upstream)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Proxy settings.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub providers: Vec<ProviderConfig>,
    /// Lifetime of a session token; a resumed session gets a fresh one.
    pub token_ttl: Duration,
    /// Largest request body forwarded upstream.
    pub max_body_bytes: usize,
    pub connect_timeout: Duration,
    /// Honor `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` for upstream calls (needed where
    /// providers are only reachable through an outbound proxy).
    pub use_system_proxy: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            token_ttl: Duration::from_secs(24 * 3600),
            max_body_bytes: 32 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
            use_system_proxy: true,
        }
    }
}

/// Per-session request counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProxyStats {
    pub requests: u64,
    pub upstream_errors: u64,
}

/// What a session's agent receives: its token and the environment to set.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionGrant {
    pub token: String,
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for SessionGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionGrant")
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

struct Grant {
    session_id: SessionId,
    expires_at: Instant,
    requests: AtomicU64,
    upstream_errors: AtomicU64,
}

struct Inner {
    addr: SocketAddr,
    config: ProxyConfig,
    providers: HashMap<String, ProviderConfig>,
    /// sha256(token) -> grant
    grants: Mutex<HashMap<String, Arc<Grant>>>,
    /// session -> sha256(token), so a session holds at most one live token.
    by_session: Mutex<HashMap<SessionId, String>>,
    client: reqwest::Client,
}

/// Handle to the running proxy. Cheap to clone.
#[derive(Clone)]
pub struct KeyProxy {
    inner: Arc<Inner>,
}

fn hash(token: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn new_token() -> CoreResult<String> {
    use std::fmt::Write;
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf)
        .map_err(|e| CoreError::Storage(format!("OS random source failed: {e}")))?;
    Ok(buf
        .iter()
        .fold(String::from(SESSION_TOKEN_PREFIX), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }))
}

fn validate(config: &ProxyConfig) -> CoreResult<HashMap<String, ProviderConfig>> {
    let mut providers = HashMap::new();
    let mut env_names = std::collections::HashSet::new();
    for p in &config.providers {
        let id_ok = !p.id.is_empty()
            && p.id.len() <= 32
            && p.id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !id_ok {
            return Err(CoreError::InvalidInput(format!(
                "provider id `{}` must be 1-32 chars of [a-z0-9_-]",
                p.id
            )));
        }
        let url = reqwest::Url::parse(&p.upstream)
            .map_err(|e| CoreError::InvalidInput(format!("provider `{}` upstream: {e}", p.id)))?;
        if !matches!(url.scheme(), "https" | "http") || url.query().is_some() {
            return Err(CoreError::InvalidInput(format!(
                "provider `{}` upstream must be an http(s) URL without a query",
                p.id
            )));
        }
        if p.api_key.trim().is_empty() {
            return Err(CoreError::InvalidInput(format!(
                "provider `{}` has an empty API key",
                p.id
            )));
        }
        let (b, k) = p.env_names();
        for name in [b, k] {
            if !env_names.insert(name.clone()) {
                return Err(CoreError::InvalidInput(format!(
                    "environment variable `{name}` is used by two providers; set distinct names with with_env"
                )));
            }
        }
        if providers.insert(p.id.clone(), p.clone()).is_some() {
            return Err(CoreError::InvalidInput(format!(
                "duplicate provider id `{}`",
                p.id
            )));
        }
    }
    Ok(providers)
}

impl KeyProxy {
    /// Validates the config, binds `bind` (must be a loopback address; port 0 picks one)
    /// and serves until the returned task is aborted.
    pub async fn start(
        config: ProxyConfig,
        bind: SocketAddr,
    ) -> CoreResult<(Self, JoinHandle<()>)> {
        if !bind.ip().is_loopback() {
            return Err(CoreError::InvalidInput(
                "the key proxy only listens on a loopback address".into(),
            ));
        }
        let providers = validate(&config)?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if !config.use_system_proxy {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| CoreError::InvalidInput(format!("http client: {e}")))?;
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let proxy = Self {
            inner: Arc::new(Inner {
                addr,
                config,
                providers,
                grants: Mutex::default(),
                by_session: Mutex::default(),
                client,
            }),
        };
        let app = Router::new()
            .route("/p/{provider}", any(forward_root))
            .route("/p/{provider}/{*rest}", any(forward))
            .fallback(|| async { error(StatusCode::NOT_FOUND, "unknown route") })
            .with_state(proxy.clone());
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "key proxy stopped");
            }
        });
        tracing::info!(%addr, providers = proxy.inner.providers.len(), "key proxy listening");
        Ok((proxy, task))
    }

    pub fn addr(&self) -> SocketAddr {
        self.inner.addr
    }

    /// Configured providers, sorted by id, without keys.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        let mut v: Vec<_> = self
            .inner
            .providers
            .values()
            .map(|p| ProviderInfo {
                id: p.id.clone(),
                kind: p.kind,
            })
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Issues the session's token (revoking any previous one) and the environment its
    /// agent needs.
    pub fn grant(&self, session_id: &SessionId) -> CoreResult<SessionGrant> {
        let token = new_token()?;
        let h = hash(&token);
        let grant = Arc::new(Grant {
            session_id: session_id.clone(),
            expires_at: Instant::now() + self.inner.config.token_ttl,
            requests: AtomicU64::new(0),
            upstream_errors: AtomicU64::new(0),
        });
        {
            let mut grants = self.inner.grants.lock().expect("grants poisoned");
            let mut by_session = self.inner.by_session.lock().expect("by_session poisoned");
            if let Some(old) = by_session.insert(session_id.clone(), h.clone()) {
                grants.remove(&old);
            }
            let now = Instant::now();
            grants.retain(|_, g| g.expires_at > now);
            grants.insert(h, grant);
        }
        let mut env = BTreeMap::new();
        for p in self.inner.providers.values() {
            let (base_env, key_env) = p.env_names();
            env.insert(base_env, format!("http://{}/p/{}", self.inner.addr, p.id));
            env.insert(key_env, token.clone());
        }
        Ok(SessionGrant { token, env })
    }

    /// Revokes the session's token. Call when the session's agent stops.
    pub fn revoke_session(&self, session_id: &SessionId) {
        let mut grants = self.inner.grants.lock().expect("grants poisoned");
        let mut by_session = self.inner.by_session.lock().expect("by_session poisoned");
        if let Some(h) = by_session.remove(session_id) {
            grants.remove(&h);
        }
    }

    /// Counters for the session's current token.
    pub fn stats(&self, session_id: &SessionId) -> Option<ProxyStats> {
        let by_session = self.inner.by_session.lock().ok()?;
        let h = by_session.get(session_id)?;
        let grants = self.inner.grants.lock().ok()?;
        grants.get(h).map(|g| ProxyStats {
            requests: g.requests.load(Ordering::Relaxed),
            upstream_errors: g.upstream_errors.load(Ordering::Relaxed),
        })
    }

    fn lookup(&self, token: &str) -> Option<Arc<Grant>> {
        if !token.starts_with(SESSION_TOKEN_PREFIX) {
            return None;
        }
        let grants = self.inner.grants.lock().ok()?;
        grants
            .get(&hash(token))
            .filter(|g| g.expires_at > Instant::now())
            .cloned()
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"error": {"type": "om_proxy_error", "message": message}}).to_string(),
    )
        .into_response()
}

/// The session token the client sent, from whichever header its SDK uses. When several
/// credential headers are present, the one that looks like a session token wins.
fn client_token(headers: &HeaderMap) -> Option<&str> {
    let mut candidates = Vec::new();
    for name in ["x-api-key", "api-key"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            candidates.push(v.trim());
        }
    }
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some((scheme, token)) = auth.trim().split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        candidates.push(token.trim());
    }
    candidates
        .iter()
        .find(|t| t.starts_with(SESSION_TOKEN_PREFIX))
        .or(candidates.first())
        .copied()
}

/// Request headers never forwarded upstream: credentials, hop-by-hop and
/// proxy-identifying headers.
fn drop_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "x-api-key"
            | "api-key"
            | "cookie"
            | "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "forwarded"
    ) || name.as_str().starts_with("x-forwarded-")
}

fn drop_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "set-cookie"
    )
}

/// Rejects paths that could climb out of the provider's base URL once decoded.
fn safe_rest(rest: &str) -> bool {
    !rest.starts_with('/')
        && !rest.contains('\\')
        && rest.split('/').all(|seg| {
            let lower = seg.to_ascii_lowercase();
            let decoded = lower.replace("%2e", ".");
            decoded != "." && decoded != ".." && !lower.contains("%2f") && !lower.contains("%5c")
        })
}

async fn forward_root(
    State(proxy): State<KeyProxy>,
    Path(provider): Path<String>,
    req: Request,
) -> Response {
    handle(proxy, provider, String::new(), req).await
}

async fn forward(
    State(proxy): State<KeyProxy>,
    Path((provider, rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    handle(proxy, provider, rest, req).await
}

async fn handle(proxy: KeyProxy, provider_id: String, rest: String, req: Request) -> Response {
    let Some(grant) = client_token(req.headers()).and_then(|t| proxy.lookup(t)) else {
        return error(StatusCode::UNAUTHORIZED, "invalid or expired session token");
    };
    grant.requests.fetch_add(1, Ordering::Relaxed);
    let Some(provider) = proxy.inner.providers.get(&provider_id) else {
        return error(StatusCode::NOT_FOUND, "unknown provider");
    };
    // Use the raw (still percent-encoded) path so the upstream sees exactly what the
    // client sent, after checking it cannot escape the base path.
    let raw_path = req.uri().path();
    let prefix = format!("/p/{provider_id}");
    let raw_rest = raw_path
        .strip_prefix(&prefix)
        .map(|r| r.strip_prefix('/').unwrap_or(r))
        .unwrap_or(&rest);
    if !safe_rest(raw_rest) {
        return error(StatusCode::BAD_REQUEST, "path not allowed");
    }
    let mut url = provider.upstream.trim_end_matches('/').to_owned();
    if !raw_rest.is_empty() {
        url.push('/');
        url.push_str(raw_rest);
    }
    if let Some(q) = req.uri().query() {
        url.push('?');
        url.push_str(q);
    }

    let method = req.method().clone();
    let mut headers = HeaderMap::new();
    for (name, value) in req.headers() {
        if !drop_request_header(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    let key = match HeaderValue::from_str(&match provider.kind {
        ProviderKind::Anthropic => provider.api_key.clone(),
        ProviderKind::OpenAi => format!("Bearer {}", provider.api_key),
    }) {
        Ok(mut v) => {
            v.set_sensitive(true);
            v
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "invalid provider key"),
    };
    match provider.kind {
        ProviderKind::Anthropic => headers.insert("x-api-key", key),
        ProviderKind::OpenAi => headers.insert(header::AUTHORIZATION, key),
    };

    let body = match axum::body::to_bytes(req.into_body(), proxy.inner.config.max_body_bytes).await
    {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let upstream = proxy
        .inner
        .client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await;
    let resp = match upstream {
        Ok(r) => r,
        Err(e) => {
            grant.upstream_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                session = %grant.session_id,
                provider = %provider_id,
                error = %e.without_url(),
                "upstream request failed"
            );
            return error(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };

    let mut out = Response::builder().status(resp.status());
    for (name, value) in resp.headers() {
        if !drop_response_header(name) {
            out = out.header(name, value);
        }
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "bad upstream response"))
}

/// Wraps a sandbox so the agent gets the proxy environment instead of real keys, and a
/// network namespace that can reach the host loopback proxy.
pub struct ProxyEnvSandbox {
    inner: Arc<dyn Sandbox>,
    env: BTreeMap<String, String>,
}

impl ProxyEnvSandbox {
    pub fn new(inner: Arc<dyn Sandbox>, grant: &SessionGrant) -> Self {
        Self {
            inner,
            env: grant.env.clone(),
        }
    }
}

impl Sandbox for ProxyEnvSandbox {
    fn mode(&self) -> SandboxMode {
        self.inner.mode()
    }

    fn wrap(&self, spec: &SandboxSpec, mut cmd: CommandSpec) -> CoreResult<CommandSpec> {
        let mut spec = spec.clone();
        spec.network = true;
        // The proxy's values win over anything the harness set for the same variable.
        cmd.env.extend(self.env.clone());
        self.inner.wrap(&spec, cmd)
    }
}
