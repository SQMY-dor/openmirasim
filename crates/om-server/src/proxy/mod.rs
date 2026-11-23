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
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
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
    /// Longest gap between two reads of an upstream response (streams included).
    pub read_timeout: Duration,
    /// Longest time to receive a client's request body.
    pub body_timeout: Duration,
    /// Requests forwarded at once across all sessions; more get `503`.
    pub max_concurrent: usize,
    /// Requests forwarded at once for one session; more get `429`.
    pub max_concurrent_per_session: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            token_ttl: Duration::from_secs(24 * 3600),
            max_body_bytes: 32 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
            use_system_proxy: true,
            read_timeout: Duration::from_secs(300),
            body_timeout: Duration::from_secs(60),
            max_concurrent: 256,
            max_concurrent_per_session: 16,
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
    in_flight: AtomicU64,
}

/// Tokens and their sessions, kept under one lock so the two maps never disagree and
/// no two locks are ever taken in different orders.
#[derive(Default)]
struct GrantStore {
    /// sha256(token) -> grant
    grants: HashMap<String, Arc<Grant>>,
    /// session -> sha256(token), so a session holds at most one live token.
    by_session: HashMap<SessionId, String>,
}

impl GrantStore {
    fn purge_expired(&mut self, now: Instant) {
        let expired: Vec<String> = self
            .grants
            .iter()
            .filter(|(_, g)| g.expires_at <= now)
            .map(|(h, _)| h.clone())
            .collect();
        for h in expired {
            if let Some(g) = self.grants.remove(&h)
                && self.by_session.get(&g.session_id) == Some(&h)
            {
                self.by_session.remove(&g.session_id);
            }
        }
    }
}

/// A validated provider: its config plus the parsed base URL and the auth header value.
struct Provider {
    config: ProviderConfig,
    base: reqwest::Url,
    key_header: HeaderValue,
}

struct Inner {
    addr: SocketAddr,
    config: ProxyConfig,
    providers: HashMap<String, Provider>,
    store: Mutex<GrantStore>,
    in_flight: Arc<tokio::sync::Semaphore>,
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

fn validate(config: &ProxyConfig) -> CoreResult<HashMap<String, Provider>> {
    if config.max_concurrent == 0 || config.max_concurrent_per_session == 0 {
        return Err(CoreError::InvalidInput(
            "proxy concurrency limits must be at least 1".into(),
        ));
    }
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
        if !matches!(url.scheme(), "https" | "http")
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            return Err(CoreError::InvalidInput(format!(
                "provider `{}` upstream must be an http(s) URL with a host and no query, fragment or credentials",
                p.id
            )));
        }
        if p.api_key.trim().is_empty() {
            return Err(CoreError::InvalidInput(format!(
                "provider `{}` has an empty API key",
                p.id
            )));
        }
        let mut key_header = HeaderValue::from_str(&match p.kind {
            ProviderKind::Anthropic => p.api_key.clone(),
            ProviderKind::OpenAi => format!("Bearer {}", p.api_key),
        })
        .map_err(|_| {
            CoreError::InvalidInput(format!(
                "provider `{}` API key is not a valid header value",
                p.id
            ))
        })?;
        key_header.set_sensitive(true);
        let mut base = url;
        let trimmed = base.path().trim_end_matches('/').to_owned();
        base.set_path(&trimmed);
        let (b, k) = p.env_names();
        for name in [b, k] {
            if !env_names.insert(name.clone()) {
                return Err(CoreError::InvalidInput(format!(
                    "environment variable `{name}` is used by two providers; set distinct names with with_env"
                )));
            }
        }
        let provider = Provider {
            config: p.clone(),
            base,
            key_header,
        };
        if providers.insert(p.id.clone(), provider).is_some() {
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
            .read_timeout(config.read_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if !config.use_system_proxy {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| CoreError::InvalidInput(format!("http client: {e}")))?;
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let in_flight = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent));
        let proxy = Self {
            inner: Arc::new(Inner {
                in_flight,
                addr,
                config,
                providers,
                store: Mutex::default(),
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
                id: p.config.id.clone(),
                kind: p.config.kind,
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
            in_flight: AtomicU64::new(0),
        });
        {
            let mut store = self.store();
            store.purge_expired(Instant::now());
            if let Some(old) = store.by_session.insert(session_id.clone(), h.clone()) {
                store.grants.remove(&old);
            }
            store.grants.insert(h, grant);
        }
        let mut env = BTreeMap::new();
        for p in self.inner.providers.values().map(|p| &p.config) {
            let (base_env, key_env) = p.env_names();
            env.insert(base_env, format!("http://{}/p/{}", self.inner.addr, p.id));
            env.insert(key_env, token.clone());
        }
        Ok(SessionGrant { token, env })
    }

    /// Revokes the session's token. Call when the session's agent stops.
    pub fn revoke_session(&self, session_id: &SessionId) {
        let mut store = self.store();
        if let Some(h) = store.by_session.remove(session_id) {
            store.grants.remove(&h);
        }
    }

    /// Revokes `grant`'s token only if it is still the session's current one, so a
    /// stale owner cannot revoke the token a newer agent of the same session holds.
    pub fn revoke_grant(&self, session_id: &SessionId, grant: &SessionGrant) {
        let h = hash(&grant.token);
        let mut store = self.store();
        if store.by_session.get(session_id) == Some(&h) {
            store.by_session.remove(session_id);
        }
        store.grants.remove(&h);
    }

