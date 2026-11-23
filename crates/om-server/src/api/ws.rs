//! `/ws/events` resumable WebSocket handler backed directly by
//! [`crate::orchestrator::Orchestrator::subscribe`] and [`crate::orchestrator::Subscription`].
//!
//! Protocol:
//! 1. Client sends `Hello { protocol_version: 1 }` -> server replies `Welcome`.
//! 2. Client sends `Subscribe { session_id, last_seq }`:
//!    - Server opens a gapless [`crate::orchestrator::Subscription`] from `last_seq`.
//!    - Sends `Subscribed { session_id, head_seq: sub.head_seq() }`.
//!    - Streams every event with `seq > last_seq` via `sub.next().await` (backlog + live,
//!      self-healing from SQLite on lag).
//! 3. Client may send `Unsubscribe { session_id }` or `Ping { nonce }` at any time.

use super::AppState;
use super::auth::verify_ws_auth;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use om_core::api::{ApiError, ApiErrorCode, ClientMessage, ServerMessage};
use om_core::ids::SessionId;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Current `/ws/events` protocol version.
pub const WS_PROTOCOL_VERSION: u32 = 1;

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
    if !verify_ws_auth(&headers, query.token.as_deref(), &state.config.auth_token) {
        let err = ApiError {
            code: ApiErrorCode::Unauthorized,
            message: "invalid or missing access token".into(),
        };
        return (StatusCode::UNAUTHORIZED, axum::Json(err)).into_response();
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
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

    while let Some(Ok(frame)) = ws_rx.next().await {
        match frame {
            Message::Text(text) => {
                let msg: ClientMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = out_tx.send(ServerMessage::Error {
                            session_id: None,
                            error: ApiError {
                                code: ApiErrorCode::InvalidInput,
                                message: format!("invalid client message: {e}"),
                            },
                        });
                        continue;
                    }
                };

                match msg {
                    ClientMessage::Hello { protocol_version } => {
                        if protocol_version != WS_PROTOCOL_VERSION {
                            let _ = out_tx.send(ServerMessage::Error {
                                session_id: None,
                                error: ApiError {
                                    code: ApiErrorCode::Unsupported,
                                    message: format!(
                                        "unsupported protocol_version {protocol_version} (expected {WS_PROTOCOL_VERSION})"
                                    ),
                                },
                            });
                            break;
                        }
                        welcomed = true;
                        let _ = out_tx.send(ServerMessage::Welcome {
                            protocol_version: WS_PROTOCOL_VERSION,
                            server_version: env!("CARGO_PKG_VERSION").to_string(),
                        });
                    }
                    ClientMessage::Ping { nonce } => {
                        let _ = out_tx.send(ServerMessage::Pong { nonce });
                    }
                    ClientMessage::Subscribe {
                        session_id,
                        last_seq,
                    } => {
                        if !welcomed {
                            let _ = out_tx.send(ServerMessage::Error {
                                session_id: Some(session_id),
                                error: ApiError {
                                    code: ApiErrorCode::InvalidInput,
                                    message: "must send Hello before Subscribe".into(),
                                },
                            });
                            continue;
                        }

                        if let Some(prev) = subscriptions.remove(&session_id) {
                            prev.abort();
                        }

                        match state.orchestrator.subscribe(&session_id, last_seq).await {
                            Ok(mut sub) => {
                                let head_seq = sub.head_seq();
                                if out_tx
                                    .send(ServerMessage::Subscribed {
                                        session_id: session_id.clone(),
                                        head_seq,
                                    })
                                    .is_err()
                                {
                                    break;
                                }

                                let tx = out_tx.clone();
                                let sid = session_id.clone();
                                let handle = tokio::spawn(async move {
                                    loop {
                                        match sub.next().await {
                                            Ok(Some(event)) => {
                                                if tx
                                                    .send(ServerMessage::Event {
                                                        event: Box::new(event),
                                                    })
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                            Ok(None) => break,
                                            Err(err) => {
                                                let _ = tx.send(ServerMessage::Error {
                                                    session_id: Some(sid),
                                                    error: ApiError::from(&err),
                                                });
                                                break;
                                            }
                                        }
                                    }
                                });
                                subscriptions.insert(session_id, handle);
                            }
                            Err(err) => {
                                let _ = out_tx.send(ServerMessage::Error {
                                    session_id: Some(session_id),
                                    error: ApiError::from(&err),
                                });
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
    drop(out_tx);
    let _ = writer.await;
}
