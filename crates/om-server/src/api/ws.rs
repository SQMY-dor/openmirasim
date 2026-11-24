//! `/ws/events` resumable WebSocket handler backed directly by
//! [`crate::orchestrator::Orchestrator::subscribe`] and [`crate::orchestrator::Subscription`].
//!
//! Supports both W2-1 RBAC device tokens (`omt_...`) and static bootstrap tokens via
//! `Authorization: Bearer <token>` or `?token=<token>`. Enforces `Action::ViewSessions`,
//! space isolation, and periodic re-validation on every connection and `Subscribe` request.

use super::AppState;
use super::auth::{authenticate_secret, extract_ws_token};
use super::validate_ws_origin;
use crate::auth::Auth;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use om_core::api::{ApiError, ApiErrorCode, ClientMessage, ServerMessage};
use om_core::auth::{Action, Principal};
use om_core::ids::SessionId;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

/// Current `/ws/events` protocol version.
pub const WS_PROTOCOL_VERSION: u32 = 1;

/// Bounded per-connection outbound buffer for session event frames.
const WS_EVENT_QUEUE_CAPACITY: usize = 256;

/// Bounded per-connection outbound buffer for control frames (`Welcome`, `Subscribed`,
/// `Resync`, `Error`, `Pong`).
const WS_CONTROL_QUEUE_CAPACITY: usize = 64;

/// Maximum inbound JSON control frame size accepted on `/ws/events` (64 KiB).
const MAX_EVENTS_WS_MESSAGE_BYTES: usize = 64 * 1024;

/// Maximum simultaneous session subscriptions per WebSocket connection.
const MAX_SUBSCRIPTIONS_PER_SOCKET: usize = 64;

/// Periodic re-validation cadence for long-lived `/ws/events` WebSockets.
const AUTH_REVALIDATE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

pub async fn ws_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(msg) = validate_ws_origin(&headers, state.config.allow_cors_origin.as_deref()) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(ApiError {
                code: ApiErrorCode::Forbidden,
                message: msg,
            }),
        )
            .into_response();
    }

    let Some(token) = extract_ws_token(&headers, query.token.as_deref()) else {
        let err = ApiError {
            code: ApiErrorCode::Unauthorized,
            message: "invalid or missing access token".into(),
        };
        return (StatusCode::UNAUTHORIZED, axum::Json(err)).into_response();
    };

    let auth = Auth::new(state.orchestrator.db().clone());
    let principal = match authenticate_secret(
        &auth,
        state.orchestrator.db(),
        &state.config.default_space_id,
        &state.config.auth_token,
        token,
    )
    .await
    {
        Ok(p) => p,
        Err(err) => {
            let api = ApiError::from(&err);
            let status =
                StatusCode::from_u16(api.code.http_status()).unwrap_or(StatusCode::UNAUTHORIZED);
            return (status, axum::Json(api)).into_response();
        }
    };

    if let Err(err) = auth.require(&principal, Action::ViewSessions) {
        let api = ApiError::from(&err);
        let status = StatusCode::from_u16(api.code.http_status()).unwrap_or(StatusCode::FORBIDDEN);
        return (status, axum::Json(api)).into_response();
    }

    ws.max_message_size(MAX_EVENTS_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, auth, principal))
        .into_response()
}

