//! Independent Binary WebSocket `/ws/pty/{session_id}` gateway (Phase 3).
//!
//! Kept strictly separate from `/ws/events` so heavy terminal output never blocks structured
//! chat or approval streams.
//!
//! Wire protocol:
//! - **Server -> Client Binary**: raw terminal bytes (initial ring-buffer snapshot + live chunks).
//! - **Client -> Server Binary**: raw stdin bytes (permitted for `Member+`; ignored for `Viewer`).
//! - **Text frames**: JSON control frames:
//!   - Client -> Server: `{"type":"resize","cols":120,"rows":36}`
//!   - Server -> Client: `{"type":"ready","cols":...,"rows":...,"writable":bool,"recording":bool}`,
//!     `{"type":"lagged","skipped":n}`, `{"type":"exited","code":...}`, `{"type":"error","message":...}`.

use super::GatewayState;
use super::auth::{authenticate_secret, extract_ws_token};
use super::validate_ws_origin;
use crate::auth::Principal;
use crate::pty::PtyOutput;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use om_core::api::{ApiError, ApiErrorCode};
use om_core::auth::Action;
use om_core::ids::SessionId;
use om_core::session::SessionStatus;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Bounded per-client outbound frame queue for backpressure isolation.
const PTY_CLIENT_QUEUE_CAPACITY: usize = 128;

/// Maximum inbound WebSocket message payload accepted on `/ws/pty/{id}` (256 KiB).
const MAX_PTY_WS_MESSAGE_BYTES: usize = 256 * 1024;

/// Periodic re-validation cadence for long-lived PTY WebSockets.
const AUTH_REVALIDATE_INTERVAL: Duration = Duration::from_secs(30);

/// Minimum interval between DB auth checks on stdin/resize frames.
const WRITE_REVALIDATE_THROTTLE: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
pub struct PtyWsQuery {
    pub token: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientPtyControl {
    Resize { cols: u16, rows: u16 },
    Ping { nonce: u64 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerPtyControl {
    Ready {
        cols: u16,
        rows: u16,
        writable: bool,
        recording: bool,
    },
    Lagged {
        skipped: u64,
    },
    Exited {
        code: Option<u32>,
    },
    Pong {
        nonce: u64,
    },
    Error {
        message: String,
    },
}

enum OutboundFrame {
    Binary(Bytes),
    Control(ServerPtyControl),
}

pub(super) async fn ws_pty_handler(
    State(gw): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PtyWsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(msg) = validate_ws_origin(&headers, gw.app.config.allow_cors_origin.as_deref()) {
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

    let principal = match authenticate_secret(
        &gw.auth,
        gw.app.orchestrator.db(),
        &gw.app.config.default_space_id,
        &gw.app.config.auth_token,
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

    if let Err(err) = gw.auth.require(&principal, Action::ViewSessions) {
        let api = ApiError::from(&err);
        let status = StatusCode::from_u16(api.code.http_status()).unwrap_or(StatusCode::FORBIDDEN);
        return (status, axum::Json(api)).into_response();
    }

    let sid = SessionId::from(id);
    let session = match gw.app.orchestrator.db().get_session(&sid).await {
        Ok(s) if s.space_id == principal.space_id => s,
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(ApiError {
                    code: ApiErrorCode::NotFound,
                    message: format!("session {sid} not found"),
                }),
            )
                .into_response();
        }
        Err(err) => {
            let api = ApiError::from(&err);
            let status =
                StatusCode::from_u16(api.code.http_status()).unwrap_or(StatusCode::NOT_FOUND);
            return (status, axum::Json(api)).into_response();
        }
    };

    let writable = principal.can(Action::PromptSession);
    let session_ended = matches!(
        session.status,
        SessionStatus::Closed | SessionStatus::Failed
    );
    let allow_spawn = writable && !session_ended;
    let cols = query.cols.unwrap_or(100);
    let rows = query.rows.unwrap_or(30);

    let effective_cwd = match gw.app.orchestrator.db().get_worktree(&sid).await {
        Ok(Some(wt)) if wt.info.removed_at.is_some() => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(ApiError {
                    code: ApiErrorCode::InvalidInput,
                    message: "the session's worktree has been removed".into(),
                }),
            )
                .into_response();
        }
        Ok(Some(wt)) if !wt.info.path.is_empty() => wt.info.path,
        _ => session.config.cwd.clone(),
    };

    let sandbox = match gw
        .app
        .orchestrator
        .pty_sandbox(&sid, &session.config)
        .await
    {
        Ok(sb) => sb,
        Err(err) => {
            let api = ApiError::from(&err);
            let status =
                StatusCode::from_u16(api.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
            return (status, axum::Json(api)).into_response();
        }
    };

    let pty_mgr = gw.pty.clone();
    let sid_clone = sid.clone();
    let cwd_clone = effective_cwd.clone();
    let attachment = match tokio::task::spawn_blocking(move || {
        pty_mgr.attach_or_spawn_in_sandbox(&sid_clone, &cwd_clone, sandbox, cols, rows, allow_spawn)
    })
    .await
    {
        Ok(Ok(att)) => att,
        Ok(Err(err)) => {
            let api = ApiError::from(&err);
            let status =
                StatusCode::from_u16(api.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
            return (status, axum::Json(api)).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("PTY attach task failed: {e}"),
                }),
            )
                .into_response();
        }
    };

