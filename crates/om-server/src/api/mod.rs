//! Axum HTTP REST + `/ws/events` + static SPA gateway (W0-5 + Phase 1 + Phase 2 RBAC /
//! KeyProxy / Web Push / Antigravity GUI Bridge), wired directly to
//! [`crate::orchestrator::Orchestrator`], [`crate::auth::Auth`],
//! [`crate::workspace::WorkspaceManager`], and [`crate::db::Db`].

pub mod auth;
pub mod hub;
pub mod pty_ws;
pub mod push;
pub mod ws;

use crate::auth::Auth;
use crate::harness::{
    HarnessDescriptor, HarnessProbeReport, RemoteScreenInfo, RemoteScreenRegistry,
    probe_registered_harnesses,
};
use crate::orchestrator::Orchestrator;
use crate::pty::{PtyConfig, PtyManager, PtyStatus};
use crate::scheduler::{Dispatcher, GoalRunner, Scheduler};
use auth::{authenticate_secret, extract_bearer_token};
use axum::body::Body;
use axum::extract::{Extension, FromRef, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use bytes::Bytes;
use hub::{
    CreateMcpServerRequest, CreateSkillRequest, EffectiveHubResponse, HubScopeLevel, HubStore,
    ListMcpServersResponse, ListSkillsResponse, McpProbeResult, McpServerRecord, SkillRecord,
    UpdateMcpServerRequest, UpdateSkillRequest,
};
use om_core::CoreError;
use om_core::api::{
    AddMemberRequest, ApiError, ApiErrorCode, ApprovalRule, AuditEntry, ChangeRoleRequest,
    ClearApprovalRulesResponse, CreateSessionRequest, IssueTokenRequest, ListApprovalRulesResponse,
    ListApprovalsResponse, ListAuditResponse, ListDispatchesResponse, ListEventsQuery,
    ListEventsResponse, ListGoalsResponse, ListMembersResponse, ListProvidersResponse,
    ListQueueResponse, ListScheduleRunsResponse, ListSchedulesResponse, ListSessionsResponse,
    ListTokensResponse, ListWorkspacesResponse, ListWorktreeFilesResponse, MAX_EVENTS_PAGE,
    MoveQueuedRequest, ProxyStats, RegisterWorkspaceRequest, ResolveApprovalRequest,
    SetScheduleEnabledRequest, SubmitPromptRequest, SubmitPromptResponse,
};
use om_core::auth::{Action, DeviceTokenRecord, IssuedToken, MemberRecord, Principal};
use om_core::dispatch::{Dispatch, NewDispatch};
use om_core::goal::{Goal, NewGoal};
use om_core::ids::{
    ApprovalId, GoalId, MemberId, QueuedPromptId, ScheduleId, SessionId, SpaceId, TokenId,
    WorkspaceId,
};
use om_core::schedule::{NewSchedule, Schedule, ScheduleRun};
use om_core::workspace::{WorkspaceSummary, WorktreeDiff, WorktreeInfo};
use push::{
    ListPushSubscriptionsResponse, PushConfigResponse, PushStore, PushSubscriptionRecord,
    PushTestResponse, RegisterPushSubscriptionRequest,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Gateway configuration.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Optional static bootstrap Bearer token (resolves to the default space's `Owner`).
    /// Leave empty to enforce pure `omt_...` RBAC device-token authentication.
    pub auth_token: String,
    /// Active default space id.
    pub default_space_id: SpaceId,
    /// Optional compiled frontend directory (e.g. `apps/web/dist`) served with SPA fallback.
    pub static_dir: Option<PathBuf>,
    /// Optional CORS allowed origin (e.g. `http://127.0.0.1:5173` during Vite dev).
    pub allow_cors_origin: Option<String>,
}

/// Optional Phase 2–4 gateway extensions (remote GUI screen registry, PWA Web Push, PTY runtime,
/// Schedules/Goals/Dispatch orchestration, and two-tier MCP Hub / Skills store).
#[derive(Clone, Default)]
pub struct ApiExtensions {
    pub remote_screens: RemoteScreenRegistry,
    pub push_store_path: Option<PathBuf>,
    pub vapid_public_key: Option<String>,
    pub allow_loopback_push: bool,
    pub pty: Option<PtyManager>,
    pub scheduler: Option<Scheduler>,
    pub goals: Option<GoalRunner>,
    pub dispatcher: Option<Dispatcher>,
    pub hub_store_path: Option<PathBuf>,
}

/// Shared Axum router state.
#[derive(Clone)]
pub struct AppState {
    pub orchestrator: Orchestrator,
    pub config: ApiConfig,
}

/// Cache TTL for `GET /api/v1/harnesses/probe` to prevent subprocess fork-bomb DoS.
const HARNESS_PROBE_CACHE_TTL: Duration = Duration::from_secs(30);

type ProbeCache = Arc<tokio::sync::Mutex<Option<(Instant, Vec<HarnessProbeReport>)>>>;

#[derive(Clone)]
pub(super) struct GatewayState {
    pub(super) app: AppState,
    pub(super) auth: Auth,
    pub(super) push: PushStore,
    pub(super) remote_screens: RemoteScreenRegistry,
    pub(super) pty: PtyManager,
    pub(super) scheduler: Scheduler,
    pub(super) goals: GoalRunner,
    pub(super) dispatcher: Dispatcher,
    pub(super) hub: HubStore,
    pub(super) probe_cache: ProbeCache,
}

impl FromRef<GatewayState> for AppState {
    fn from_ref(state: &GatewayState) -> Self {
        state.app.clone()
    }
}

/// Wrapper translating [`CoreError`] into the canonical [`ApiError`] JSON HTTP response.
pub struct HttpApiError(pub ApiError);

impl From<CoreError> for HttpApiError {
    fn from(err: CoreError) -> Self {
        let api = ApiError::from(&err);
        if api.code == ApiErrorCode::Internal {
            // Clients get a generic message; keep the detail in the server log.
            tracing::error!(error = %err, "internal error answering a request");
        }
        Self(api)
    }
}

impl IntoResponse for HttpApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0)).into_response()
    }
}