async fn send_control(tx: &mpsc::Sender<ServerMessage>, msg: ServerMessage) -> bool {
    tokio::time::timeout(Duration::from_secs(2), tx.send(msg))
        .await
        .is_ok_and(|r| r.is_ok())
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    auth: Auth,
    mut principal: Principal,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ServerMessage>(WS_CONTROL_QUEUE_CAPACITY);
    let (evt_tx, mut evt_rx) = mpsc::channel::<ServerMessage>(WS_EVENT_QUEUE_CAPACITY);
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<()>(1);

    let writer = tokio::spawn(async move {
        loop {
            let next_msg = tokio::select! {
                biased;
                msg = ctrl_rx.recv() => match msg {
                    Some(m) => Some(m),
                    None => break,
                },
                msg = evt_rx.recv() => msg,
            };
            let Some(msg) = next_msg else {
                break;
            };
            let Ok(text) = serde_json::to_string(&msg) else {
                continue;
            };
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut welcomed = false;
    let mut subscriptions: HashMap<SessionId, JoinHandle<()>> = HashMap::new();
    let mut revalidate_tick = tokio::time::interval(AUTH_REVALIDATE_INTERVAL);
    revalidate_tick.tick().await;

    loop {
        let frame = tokio::select! {
            _ = fatal_rx.recv() => break,
            _ = revalidate_tick.tick() => {
                match auth.verify_principal(&principal).await {
                    Ok(fresh) => {
                        principal = fresh;
                        continue;
                    }
                    Err(err) => {
                        let _ = send_control(
                            &ctrl_tx,
                            ServerMessage::Error {
                                session_id: None,
                                error: ApiError::from(&err),
                            },
                        )
                        .await;
                        break;
                    }
                }
            }
            next = ws_rx.next() => match next {
                Some(Ok(f)) => f,
                _ => break,
            }
        };

        match frame {
            Message::Text(text) => {
                let msg: ClientMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        if !send_control(
                            &ctrl_tx,
                            ServerMessage::Error {
                                session_id: None,
                                error: ApiError {
                                    code: ApiErrorCode::InvalidInput,
                                    message: format!("invalid client message: {e}"),
                                },
                            },
                        )
                        .await
                        {
                            break;
                        }
                        continue;
                    }
                };

                match msg {
                    ClientMessage::Hello { protocol_version } => {
                        if protocol_version != WS_PROTOCOL_VERSION {
                            let _ = send_control(
                                &ctrl_tx,
                                ServerMessage::Error {
                                    session_id: None,
                                    error: ApiError {
                                        code: ApiErrorCode::Unsupported,
                                        message: format!(
                                            "unsupported protocol_version {protocol_version} (expected {WS_PROTOCOL_VERSION})"
                                        ),
                                    },
                                },
                            )
                            .await;
                            break;
                        }
                        welcomed = true;
                        if !send_control(
                            &ctrl_tx,
                            ServerMessage::Welcome {
                                protocol_version: WS_PROTOCOL_VERSION,
                                server_version: env!("CARGO_PKG_VERSION").to_string(),
                            },
                        )
                        .await
                        {
                            break;
                        }
                    }
                    ClientMessage::Ping { nonce } => {
                        if !send_control(&ctrl_tx, ServerMessage::Pong { nonce }).await {
                            break;
                        }
                    }
                    ClientMessage::Subscribe {
                        session_id,
                        last_seq,
                    } => {
                        if !welcomed {
                            if !send_control(
                                &ctrl_tx,
                                ServerMessage::Error {
                                    session_id: Some(session_id),
                                    error: ApiError {
                                        code: ApiErrorCode::InvalidInput,
                                        message: "must send Hello before Subscribe".into(),
                                    },
                                },
                            )
                            .await
                            {
                                break;
                            }
                            continue;
                        }

                        // Re-verify principal liveness before granting new subscriptions.
                        match auth.verify_principal(&principal).await {
                            Ok(fresh) => principal = fresh,
                            Err(err) => {
                                let _ = send_control(
                                    &ctrl_tx,
                                    ServerMessage::Error {
                                        session_id: Some(session_id),
                                        error: ApiError::from(&err),
                                    },
                                )
                                .await;
                                break;
                            }
                        }

                        // Enforce space isolation: sessions from another space appear as NotFound.
                        match state.orchestrator.db().get_session(&session_id).await {
                            Ok(summary) if summary.space_id == principal.space_id => {}
                            Ok(_) => {
                                if !send_control(
                                    &ctrl_tx,
                                    ServerMessage::Error {
                                        session_id: Some(session_id.clone()),
                                        error: ApiError {
                                            code: ApiErrorCode::NotFound,
                                            message: format!("session {session_id} not found"),
                                        },
                                    },
                                )
                                .await
                                {
                                    break;
                                }
                                continue;
                            }
                            Err(err) => {
                                if !send_control(
                                    &ctrl_tx,
                                    ServerMessage::Error {
                                        session_id: Some(session_id),
                                        error: ApiError::from(&err),
                                    },
                                )
                                .await
                                {
                                    break;
                                }
                                continue;
                            }
                        }

                        if !subscriptions.contains_key(&session_id)
                            && subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_SOCKET
                        {
                            if !send_control(
                                &ctrl_tx,
                                ServerMessage::Error {
                                    session_id: Some(session_id),
                                    error: ApiError {
                                        code: ApiErrorCode::LimitExceeded,
                                        message: format!(
                                            "too many concurrent subscriptions (max {MAX_SUBSCRIPTIONS_PER_SOCKET})"
                                        ),
                                    },
                                },
                            )
                            .await
                            {
                                break;
                            }
                            continue;
                        }

                        if let Some(prev) = subscriptions.remove(&session_id) {
                            prev.abort();
                        }

                        match state.orchestrator.subscribe(&session_id, last_seq).await {
                            Ok(mut sub) => {
                                let head_seq = sub.head_seq();
                                if !send_control(
                                    &ctrl_tx,
                                    ServerMessage::Subscribed {
                                        session_id: session_id.clone(),
                                        head_seq,
                                    },
                                )
                                .await
                                {
                                    break;
                                }

                                let ev_tx = evt_tx.clone();
                                let ct_tx = ctrl_tx.clone();
                                let ft_tx = fatal_tx.clone();
                                let sid = session_id.clone();
                                let handle = tokio::spawn(async move {
                                    loop {
                                        match sub.next().await {
                                            Ok(Some(event)) => {
                                                match ev_tx.try_send(ServerMessage::Event {
                                                    event: Box::new(event),
                                                }) {
                                                    Ok(()) => {}
                                                    Err(TrySendError::Full(_)) => {
                                                        tracing::debug!(
                                                            session = %sid,
                                                            "slow WS subscriber queue full; requesting resync"
                                                        );
                                                        if !send_control(
                                                            &ct_tx,
                                                            ServerMessage::Resync {
                                                                session_id: sid,
                                                            },
                                                        )
                                                        .await
                                                        {
                                                            let _ = ft_tx.try_send(());
                                                        }
                                                        break;
                                                    }
                                                    Err(TrySendError::Closed(_)) => break,
                                                }
                                            }
                                            Ok(None) => break,
                                            Err(err) => {
                                                if !send_control(
                                                    &ct_tx,
                                                    ServerMessage::Error {
                                                        session_id: Some(sid),
                                                        error: ApiError::from(&err),
                                                    },
                                                )
                                                .await
                                                {
                                                    let _ = ft_tx.try_send(());
                                                }
                                                break;
                                            }
                                        }
                                    }
                                });
                                subscriptions.insert(session_id, handle);
                            }
                            Err(err) => {
                                if !send_control(
                                    &ctrl_tx,
                                    ServerMessage::Error {
                                        session_id: Some(session_id),
                                        error: ApiError::from(&err),
                                    },
                                )
                                .await
                                {
                                    break;
                                }
                            }
                        }
                    }
                    ClientMessage::Unsubscribe { session_id } => {
                        if let Some(prev) = subscriptions.remove(&session_id) {
                            prev.abort();
                        }
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
        }
    }

    for (_, handle) in subscriptions.drain() {
        handle.abort();
    }
    drop(evt_tx);
    drop(ctrl_tx);
    let _ = writer.await;
}
