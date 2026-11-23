//! Axum HTTP REST + `/ws/events` + static SPA gateway (W0-5 + Phase 1/W2-3 routes),
//! wired directly to [`crate::orchestrator::Orchestrator`], [`crate::workspace::WorkspaceManager`],
//! and [`crate::db::Db`].

pub mod auth;
pub mod ws;

use crate::harness::{HarnessDescriptor, HarnessProbeReport, probe_registered_harnesses};
use crate::orchestrator::Orchestrator;
use auth::verify_rest_auth;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use om_core::CoreError;
use om_core::api::{
    ApiError, ApiErrorCode, CreateSessionRequest, ListApprovalsResponse, ListEventsQuery,
    ListEventsResponse, ListSessionsResponse, MAX_EVENTS_PAGE, ResolveApprovalRequest,
    SubmitPromptResponse,
};
use om_core::approval::ApprovalOptionKind;
use om_core::content::Prompt;
use om_core::ids::{ApprovalId, MemberId, QueuedPromptId, SessionId, SpaceId, WorkspaceId};
use om_core::proxy::ProviderInfo;
use om_core::queue::{QueuedPrompt, SubmitMode};
use om_core::workspace::{WorkspaceSummary, WorktreeDiff, WorktreeEntry, WorktreeInfo};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Gateway configuration.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Static Bearer token required by Phase 0/1 authentication.
    pub auth_token: String,
    /// Active default space id.
    pub default_space_id: SpaceId,
    /// Optional compiled frontend directory (e.g. `apps/web/dist`) served with SPA fallback.
    pub static_dir: Option<PathBuf>,
    /// Optional CORS allowed origin (e.g. `http://127.0.0.1:5173` during Vite dev).
    pub allow_cors_origin: Option<String>,
}

/// Shared Axum router state.
#[derive(Clone)]
pub struct AppState {
    pub orchestrator: Orchestrator,
    pub config: ApiConfig,
}

/// Wrapper translating [`CoreError`] into the canonical [`ApiError`] JSON HTTP response.
pub struct HttpApiError(pub ApiError);