    ws.max_message_size(MAX_PTY_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_pty_socket(socket, gw, principal, attachment, writable))
        .into_response()
}

async fn handle_pty_socket(
    socket: WebSocket,
    gw: GatewayState,
    mut principal: Principal,
    attachment: crate::pty::PtyAttachment,
    mut writable: bool,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundFrame>(PTY_CLIENT_QUEUE_CAPACITY);

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let msg = match frame {
                OutboundFrame::Binary(bytes) => Message::Binary(bytes),
                OutboundFrame::Control(ctrl) => match serde_json::to_string(&ctrl) {
                    Ok(txt) => Message::Text(txt.into()),
                    Err(_) => continue,
                },
            };
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Send Ready control frame + initial scrollback catch-up snapshot.
    let _ = out_tx
        .send(OutboundFrame::Control(ServerPtyControl::Ready {
            cols: attachment.cols,
            rows: attachment.rows,
            writable,
            recording: attachment.has_recording,
        }))
        .await;
    if !attachment.snapshot.is_empty() {
        let _ = out_tx
            .send(OutboundFrame::Binary(attachment.snapshot.clone()))
            .await;
    }

    let (pty_writer, mut rx) = attachment.split();
    let out_tx_pump = out_tx.clone();
    let pump = tokio::spawn(async move {
        let mut dropped_local: u64 = 0;
        loop {
            match rx.recv().await {
                Ok(PtyOutput::Chunk(bytes)) => {
                    if dropped_local > 0
                        && out_tx_pump
                            .try_send(OutboundFrame::Control(ServerPtyControl::Lagged {
                                skipped: dropped_local,
                            }))
                            .is_ok()
                    {
                        dropped_local = 0;
                    }
                    // Bounded backpressure: drop chunk cleanly for slow client and report lag.
                    match out_tx_pump.try_send(OutboundFrame::Binary(bytes)) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            dropped_local = dropped_local.saturating_add(1);
                        }
                        Err(TrySendError::Closed(_)) => break,
                    }
                }
                Ok(PtyOutput::Exited(code)) => {
                    if dropped_local > 0 {
                        let _ = out_tx_pump
                            .send(OutboundFrame::Control(ServerPtyControl::Lagged {
                                skipped: dropped_local,
                            }))
                            .await;
                    }
                    let _ = out_tx_pump
                        .send(OutboundFrame::Control(ServerPtyControl::Exited { code }))
                        .await;
                    break;
                }
                Err(RecvError::Lagged(skipped)) => {
                    dropped_local = dropped_local.saturating_add(skipped);
                    if out_tx_pump
                        .try_send(OutboundFrame::Control(ServerPtyControl::Lagged {
                            skipped: dropped_local,
                        }))
                        .is_ok()
                    {
                        dropped_local = 0;
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    let mut revalidate_tick = tokio::time::interval(AUTH_REVALIDATE_INTERVAL);
    revalidate_tick.tick().await;
    let mut last_auth_check = Instant::now();

    loop {
        tokio::select! {
            _ = revalidate_tick.tick() => {
                match gw.auth.verify_principal(&principal).await {
                    Ok(fresh) => {
                        principal = fresh;
                        writable = principal.can(Action::PromptSession);
                        last_auth_check = Instant::now();
                    }
                    Err(e) => {
                        let _ = out_tx
                            .send(OutboundFrame::Control(ServerPtyControl::Error {
                                message: format!("session authentication expired: {e}"),
                            }))
                            .await;
                        break;
                    }
                }
            }
            next = ws_rx.next() => {
                let Some(Ok(frame)) = next else {
                    break;
                };
                match frame {
                    Message::Binary(bytes) => {
                        if last_auth_check.elapsed() >= WRITE_REVALIDATE_THROTTLE {
                            match gw.auth.verify_principal(&principal).await {
                                Ok(fresh) => {
                                    principal = fresh;
                                    writable = principal.can(Action::PromptSession);
                                    last_auth_check = Instant::now();
                                }
                                Err(e) => {
                                    let _ = out_tx
                                        .send(OutboundFrame::Control(ServerPtyControl::Error {
                                            message: format!("session authentication expired: {e}"),
                                        }))
                                        .await;
                                    break;
                                }
                            }
                        }
                        if writable {
                            if pty_writer.write_input(bytes).await.is_err() {
                                break;
                            }
                        } else {
                            let _ = out_tx.try_send(OutboundFrame::Control(ServerPtyControl::Error {
                                message: "read-only PTY attachment (Viewer role)".into(),
                            }));
                        }
                    }
                    Message::Text(txt) => {
                        if let Ok(ctrl) = serde_json::from_str::<ClientPtyControl>(&txt) {
                            match ctrl {
                                ClientPtyControl::Resize { cols, rows } => {
                                    if last_auth_check.elapsed() >= WRITE_REVALIDATE_THROTTLE {
                                        match gw.auth.verify_principal(&principal).await {
                                            Ok(fresh) => {
                                                principal = fresh;
                                                writable = principal.can(Action::PromptSession);
                                                last_auth_check = Instant::now();
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                    if writable {
                                        let _ = pty_writer.resize(cols, rows).await;
                                    }
                                }
                                ClientPtyControl::Ping { nonce } => {
                                    let _ = out_tx
                                        .try_send(OutboundFrame::Control(ServerPtyControl::Pong { nonce }));
                                }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
        }
    }

    pump.abort();
    drop(out_tx);
    let _ = writer.await;
}