    fn store(&self) -> std::sync::MutexGuard<'_, GrantStore> {
        self.inner
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Counters for the session's current token.
    pub fn stats(&self, session_id: &SessionId) -> Option<ProxyStats> {
        let store = self.store();
        let h = store.by_session.get(session_id)?;
        store.grants.get(h).map(|g| ProxyStats {
            requests: g.requests.load(Ordering::Relaxed),
            upstream_errors: g.upstream_errors.load(Ordering::Relaxed),
        })
    }

    fn lookup(&self, token: &str) -> Option<Arc<Grant>> {
        if !token.starts_with(SESSION_TOKEN_PREFIX) {
            return None;
        }
        let store = self.store();
        store
            .grants
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

/// Value of a hex digit.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decodes one path segment; `None` on a malformed escape.
fn decode_segment(seg: &str) -> Option<Vec<u8>> {
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex_val(*b.get(i + 1)?)?;
            let lo = hex_val(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Rejects paths that could climb out of the provider's base URL, change the query or
/// fragment, or be rewritten by URL parsers (which strip tabs and newlines), whether
/// written literally or percent-encoded.
fn safe_rest(rest: &str) -> bool {
    if rest.starts_with('/') {
        return false;
    }
    rest.split('/').all(|seg| {
        let Some(decoded) = decode_segment(seg) else {
            return false;
        };
        let bad_byte = decoded
            .iter()
            .chain(seg.as_bytes())
            .any(|&c| c <= 0x20 || c == 0x7f || matches!(c, b'?' | b'#' | b'\\' | b'/'));
        !bad_byte && decoded != b"." && decoded != b".."
    })
}

/// The upstream URL for `raw_rest` under `base`, checked again after parsing so it keeps
/// the base's scheme, host and port and stays under the base path.
fn upstream_url(base: &reqwest::Url, raw_rest: &str, query: Option<&str>) -> Option<reqwest::Url> {
    let mut s = base.as_str().trim_end_matches('/').to_owned();
    if !raw_rest.is_empty() {
        s.push('/');
        s.push_str(raw_rest);
    }
    if let Some(q) = query {
        if q.bytes().any(|c| c <= 0x20 || c == 0x7f || c == b'#') {
            return None;
        }
        s.push('?');
        s.push_str(q);
    }
    let url = reqwest::Url::parse(&s).ok()?;
    let base_path = base.path().trim_end_matches('/');
    let path_ok = url.path() == base_path
        || url.path() == format!("{base_path}/")
        || url.path().starts_with(&format!("{base_path}/"));
    (url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && path_ok)
        .then_some(url)
}

/// Header names a request's `Connection` header marks as hop-by-hop.
fn connection_named(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Decrements a grant's in-flight count when the request (and its streamed response)
/// is done.
struct InFlight {
    grant: Arc<Grant>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.grant.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
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
    let method = req.method().clone();
    if !matches!(
        method,
        Method::GET | Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    // Use the raw (still percent-encoded) path so the upstream sees exactly what the
    // client sent, after checking it cannot escape the base path. The router decoded
    // `provider`, so a raw path that does not start with it literally is refused
    // rather than falling back to the decoded rest.
    let _ = &rest;
    let raw_path = req.uri().path();
    let prefix = format!("/p/{provider_id}");
    let raw_rest = match raw_path.strip_prefix(&prefix) {
        Some("") => "",
        Some(r) if r.starts_with('/') => &r[1..],
        _ => return error(StatusCode::BAD_REQUEST, "path not allowed"),
    };
    if !safe_rest(raw_rest) {
        return error(StatusCode::BAD_REQUEST, "path not allowed");
    }
    let Some(url) = upstream_url(&provider.base, raw_rest, req.uri().query()) else {
        return error(StatusCode::BAD_REQUEST, "path not allowed");
    };

    // Concurrency: one slot per session, then a global slot.
    let per_session = grant.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
    let Ok(permit) = proxy.inner.in_flight.clone().try_acquire_owned() else {
        grant.in_flight.fetch_sub(1, Ordering::Relaxed);
        return error(StatusCode::SERVICE_UNAVAILABLE, "proxy is busy");
    };
    let guard = InFlight {
        grant: grant.clone(),
        _permit: permit,
    };
    if per_session > proxy.inner.config.max_concurrent_per_session {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many concurrent requests for this session",
        );
    }

    let hop = connection_named(req.headers());
    let mut headers = HeaderMap::new();
    for (name, value) in req.headers() {
        if !drop_request_header(name) && !hop.iter().any(|h| h == name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    match provider.config.kind {
        ProviderKind::Anthropic => headers.insert("x-api-key", provider.key_header.clone()),
        ProviderKind::OpenAi => headers.insert(header::AUTHORIZATION, provider.key_header.clone()),
    };

    let body = match tokio::time::timeout(
        proxy.inner.config.body_timeout,
        axum::body::to_bytes(req.into_body(), proxy.inner.config.max_body_bytes),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "request body timed out"),
    };

    let upstream = proxy
        .inner
        .client
        .request(method, url)
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
    let hop = connection_named(resp.headers());
    for (name, value) in resp.headers() {
        if !drop_response_header(name) && !hop.iter().any(|h| h == name.as_str()) {
            out = out.header(name, value);
        }
    }
    let stream = futures::StreamExt::map(resp.bytes_stream(), move |chunk| {
        let _held = &guard;
        chunk
    });
    out.body(Body::from_stream(stream))
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