impl From<CoreError> for HttpApiError {
    fn from(err: CoreError) -> Self {
        Self(ApiError::from(&err))
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

/// Builds the complete Axum router (public `/healthz`, protected `/api/v1/*` and `/ws/events`,
/// and optional static SPA fallback).
pub fn build_router(state: AppState) -> Router {
    let api_routes = Router::new()
        // Harnesses & key proxy providers
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
        // Approvals, remembered rules & audit (W0-5 / W1-3)
        .route("/sessions/{id}/approvals", get(list_approvals))
        .route(
            "/sessions/{id}/approval-rules",
            get(list_approval_rules).delete(clear_approval_rules),
        )
        .route("/approvals/{id}/resolve", post(resolve_approval))
        .route("/audit", get(list_audit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer_token,
        ));

    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws/events", get(ws::ws_events_handler))
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

    router.layer(trace).with_state(state)
}

async fn require_bearer_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !verify_rest_auth(req.headers(), &state.config.auth_token) {
        return HttpApiError(ApiError {
            code: ApiErrorCode::Unauthorized,
            message: "invalid or missing Authorization Bearer token".into(),
        })
        .into_response();
    }
    next.run(req).await
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

// ------------------------------------------------------------------ harnesses

#[derive(Serialize)]
struct ListHarnessesResponse {
    harnesses: Vec<HarnessDescriptor>,
}

async fn list_harnesses(State(state): State<AppState>) -> Json<ListHarnessesResponse> {
    Json(ListHarnessesResponse {
        harnesses: state.orchestrator.registry().list(),
    })
}

#[derive(Serialize)]
struct ProbeHarnessesResponse {
    probes: Vec<HarnessProbeReport>,
}

async fn probe_harnesses(State(state): State<AppState>) -> Json<ProbeHarnessesResponse> {
    let probes = probe_registered_harnesses(state.orchestrator.registry()).await;
    Json(ProbeHarnessesResponse { probes })
}

#[derive(Serialize)]
struct ListProvidersResponse {
    providers: Vec<ProviderInfo>,
}

async fn list_providers(State(state): State<AppState>) -> Json<ListProvidersResponse> {
    let providers = state
        .orchestrator
        .key_proxy()
        .map(|p| p.providers())
        .unwrap_or_default();
    Json(ListProvidersResponse { providers })
}

// ----------------------------------------------------------------- workspaces

fn require_workspaces(
    state: &AppState,
) -> Result<&crate::workspace::WorkspaceManager, HttpApiError> {
    state.orchestrator.workspaces().ok_or_else(|| {
        HttpApiError::from(CoreError::Unsupported {
            harness: "server".into(),
            capability: "workspaces (not configured on this server)",
        })
    })
}

#[derive(Serialize)]
struct ListWorkspacesResponse {
    workspaces: Vec<WorkspaceSummary>,
}

async fn list_workspaces(State(state): State<AppState>) -> ApiResult<Json<ListWorkspacesResponse>> {
    let mgr = require_workspaces(&state)?;
    let workspaces = mgr.list(&state.config.default_space_id).await?;
    Ok(Json(ListWorkspacesResponse { workspaces }))
}

#[derive(Deserialize)]
struct RegisterWorkspaceRequest {
    name: String,
    repo_path: String,
}

async fn register_workspace(
    State(state): State<AppState>,
    Json(body): Json<RegisterWorkspaceRequest>,
) -> ApiResult<(StatusCode, Json<WorkspaceSummary>)> {
    let mgr = require_workspaces(&state)?;
    let summary = mgr
        .register(
            state.config.default_space_id.clone(),
            &body.name,
            &body.repo_path,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn get_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorkspaceSummary>> {
    let mgr = require_workspaces(&state)?;
    let summary = mgr.get(&WorkspaceId::from(id)).await?;
    Ok(Json(summary))
}

async fn get_worktree(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorktreeInfo>> {
    let _ = require_workspaces(&state)?;
    let stored = state
        .orchestrator
        .db()
        .get_worktree(&SessionId::from(id.clone()))
        .await?
        .ok_or_else(|| HttpApiError::from(CoreError::NotFound(format!("worktree {id}"))))?;
    Ok(Json(stored.info))
}

async fn remove_worktree(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let mgr = require_workspaces(&state)?;
    mgr.remove_worktree(&SessionId::from(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct ListWorktreeFilesResponse {
    entries: Vec<WorktreeEntry>,
}

async fn list_worktree_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListWorktreeFilesResponse>> {
    let mgr = require_workspaces(&state)?;
    let entries = mgr.list_files(&SessionId::from(id)).await?;
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
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ReadFileQuery>,
) -> ApiResult<Json<ReadWorktreeFileResponse>> {
    let mgr = require_workspaces(&state)?;
    let bytes = mgr.read_file(&SessionId::from(id), &query.path).await?;
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
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorktreeDiff>> {
    let mgr = require_workspaces(&state)?;
    let diff = mgr.diff(&SessionId::from(id)).await?;
    Ok(Json(diff))
}

// ------------------------------------------------------------------- sessions

async fn list_sessions(State(state): State<AppState>) -> ApiResult<Json<ListSessionsResponse>> {
    let sessions = state
        .orchestrator
        .db()
        .list_sessions(&state.config.default_space_id)
        .await?;
    Ok(Json(ListSessionsResponse { sessions }))
}

async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> ApiResult<(StatusCode, Json<om_core::session::SessionSummary>)> {
    let summary = state
        .orchestrator
        .create_session(state.config.default_space_id.clone(), body.config, None)
        .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<om_core::session::SessionSummary>> {
    let summary = state
        .orchestrator
        .db()
        .get_session(&SessionId::from(id))
        .await?;
    Ok(Json(summary))
}

async fn resume_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<om_core::session::SessionSummary>> {
    let summary = state
        .orchestrator
        .resume_session(&SessionId::from(id))
        .await?;
    Ok(Json(summary))
}

async fn close_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .orchestrator
        .close_session(&SessionId::from(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ListEventsQuery>,
) -> ApiResult<Json<ListEventsResponse>> {
    let limit = query
        .limit
        .unwrap_or(MAX_EVENTS_PAGE)
        .clamp(1, MAX_EVENTS_PAGE);
    let page = state
        .orchestrator
        .db()
        .list_events(&SessionId::from(id), query.after_seq, limit)
        .await?;
    Ok(Json(ListEventsResponse {
        events: page.events,
        head_seq: page.head_seq,
        has_more: page.has_more,
    }))
}

#[derive(Deserialize)]
struct SubmitPromptPayload {
    prompt: Prompt,
    /// Optional W2-3 submission mode (`queue`, `next`, `steer`). When omitted, preserves
    /// strict Phase 0 `submit_prompt` semantics (`409 Conflict` if busy).
    #[serde(default)]
    mode: Option<SubmitMode>,
}

async fn submit_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SubmitPromptPayload>,
) -> ApiResult<Response> {
    let sid = SessionId::from(id);
    if let Some(mode) = body.mode {
        let outcome = state
            .orchestrator
            .send_prompt(&sid, None, body.prompt, mode)
            .await?;
        return Ok((StatusCode::ACCEPTED, Json(outcome)).into_response());
    }
    let turn_id = state
        .orchestrator
        .submit_prompt(&sid, None, body.prompt)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(SubmitPromptResponse { turn_id })).into_response())
}

async fn cancel_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.orchestrator.cancel_turn(&SessionId::from(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------- queue & steer

#[derive(Serialize)]
struct ListQueueResponse {
    items: Vec<QueuedPrompt>,
}

async fn list_queue(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListQueueResponse>> {
    let items = state.orchestrator.queue(&SessionId::from(id)).await?;
    Ok(Json(ListQueueResponse { items }))
}

async fn withdraw_queued(
    State(state): State<AppState>,
    Path((id, qid)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    state
        .orchestrator
        .withdraw_queued(&SessionId::from(id), QueuedPromptId::from(qid), None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MoveQueuedRequest {
    position: usize,
}

async fn move_queued(
    State(state): State<AppState>,
    Path((id, qid)): Path<(String, String)>,
    Json(body): Json<MoveQueuedRequest>,
) -> ApiResult<Json<ListQueueResponse>> {
    let items = state
        .orchestrator
        .move_queued(
            &SessionId::from(id),
            QueuedPromptId::from(qid),
            body.position,
        )
        .await?;
    Ok(Json(ListQueueResponse { items }))
}

async fn resume_queue(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .orchestrator
        .resume_queue(&SessionId::from(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------- approvals & audit

#[derive(Debug, Default, Deserialize)]
struct ListApprovalsQuery {
    #[serde(default)]
    pending_only: bool,
}

async fn list_approvals(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ListApprovalsQuery>,
) -> ApiResult<Json<ListApprovalsResponse>> {
    let stored = state
        .orchestrator
        .db()
        .list_approvals(&SessionId::from(id), query.pending_only)
        .await?;
    let approvals = stored.into_iter().map(|s| s.record).collect();
    Ok(Json(ListApprovalsResponse { approvals }))
}

async fn resolve_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ResolveApprovalRequest>,
) -> ApiResult<Json<om_core::approval::ApprovalRecord>> {
    let record = state
        .orchestrator
        .resolve_approval(&ApprovalId::from(id), body.decision, None)
        .await?;
    Ok(Json(record))
}

#[derive(Serialize)]
struct ApprovalRuleView {
    rule_key: String,
    allow: bool,
    option_kind: ApprovalOptionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<MemberId>,
    created_at: i64,
}

#[derive(Serialize)]
struct ListApprovalRulesResponse {
    rules: Vec<ApprovalRuleView>,
}

async fn list_approval_rules(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListApprovalRulesResponse>> {
    let sid = SessionId::from(id);
    // Ensure session exists before listing rules.
    state.orchestrator.db().get_session(&sid).await?;
    let rules = state
        .orchestrator
        .db()
        .approval_rules(&sid)
        .await?
        .into_iter()
        .map(|r| ApprovalRuleView {
            rule_key: r.rule_key,
            allow: r.allow,
            option_kind: r.option_kind,
            created_by: r.created_by,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(ListApprovalRulesResponse { rules }))
}

#[derive(Serialize)]
struct ClearApprovalRulesResponse {
    cleared: usize,
}

async fn clear_approval_rules(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ClearApprovalRulesResponse>> {
    let sid = SessionId::from(id);
    state.orchestrator.db().get_session(&sid).await?;
    let cleared = state
        .orchestrator
        .db()
        .clear_approval_rules(&sid, None)
        .await?;
    Ok(Json(ClearApprovalRulesResponse { cleared }))
}

#[derive(Debug, Default, Deserialize)]
struct ListAuditQuery {
    target: Option<String>,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct AuditEntryView {
    id: i64,
    ts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<MemberId>,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    detail: serde_json::Value,
}

#[derive(Serialize)]
struct ListAuditResponse {
    entries: Vec<AuditEntryView>,
}

async fn list_audit(
    State(state): State<AppState>,
    Query(query): Query<ListAuditQuery>,
) -> ApiResult<Json<ListAuditResponse>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let entries = state
        .orchestrator
        .db()
        .list_audit(&state.config.default_space_id, query.target, limit)
        .await?
        .into_iter()
        .map(|e| AuditEntryView {
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