type ApiResult<T> = Result<T, HttpApiError>;

/// Builds the complete Axum router with default Phase 2–4 extensions.
pub fn build_router(state: AppState) -> Router {
    build_router_with_extensions(state, ApiExtensions::default())
}

/// Builds the complete Axum router with explicit Phase 2–4 extensions (remote GUI screen
/// registry, persistent PWA push store, optional VAPID key, native PTY supervisor,
/// Schedules/Goals/Dispatch engines, and two-tier MCP Hub / Skills registry).
pub fn build_router_with_extensions(state: AppState, ext: ApiExtensions) -> Router {
    let auth = Auth::new(state.orchestrator.db().clone());
    let push = PushStore::new(
        ext.push_store_path,
        ext.vapid_public_key,
        ext.allow_loopback_push,
    );
    let pty = ext
        .pty
        .unwrap_or_else(|| PtyManager::new(PtyConfig::default()));
    let scheduler = ext
        .scheduler
        .unwrap_or_else(|| Scheduler::new(state.orchestrator.clone()));
    let goals = ext
        .goals
        .unwrap_or_else(|| GoalRunner::new(state.orchestrator.clone()));
    let dispatcher = ext
        .dispatcher
        .unwrap_or_else(|| Dispatcher::new(state.orchestrator.clone()));
    let hub = HubStore::new(ext.hub_store_path);
    let gw = GatewayState {
        app: state.clone(),
        auth,
        push,
        remote_screens: ext.remote_screens,
        pty,
        scheduler,
        goals,
        dispatcher,
        hub,
        probe_cache: Arc::new(tokio::sync::Mutex::new(None)),
    };

    let api_routes = Router::new()
        // Identity & Team RBAC (W2-1)
        .route("/me", get(get_me))
        .route("/members", get(list_members).post(add_member))
        .route(
            "/members/{id}",
            patch(change_member_role).delete(disable_member),
        )
        .route(
            "/members/{id}/tokens",
            get(list_member_tokens).post(issue_member_token),
        )
        .route("/tokens/{id}", delete(revoke_token))
        // Harnesses & key proxy providers (W1-4 / W2-2)
        .route("/harnesses", get(list_harnesses))
        .route("/harnesses/probe", get(probe_harnesses))
        .route("/providers", get(list_providers))
        // Workspaces (W1-2)
        .route("/workspaces", get(list_workspaces).post(register_workspace))
        .route("/workspaces/{id}", get(get_workspace))
        // Sessions & events (W0-5)
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(close_session))
        .route("/sessions/{id}/resume", post(resume_session))
        .route("/sessions/{id}/events", get(list_events))
        .route("/sessions/{id}/prompts", post(submit_prompt))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/proxy/stats", get(get_session_proxy_stats))
        .route("/sessions/{id}/remote-screen", get(get_remote_screen))
        // Native PTY status, termination, and asciicast v2 recording (Phase 3)
        .route("/sessions/{id}/pty", get(get_pty_status).delete(kill_pty))
        .route("/sessions/{id}/pty/recording", get(get_pty_recording))
        // Queue & Steer (W2-3)
        .route("/sessions/{id}/queue", get(list_queue))
        .route("/sessions/{id}/queue/resume", post(resume_queue))
        .route("/sessions/{id}/queue/{qid}", delete(withdraw_queued))
        .route("/sessions/{id}/queue/{qid}/move", post(move_queued))
        // Worktree inspection & diff (W1-2)
        .route(
            "/sessions/{id}/worktree",
            get(get_worktree).delete(remove_worktree),
        )
        .route("/sessions/{id}/files", get(list_worktree_files))
        .route("/sessions/{id}/file", get(read_worktree_file))
        .route("/sessions/{id}/diff", get(get_worktree_diff))
        // Approvals, remembered rules & audit (W0-5 / W1-3 / W2-1)
        .route("/sessions/{id}/approvals", get(list_approvals))
        .route(
            "/sessions/{id}/approval-rules",
            get(list_approval_rules).delete(clear_approval_rules),
        )
        .route("/approvals/{id}/resolve", post(resolve_approval))
        .route("/audit", get(list_audit))
        // PWA Web Push (W2-4)
        .route("/push/config", get(get_push_config))
        .route(
            "/push/subscriptions",
            get(list_push_subscriptions).post(register_push_subscription),
        )
        .route("/push/subscriptions/{id}", delete(delete_push_subscription))
        .route("/push/test", post(test_push))
        // Schedules, Goals & Session-to-Session Dispatches (Phase 4)
        .route("/schedules", get(list_schedules).post(create_schedule))
        .route(
            "/schedules/{id}",
            get(get_schedule)
                .put(update_schedule)
                .delete(delete_schedule),
        )
        .route("/schedules/{id}/enabled", post(set_schedule_enabled))
        .route("/schedules/{id}/trigger", post(trigger_schedule))
        .route("/schedules/{id}/runs", get(list_schedule_runs))
        .route("/sessions/{id}/goals", get(list_goals).post(set_goal))
        .route("/goals/{id}/stop", post(stop_goal))
        .route("/sessions/{id}/dispatches", get(list_dispatches))
        .route("/dispatches", post(create_dispatch))
        // Two-tier Space / Workspace MCP Hub & Skills Lifecycle (Phase 4)
        .route(
            "/mcp/servers",
            get(list_mcp_servers).post(create_mcp_server),
        )
        .route(
            "/mcp/servers/{id}",
            patch(update_mcp_server).delete(delete_mcp_server),
        )
        .route("/mcp/servers/{id}/probe", post(probe_mcp_server))
        .route("/skills", get(list_skills).post(create_skill))
        .route("/skills/{id}", patch(update_skill).delete(delete_skill))
        .route("/hub/effective", get(get_effective_hub))
        .layer(middleware::from_fn_with_state(gw.clone(), require_auth));

    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws/events", get(ws::ws_events_handler))
        .route("/ws/pty/{id}", get(pty_ws::ws_pty_handler))
        .nest("/api/v1", api_routes);

    if let Some(dir) = &state.config.static_dir {
        let index = dir.join("index.html");
        let service = ServeDir::new(dir).fallback(ServeFile::new(index));
        router = router.fallback_service(service);
    }

    if let Some(origin) = &state.config.allow_cors_origin
        && let Ok(val) = origin.parse::<HeaderValue>()
    {
        let cors = CorsLayer::new()
            .allow_origin(val)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);
        router = router.layer(cors);
    }

    // Never log query strings (e.g. `/ws/events?token=...`) to avoid leaking access tokens.
    let trace = TraceLayer::new_for_http().make_span_with(|req: &http::Request<_>| {
        tracing::info_span!(
            "http_request",
            method = %req.method(),
            path = %req.uri().path(),
        )
    });

    router
        .layer(middleware::from_fn(apply_security_headers))
        .layer(trace)
        .with_state(gw)
}

