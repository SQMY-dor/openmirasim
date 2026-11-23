//! Axum HTTP REST + `/ws/events` + static SPA gateway (W0-5), wired directly to
//! [`crate::orchestrator::Orchestrator`] and [`crate::db::Db`].

pub mod auth;
pub mod ws;

use crate::orchestrator::Orchestrator;
use auth::verify_rest_auth;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use om_core::CoreError;
use om_core::api::{
    ApiError, ApiErrorCode, CreateSessionRequest, ListApprovalsResponse, ListEventsQuery,
    ListEventsResponse, ListSessionsResponse, MAX_EVENTS_PAGE, ResolveApprovalRequest,
    SubmitPromptRequest, SubmitPromptResponse,
};
use om_core::ids::{ApprovalId, SessionId, SpaceId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Gateway configuration.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Static Bearer token required by Phase 0 authentication.
    pub auth_token: String,
    /// Active Phase 0 default space id.
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
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(close_session))
        .route("/sessions/{id}/resume", post(resume_session))
        .route("/sessions/{id}/events", get(list_events))
        .route("/sessions/{id}/prompts", post(submit_prompt))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/approvals", get(list_approvals))
        .route("/approvals/{id}/resolve", post(resolve_approval))
        .route("/harnesses", get(list_harnesses))
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

async fn submit_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SubmitPromptRequest>,
) -> ApiResult<(StatusCode, Json<SubmitPromptResponse>)> {
    let turn_id = state
        .orchestrator
        .submit_prompt(&SessionId::from(id), None, body.prompt)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(SubmitPromptResponse { turn_id })))
}

async fn cancel_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.orchestrator.cancel_turn(&SessionId::from(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

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
struct ListHarnessesResponse {
    harnesses: Vec<crate::harness::HarnessDescriptor>,
}

async fn list_harnesses(State(state): State<AppState>) -> Json<ListHarnessesResponse> {
    Json(ListHarnessesResponse {
        harnesses: state.orchestrator.registry().list(),
    })
}