async fn apply_security_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    if is_api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

/// Validates browser `Origin` headers on WebSocket upgrade requests to prevent cross-site
/// WebSocket hijacking (CSWSH). Non-browser clients (CLI, Tauri) omitting `Origin` are allowed.
pub(super) fn validate_ws_origin(
    headers: &HeaderMap,
    allow_cors_origin: Option<&str>,
) -> Result<(), String> {
    let Some(origin_raw) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let Ok(origin) = origin_raw.to_str() else {
        return Err("invalid Origin header".into());
    };
    let origin = origin.trim();
    if let Some(allowed) = allow_cors_origin
        && allowed.trim() == origin
    {
        return Ok(());
    }
    let parsed_origin =
        reqwest::Url::parse(origin).map_err(|_| "malformed WebSocket Origin header".to_string())?;
    let Some(host_hdr) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return Err("missing Host header for WebSocket Origin check".into());
    };
    let host_url =
        reqwest::Url::parse(&format!("{}://{}", parsed_origin.scheme(), host_hdr.trim()))
            .map_err(|_| "malformed Host header".to_string())?;
    if parsed_origin.host_str() == host_url.host_str()
        && parsed_origin.port_or_known_default() == host_url.port_or_known_default()
    {
        Ok(())
    } else {
        Err("WebSocket Origin does not match server Host".into())
    }
}

async fn require_auth(State(gw): State<GatewayState>, mut req: Request, next: Next) -> Response {
    let Some(token) = extract_bearer_token(req.headers()) else {
        return HttpApiError(ApiError {
            code: ApiErrorCode::Unauthorized,
            message: "invalid or missing Authorization Bearer token".into(),
        })
        .into_response();
    };

    match authenticate_secret(
        &gw.auth,
        gw.app.orchestrator.db(),
        &gw.app.config.default_space_id,
        &gw.app.config.auth_token,
        token,
    )
    .await
    {
        Ok(principal) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(err) => HttpApiError::from(err).into_response(),
    }
}

async fn ensure_session_in_space(
    gw: &GatewayState,
    principal: &Principal,
    sid: &SessionId,
) -> Result<om_core::session::SessionSummary, HttpApiError> {
    let summary = gw.app.orchestrator.db().get_session(sid).await?;
    if summary.space_id != principal.space_id {
        return Err(HttpApiError::from(CoreError::NotFound(format!(
            "session {sid}"
        ))));
    }
    Ok(summary)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

// ------------------------------------------------------------ identity & RBAC

async fn get_me(Extension(principal): Extension<Principal>) -> Json<Principal> {
    Json(principal)
}

async fn list_members(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ListMembersResponse>> {
    let members = gw.auth.list_members(&principal).await?;
    Ok(Json(ListMembersResponse { members }))
}

async fn add_member(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<AddMemberRequest>,
) -> ApiResult<(StatusCode, Json<MemberRecord>)> {
    let member = gw
        .auth
        .add_member(&principal, &body.display_name, body.role)
        .await?;
    Ok((StatusCode::CREATED, Json(member)))
}

async fn change_member_role(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<ChangeRoleRequest>,
) -> ApiResult<Json<MemberRecord>> {
    let member = gw
        .auth
        .change_role(&principal, &MemberId::from(id), body.role)
        .await?;
    Ok(Json(member))
}

async fn disable_member(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<MemberRecord>> {
    let member = gw
        .auth
        .disable_member(&principal, &MemberId::from(id))
        .await?;
    Ok(Json(member))
}

async fn list_member_tokens(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListTokensResponse>> {
    let tokens = gw.auth.list_tokens(&principal, &MemberId::from(id)).await?;
    Ok(Json(ListTokensResponse { tokens }))
}

async fn issue_member_token(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<IssueTokenRequest>,
) -> ApiResult<(StatusCode, Json<IssuedToken>)> {
    let ttl = body.ttl_secs.map(Duration::from_secs);
    let issued = gw
        .auth
        .issue_token(&principal, &MemberId::from(id), &body.label, ttl)
        .await?;
    Ok((StatusCode::CREATED, Json(issued)))
}

async fn revoke_token(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<DeviceTokenRecord>> {
    let token = gw.auth.revoke_token(&principal, &TokenId::from(id)).await?;
    Ok(Json(token))
}

// ------------------------------------------------------------------ harnesses

#[derive(Serialize)]
struct ListHarnessesResponse {
    harnesses: Vec<HarnessDescriptor>,
}

async fn list_harnesses(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ListHarnessesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    Ok(Json(ListHarnessesResponse {
        harnesses: gw.app.orchestrator.registry().list(),
    }))
}

#[derive(Serialize)]
struct ProbeHarnessesResponse {
    probes: Vec<HarnessProbeReport>,
}

async fn probe_harnesses(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ProbeHarnessesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let mut probes = {
        let mut cache = gw.probe_cache.lock().await;
        if let Some((at, cached)) = cache.as_ref()
            && at.elapsed() < HARNESS_PROBE_CACHE_TTL
        {
            cached.clone()
        } else {
            let fresh = probe_registered_harnesses(gw.app.orchestrator.registry()).await;
            *cache = Some((Instant::now(), fresh.clone()));
            fresh
        }
    };
    if !principal.can(Action::ManageWorkspaces) {
        for report in &mut probes {
            report.binary_path = None;
        }
    }
    Ok(Json(ProbeHarnessesResponse { probes }))
}

async fn list_providers(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ListProvidersResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let providers = gw
        .app
        .orchestrator
        .key_proxy()
        .map(|p| p.providers())
        .unwrap_or_default();
    Ok(Json(ListProvidersResponse { providers }))
}

async fn get_session_proxy_stats(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProxyStats>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let stats = gw
        .app
        .orchestrator
        .key_proxy()
        .and_then(|p| p.stats(&sid))
        .map(|s| ProxyStats {
            requests: s.requests,
            upstream_errors: s.upstream_errors,
        })
        .unwrap_or(ProxyStats {
            requests: 0,
            upstream_errors: 0,
        });
    Ok(Json(stats))
}

#[derive(Serialize)]
struct RemoteScreenResponse {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen: Option<RemoteScreenInfo>,
}

async fn get_remote_screen(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<RemoteScreenResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    // Viewers (lacking `PromptSession`) must never receive secret-bearing GUI takeover URLs.
    if !principal.can(Action::PromptSession) {
        return Ok(Json(RemoteScreenResponse {
            available: false,
            screen: None,
        }));
    }
    let screen = gw.remote_screens.get(&sid).map(|mut info| {
        info.profile_dir.clear();
        info
    });
    Ok(Json(RemoteScreenResponse {
        available: screen.is_some(),
        screen,
    }))
}

// ----------------------------------------------------------------- workspaces

fn require_workspaces(
    gw: &GatewayState,
) -> Result<&crate::workspace::WorkspaceManager, HttpApiError> {
    gw.app.orchestrator.workspaces().ok_or_else(|| {
        HttpApiError::from(CoreError::Unsupported {
            harness: "server".into(),
            capability: "workspaces (not configured on this server)",
        })
    })
}

async fn list_workspaces(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ListWorkspacesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let mgr = require_workspaces(&gw)?;
    let workspaces = mgr.list(&principal.space_id).await?;
    Ok(Json(ListWorkspacesResponse { workspaces }))
}

async fn register_workspace(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<RegisterWorkspaceRequest>,
) -> ApiResult<(StatusCode, Json<WorkspaceSummary>)> {
    gw.auth.require(&principal, Action::ManageWorkspaces)?;
    let mgr = require_workspaces(&gw)?;
    let summary = mgr
        .register(principal.space_id.clone(), &body.name, &body.repo_path)
        .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn get_workspace(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorkspaceSummary>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let mgr = require_workspaces(&gw)?;
    let wid = WorkspaceId::from(id);
    let summary = mgr.get(&wid).await?;
    if summary.space_id != principal.space_id {
        return Err(HttpApiError::from(CoreError::NotFound(format!(
            "workspace {wid}"
        ))));
    }
    Ok(Json(summary))
}

async fn get_worktree(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorktreeInfo>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let _ = require_workspaces(&gw)?;
    let sid = SessionId::from(id.clone());
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let stored = gw
        .app
        .orchestrator
        .db()
        .get_worktree(&sid)
        .await?
        .ok_or_else(|| HttpApiError::from(CoreError::NotFound(format!("worktree {id}"))))?;
    Ok(Json(stored.info))
}

async fn remove_worktree(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let mgr = require_workspaces(&gw)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    if gw.app.orchestrator.is_session_live(&sid) || gw.pty.is_active(&sid) {
        return Err(HttpApiError::from(CoreError::InvalidInput(
            "cannot remove worktree while the session or PTY is still active; close it first"
                .into(),
        )));
    }
    mgr.remove_worktree(&sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_worktree_files(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListWorktreeFilesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let mgr = require_workspaces(&gw)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let entries = mgr.list_files(&sid).await?;
    Ok(Json(ListWorktreeFilesResponse { entries }))
}

#[derive(Deserialize)]
struct ReadFileQuery {
    path: String,
}

#[derive(Serialize)]
struct ReadWorktreeFileResponse {
    path: String,
    content: String,
    utf8: bool,
    size: usize,
}

async fn read_worktree_file(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Query(query): Query<ReadFileQuery>,
) -> ApiResult<Json<ReadWorktreeFileResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let mgr = require_workspaces(&gw)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let bytes = mgr.read_file(&sid, &query.path).await?;
    let size = bytes.len();
    let (content, utf8) = match String::from_utf8(bytes) {
        Ok(s) => (s, true),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), false),
    };
    Ok(Json(ReadWorktreeFileResponse {
        path: query.path,
        content,
        utf8,
        size,
    }))
}

async fn get_worktree_diff(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorktreeDiff>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let mgr = require_workspaces(&gw)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let diff = mgr.diff(&sid).await?;
    Ok(Json(diff))
}

// ------------------------------------------------------------------- sessions

async fn list_sessions(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ListSessionsResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sessions = gw
        .app
        .orchestrator
        .db()
        .list_sessions(&principal.space_id)
        .await?;
    Ok(Json(ListSessionsResponse { sessions }))
}

async fn create_session(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<CreateSessionRequest>,
) -> ApiResult<(StatusCode, Json<om_core::session::SessionSummary>)> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let summary = gw
        .app
        .orchestrator
        .create_session(
            principal.space_id.clone(),
            body.config,
            Some(principal.member_id.clone()),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn get_session(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<om_core::session::SessionSummary>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let summary = ensure_session_in_space(&gw, &principal, &SessionId::from(id)).await?;
    Ok(Json(summary))
}

async fn resume_session(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<om_core::session::SessionSummary>> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let summary = gw.app.orchestrator.resume_session(&sid).await?;
    Ok(Json(summary))
}

async fn close_session(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    gw.pty.kill(&sid);
    gw.app.orchestrator.close_session(&sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_events(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Query(query): Query<ListEventsQuery>,
) -> ApiResult<Json<ListEventsResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let limit = query
        .limit
        .unwrap_or(MAX_EVENTS_PAGE)
        .clamp(1, MAX_EVENTS_PAGE);
    let page = gw
        .app
        .orchestrator
        .db()
        .list_events(&sid, query.after_seq, limit)
        .await?;
    Ok(Json(ListEventsResponse {
        events: page.events,
        head_seq: page.head_seq,
        has_more: page.has_more,
    }))
}

async fn submit_prompt(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<SubmitPromptRequest>,
) -> ApiResult<Response> {
    gw.auth.require(&principal, Action::PromptSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let actor = Some(principal.member_id.clone());
    if let Some(mode) = body.mode {
        let outcome = gw
            .app
            .orchestrator
            .send_prompt(&sid, actor, body.prompt, mode)
            .await?;
        return Ok((StatusCode::ACCEPTED, Json(outcome)).into_response());
    }
    let turn_id = gw
        .app
        .orchestrator
        .submit_prompt(&sid, actor, body.prompt)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(SubmitPromptResponse { turn_id })).into_response())
}

async fn cancel_turn(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::PromptSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    gw.app.orchestrator.cancel_turn(&sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------- queue & steer

async fn list_queue(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListQueueResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let items = gw.app.orchestrator.queue(&sid).await?;
    Ok(Json(ListQueueResponse { items }))
}

async fn withdraw_queued(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path((id, qid)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::PromptSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    gw.app
        .orchestrator
        .withdraw_queued(
            &sid,
            QueuedPromptId::from(qid),
            Some(principal.member_id.clone()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn move_queued(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path((id, qid)): Path<(String, String)>,
    Json(body): Json<MoveQueuedRequest>,
) -> ApiResult<Json<ListQueueResponse>> {
    gw.auth.require(&principal, Action::PromptSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let items = gw
        .app
        .orchestrator
        .move_queued(&sid, QueuedPromptId::from(qid), body.position as usize)
        .await?;
    Ok(Json(ListQueueResponse { items }))
}

async fn resume_queue(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::PromptSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    gw.app.orchestrator.resume_queue(&sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------- approvals & audit

#[derive(Debug, Default, Deserialize)]
struct ListApprovalsQuery {
    #[serde(default)]
    pending_only: bool,
}

async fn list_approvals(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Query(query): Query<ListApprovalsQuery>,
) -> ApiResult<Json<ListApprovalsResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let stored = gw
        .app
        .orchestrator
        .db()
        .list_approvals(&sid, query.pending_only)
        .await?;
    let approvals = stored.into_iter().map(|s| s.record).collect();
    Ok(Json(ListApprovalsResponse { approvals }))
}

async fn resolve_approval(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<ResolveApprovalRequest>,
) -> ApiResult<Json<om_core::approval::ApprovalRecord>> {
    gw.auth.require(&principal, Action::ResolveApproval)?;
    let aid = ApprovalId::from(id);
    let stored = gw.app.orchestrator.db().get_approval(&aid).await?;
    if ensure_session_in_space(&gw, &principal, &stored.record.session_id)
        .await
        .is_err()
    {
        return Err(HttpApiError::from(CoreError::NotFound(format!(
            "approval {aid}"
        ))));
    }
    let record = gw
        .app
        .orchestrator
        .resolve_approval(&aid, body.decision, Some(principal.member_id.clone()))
        .await?;
    Ok(Json(record))
}

async fn list_approval_rules(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListApprovalRulesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let rules = gw
        .app
        .orchestrator
        .db()
        .approval_rules(&sid)
        .await?
        .into_iter()
        .map(|r| ApprovalRule {
            rule_key: r.rule_key,
            allow: r.allow,
            option_kind: r.option_kind,
            created_by: r.created_by,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(ListApprovalRulesResponse { rules }))
}

async fn clear_approval_rules(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ClearApprovalRulesResponse>> {
    gw.auth.require(&principal, Action::ManageApprovalRules)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let cleared = gw
        .app
        .orchestrator
        .db()
        .clear_approval_rules(&sid, Some(principal.member_id.clone()))
        .await?;
    Ok(Json(ClearApprovalRulesResponse {
        cleared: u32::try_from(cleared).unwrap_or(u32::MAX),
    }))
}

#[derive(Debug, Default, Deserialize)]
struct ListAuditQuery {
    target: Option<String>,
    limit: Option<u32>,
}

async fn list_audit(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ListAuditQuery>,
) -> ApiResult<Json<ListAuditResponse>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let entries = gw
        .auth
        .audit_log(&principal, query.target, limit)
        .await?
        .into_iter()
        .map(|e| AuditEntry {
            id: e.id,
            ts: e.ts,
            actor: e.actor,
            action: e.action,
            target: e.target,
            detail: e.detail,
        })
        .collect();
    Ok(Json(ListAuditResponse { entries }))
}

// -------------------------------------------------------------- PWA Web Push

async fn get_push_config(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> Json<PushConfigResponse> {
    Json(gw.push.config_for(&principal.member_id))
}

async fn list_push_subscriptions(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> Json<ListPushSubscriptionsResponse> {
    Json(ListPushSubscriptionsResponse {
        subscriptions: gw.push.list_for(&principal.member_id),
    })
}

async fn register_push_subscription(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<RegisterPushSubscriptionRequest>,
) -> ApiResult<(StatusCode, Json<PushSubscriptionRecord>)> {
    let record = gw.push.register(&principal.member_id, body)?;
    Ok((StatusCode::CREATED, Json(record)))
}

async fn delete_push_subscription(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.push.delete(&principal.member_id, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn test_push(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> Json<PushTestResponse> {
    let subs = gw.push.list_for(&principal.member_id);
    Json(PushTestResponse {
        delivered: 0,
        registered: subs.len(),
        mode: if subs.is_empty() {
            "service_worker_local".into()
        } else {
            "web_push_registered_dry_run".into()
        },
        message: if subs.is_empty() {
            "已启用本地 PWA Service Worker 审批通知（服务端 VAPID 主动推送尚未配置）".into()
        } else {
            format!(
                "已登记 {} 个 Web Push 订阅端；服务端 VAPID 远端投递暂未启用（dry-run，实际投递 0）",
                subs.len()
            )
        },
    })
}

// ----------------------------------------------------------------- native PTY

async fn get_pty_status(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<PtyStatus>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    Ok(Json(gw.pty.status(&sid)))
}

async fn kill_pty(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    gw.pty.kill(&sid);
    Ok(StatusCode::NO_CONTENT)
}

async fn get_pty_recording(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let path = gw.pty.recording_path(&sid)?;
    let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        opts.open(path)
    })
    .await
    .map_err(|e| HttpApiError::from(CoreError::Io(std::io::Error::other(e.to_string()))))?
    .map_err(|e| HttpApiError::from(CoreError::Io(e)))?;

    let stream = futures::stream::try_unfold((Some(file), 0u64), |(file_opt, sent)| async move {
        const CHUNK_SIZE: usize = 64 * 1024;
        const MAX_STREAM_BYTES: u64 = crate::pty::cast::DEFAULT_MAX_CAST_BYTES;
        let Some(mut file) = file_opt else {
            return Ok::<_, std::io::Error>(None);
        };
        if sent >= MAX_STREAM_BYTES {
            return Ok(None);
        }
        let to_read = (MAX_STREAM_BYTES - sent).min(CHUNK_SIZE as u64) as usize;
        let (file, buf) =
            tokio::task::spawn_blocking(move || -> std::io::Result<(std::fs::File, Vec<u8>)> {
                use std::io::Read;
                let mut buf = vec![0u8; to_read];
                let n = file.read(&mut buf)?;
                buf.truncate(n);
                Ok((file, buf))
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))??;
        if buf.is_empty() {
            Ok(None)
        } else {
            let next_sent = sent.saturating_add(buf.len() as u64);
            Ok(Some((Bytes::from(buf), (Some(file), next_sent))))
        }
    });

    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-asciicast; charset=utf-8"),
        )],
        Body::from_stream(stream),
    )
        .into_response())
}

// -------------------------------------------------- Schedules, Goals, Dispatch

#[derive(Deserialize, Default)]
struct LimitQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn ensure_schedule_in_space(
    gw: &GatewayState,
    principal: &Principal,
    id: &ScheduleId,
) -> ApiResult<Schedule> {
    let s = gw.scheduler.get(id).await?;
    if s.space_id != principal.space_id {
        return Err(HttpApiError::from(CoreError::NotFound(format!(
            "schedule {id}"
        ))));
    }
    Ok(s)
}

async fn list_schedules(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<ListSchedulesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let schedules = gw.scheduler.list(&principal.space_id).await?;
    Ok(Json(ListSchedulesResponse { schedules }))
}

async fn create_schedule(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<NewSchedule>,
) -> ApiResult<(StatusCode, Json<Schedule>)> {
    let schedule = gw
        .scheduler
        .create(principal.space_id.clone(), &principal.member_id, body)
        .await?;
    Ok((StatusCode::CREATED, Json(schedule)))
}

async fn get_schedule(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Schedule>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let schedule = ensure_schedule_in_space(&gw, &principal, &ScheduleId::from(id)).await?;
    Ok(Json(schedule))
}

async fn update_schedule(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<NewSchedule>,
) -> ApiResult<Json<Schedule>> {
    let schedule = gw
        .scheduler
        .update(&ScheduleId::from(id), &principal.member_id, body)
        .await?;
    Ok(Json(schedule))
}

async fn set_schedule_enabled(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<SetScheduleEnabledRequest>,
) -> ApiResult<Json<Schedule>> {
    let schedule = gw
        .scheduler
        .set_enabled(&ScheduleId::from(id), &principal.member_id, body.enabled)
        .await?;
    Ok(Json(schedule))
}

async fn trigger_schedule(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ScheduleRun>> {
    let run = gw
        .scheduler
        .trigger(&ScheduleId::from(id), &principal.member_id)
        .await?;
    Ok(Json(run))
}

async fn list_schedule_runs(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<ListScheduleRunsResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = ScheduleId::from(id);
    ensure_schedule_in_space(&gw, &principal, &sid).await?;
    let runs = gw.scheduler.runs(&sid, query.limit.unwrap_or(50)).await?;
    Ok(Json(ListScheduleRunsResponse { runs }))
}

async fn delete_schedule(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.scheduler
        .delete(&ScheduleId::from(id), &principal.member_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_goals(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListGoalsResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let goals = gw.goals.list(&sid).await?;
    Ok(Json(ListGoalsResponse { goals }))
}

async fn set_goal(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<NewGoal>,
) -> ApiResult<(StatusCode, Json<Goal>)> {
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let goal = gw.goals.set_goal(&sid, &principal.member_id, body).await?;
    Ok((StatusCode::CREATED, Json(goal)))
}

async fn stop_goal(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Goal>> {
    let goal = gw
        .goals
        .stop_goal(&GoalId::from(id), &principal.member_id)
        .await?;
    Ok(Json(goal))
}

async fn list_dispatches(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<ListDispatchesResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let sid = SessionId::from(id);
    ensure_session_in_space(&gw, &principal, &sid).await?;
    let dispatches = gw.dispatcher.list(&sid, query.limit.unwrap_or(50)).await?;
    Ok(Json(ListDispatchesResponse { dispatches }))
}

async fn create_dispatch(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<NewDispatch>,
) -> ApiResult<(StatusCode, Json<Dispatch>)> {
    ensure_session_in_space(&gw, &principal, &body.from_session).await?;
    let dispatch = gw
        .dispatcher
        .dispatch_as(&principal.member_id, body)
        .await?;
    Ok((StatusCode::CREATED, Json(dispatch)))
}

// ------------------------------------------------- Space/Workspace MCP & Skills

#[derive(Deserialize, Default)]
struct HubFilterQuery {
    #[serde(default)]
    scope: Option<HubScopeLevel>,
    #[serde(default)]
    workspace_id: Option<String>,
}

async fn ensure_workspace_in_space(
    gw: &GatewayState,
    principal: &Principal,
    ws_id: &WorkspaceId,
) -> ApiResult<()> {
    let stored = gw.app.orchestrator.db().get_workspace(ws_id).await?;
    if stored.summary.space_id != principal.space_id {
        return Err(HttpApiError::from(CoreError::NotFound(format!(
            "workspace {ws_id}"
        ))));
    }
    Ok(())
}

async fn require_hub_write(
    gw: &GatewayState,
    principal: &Principal,
    scope: HubScopeLevel,
    workspace_id: Option<&WorkspaceId>,
) -> ApiResult<()> {
    match scope {
        HubScopeLevel::Space => {
            gw.auth.require(principal, Action::ManageWorkspaces)?;
        }
        HubScopeLevel::Workspace => {
            gw.auth.require(principal, Action::CreateSession)?;
            if let Some(ws) = workspace_id {
                ensure_workspace_in_space(gw, principal, ws).await?;
            }
        }
    }
    Ok(())
}

async fn list_mcp_servers(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HubFilterQuery>,
) -> ApiResult<Json<ListMcpServersResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let ws = query
        .workspace_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(WorkspaceId::from);
    if let Some(ref wid) = ws {
        ensure_workspace_in_space(&gw, &principal, wid).await?;
    }
    let is_admin = principal.can(Action::ManageWorkspaces);
    let servers = gw.hub.list_mcp(
        &principal.space_id,
        &principal.member_id,
        is_admin,
        query.scope,
        ws.as_ref(),
    );
    Ok(Json(ListMcpServersResponse { servers }))
}

async fn create_mcp_server(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<CreateMcpServerRequest>,
) -> ApiResult<(StatusCode, Json<McpServerRecord>)> {
    require_hub_write(&gw, &principal, body.scope, body.workspace_id.as_ref()).await?;
    let is_admin = principal.can(Action::ManageWorkspaces);
    let record = gw.hub.create_mcp(
        principal.space_id.clone(),
        principal.member_id.clone(),
        is_admin,
        body,
    )?;
    let _ = gw
        .app
        .orchestrator
        .db()
        .audit(
            Some(principal.member_id.clone()),
            "mcp.created",
            &record.id,
            serde_json::json!({
                "name": record.name,
                "scope": record.scope,
                "workspace_id": record.workspace_id,
                "owner_member_id": record.owner_member_id,
                "global_available": record.global_available,
                "transport": record.transport,
            }),
        )
        .await;
    Ok((StatusCode::CREATED, Json(record)))
}

async fn update_mcp_server(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMcpServerRequest>,
) -> ApiResult<Json<McpServerRecord>> {
    // Check baseline write capability before existence lookup (prevents Viewer ID oracle).
    gw.auth.require(&principal, Action::CreateSession)?;
    let is_admin = principal.can(Action::ManageWorkspaces);
    let existing = gw
        .hub
        .get_mcp(&principal.space_id, &id, &principal.member_id, is_admin)?;
    require_hub_write(
        &gw,
        &principal,
        existing.scope,
        existing.workspace_id.as_ref(),
    )
    .await?;
    let updated = gw.hub.update_mcp(
        &principal.space_id,
        &id,
        &principal.member_id,
        is_admin,
        body,
    )?;
    let _ = gw
        .app
        .orchestrator
        .db()
        .audit(
            Some(principal.member_id.clone()),
            "mcp.updated",
            &updated.id,
            serde_json::json!({
                "name": updated.name,
                "enabled": updated.enabled,
                "global_available": updated.global_available,
            }),
        )
        .await;
    Ok(Json(updated))
}

async fn delete_mcp_server(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let is_admin = principal.can(Action::ManageWorkspaces);
    let existing = if is_admin {
        gw.hub.get_mcp_for_admin(&principal.space_id, &id)?
    } else {
        gw.hub
            .get_mcp(&principal.space_id, &id, &principal.member_id, false)?
    };
    require_hub_write(
        &gw,
        &principal,
        existing.scope,
        existing.workspace_id.as_ref(),
    )
    .await?;
    let removed = gw
        .hub
        .delete_mcp(&principal.space_id, &id, &principal.member_id, is_admin)?;
    let _ = gw
        .app
        .orchestrator
        .db()
        .audit(
            Some(principal.member_id.clone()),
            "mcp.deleted",
            &removed.id,
            serde_json::json!({ "name": removed.name }),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn probe_mcp_server(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<McpProbeResult>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let is_admin = principal.can(Action::ManageWorkspaces);
    let result = gw
        .hub
        .probe_mcp(&principal.space_id, &id, &principal.member_id, is_admin)?;
    Ok(Json(result))
}

async fn list_skills(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HubFilterQuery>,
) -> ApiResult<Json<ListSkillsResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let ws = query
        .workspace_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(WorkspaceId::from);
    if let Some(ref wid) = ws {
        ensure_workspace_in_space(&gw, &principal, wid).await?;
    }
    let skills = gw
        .hub
        .list_skills(&principal.space_id, query.scope, ws.as_ref());
    Ok(Json(ListSkillsResponse { skills }))
}

async fn create_skill(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<CreateSkillRequest>,
) -> ApiResult<(StatusCode, Json<SkillRecord>)> {
    require_hub_write(&gw, &principal, body.scope, body.workspace_id.as_ref()).await?;
    let record = gw.hub.create_skill(
        principal.space_id.clone(),
        principal.member_id.clone(),
        body,
    )?;
    let _ = gw
        .app
        .orchestrator
        .db()
        .audit(
            Some(principal.member_id.clone()),
            "skill.created",
            &record.id,
            serde_json::json!({
                "name": record.name,
                "version": record.version,
                "scope": record.scope,
                "lifecycle": record.lifecycle,
            }),
        )
        .await;
    Ok((StatusCode::CREATED, Json(record)))
}

async fn update_skill(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSkillRequest>,
) -> ApiResult<Json<SkillRecord>> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let existing = gw.hub.get_skill(&principal.space_id, &id)?;
    require_hub_write(
        &gw,
        &principal,
        existing.scope,
        existing.workspace_id.as_ref(),
    )
    .await?;
    let updated = gw.hub.update_skill(&principal.space_id, &id, body)?;
    let _ = gw
        .app
        .orchestrator
        .db()
        .audit(
            Some(principal.member_id.clone()),
            "skill.updated",
            &updated.id,
            serde_json::json!({
                "name": updated.name,
                "version": updated.version,
                "lifecycle": updated.lifecycle,
                "enabled": updated.enabled,
            }),
        )
        .await;
    Ok(Json(updated))
}

async fn delete_skill(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    gw.auth.require(&principal, Action::CreateSession)?;
    let existing = gw.hub.get_skill(&principal.space_id, &id)?;
    require_hub_write(
        &gw,
        &principal,
        existing.scope,
        existing.workspace_id.as_ref(),
    )
    .await?;
    let removed = gw.hub.delete_skill(&principal.space_id, &id)?;
    let _ = gw
        .app
        .orchestrator
        .db()
        .audit(
            Some(principal.member_id.clone()),
            "skill.deleted",
            &removed.id,
            serde_json::json!({ "name": removed.name }),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_effective_hub(
    State(gw): State<GatewayState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HubFilterQuery>,
) -> ApiResult<Json<EffectiveHubResponse>> {
    gw.auth.require(&principal, Action::ViewSessions)?;
    let ws = query
        .workspace_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(WorkspaceId::from);
    if let Some(ref wid) = ws {
        ensure_workspace_in_space(&gw, &principal, wid).await?;
    }
    let resolved = gw
        .hub
        .effective(&principal.space_id, &principal.member_id, ws.as_ref());
    Ok(Json(resolved))
}
