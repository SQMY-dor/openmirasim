//! Minimal JSON-RPC 2.0 peer over newline-delimited stdio (the ACP stdio transport).
//!
//! Both sides may send requests, so the connection tracks outgoing requests by id and
//! surfaces incoming requests / notifications on a channel for the harness to handle.

use om_core::{CoreError, CoreResult};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INTERNAL: i64 = -32603;
    /// Local, non-wire code: the peer closed the connection.
    pub const CONNECTION_CLOSED: i64 = -32099;
    /// Local, non-wire code: no response within the timeout.
    pub const TIMEOUT: i64 = -32098;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn from_value(v: &Value) -> Self {
        Self {
            code: v
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(Self::INTERNAL),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned(),
            data: v.get("data").cloned(),
        }
    }

    fn to_value(&self) -> Value {
        let mut v = json!({"code": self.code, "message": self.message});
        if let Some(d) = &self.data {
            v["data"] = d.clone();
        }
        v
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl From<RpcError> for CoreError {
    fn from(e: RpcError) -> Self {
        CoreError::Harness(e.to_string())
    }
}

/// A message initiated by the peer.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The peer's output ended; no more messages follow.
    Closed,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// Handle for sending requests, notifications and responses. Cheap to clone.
#[derive(Clone)]
pub struct RpcConnection {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl RpcConnection {
    /// Starts reader and writer tasks over the given streams.
    pub fn spawn<R, W>(reader: R, writer: W) -> (Self, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();

        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = out_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err()
                    || writer.write_all(b"\n").await.is_err()
                    || writer.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(msg) => dispatch(msg, &reader_pending, &in_tx),
                    Err(_) => tracing::debug!(%line, "ignoring non-JSON line from agent"),
                }
            }
            let drained: Vec<_> = reader_pending
                .lock()
                .map(|mut p| p.drain().collect())
                .unwrap_or_default();
            for (_, tx) in drained {
                let _ = tx.send(Err(RpcError::new(
                    RpcError::CONNECTION_CLOSED,
                    "agent connection closed",
                )));
            }
            let _ = in_tx.send(Incoming::Closed);
        });

        (
            Self {
                out: out_tx,
                pending,
                next_id: Arc::new(AtomicU64::new(0)),
            },
            in_rx,
        )
    }

    fn send(&self, msg: Value) -> CoreResult<()> {
        self.out
            .send(msg.to_string())
            .map_err(|_| CoreError::Harness("agent connection closed".into()))
    }

    /// Sends a request and waits for its response.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| RpcError::new(RpcError::INTERNAL, "poisoned"))?
            .insert(id, tx);
        if self
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .is_err()
        {
            self.forget(id);
            return Err(RpcError::new(
                RpcError::CONNECTION_CLOSED,
                "agent connection closed",
            ));
        }
        let result = match timeout {
            Some(t) => match tokio::time::timeout(t, rx).await {
                Ok(r) => r,
                Err(_) => {
                    self.forget(id);
                    return Err(RpcError::new(
                        RpcError::TIMEOUT,
                        format!("`{method}` timed out after {t:?}"),
                    ));
                }
            },
            None => rx.await,
        };
        result.unwrap_or_else(|_| {
            Err(RpcError::new(
                RpcError::CONNECTION_CLOSED,
                "agent connection closed",
            ))
        })
    }

    fn forget(&self, id: u64) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&id);
        }
    }

    /// Sends a notification (no response expected).
    pub fn notify(&self, method: &str, params: Value) -> CoreResult<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// Answers a request the peer sent us.
    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) -> CoreResult<()> {
        let msg = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_value()}),
        };
        self.send(msg)
    }
}

fn dispatch(msg: Value, pending: &Pending, in_tx: &mpsc::UnboundedSender<Incoming>) {
    let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => {
            let _ = in_tx.send(Incoming::Request { id, method, params });
        }
        (Some(method), None) => {
            let _ = in_tx.send(Incoming::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(num) = id.as_u64() else {
                tracing::debug!(?id, "response with non-numeric id ignored");
                return;
            };
            let tx = pending.lock().ok().and_then(|mut p| p.remove(&num));
            if let Some(tx) = tx {
                let result = match msg.get("error") {
                    Some(err) if !err.is_null() => Err(RpcError::from_value(err)),
                    _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
        }
        (None, None) => tracing::debug!("malformed JSON-RPC message ignored"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_response_and_incoming() {
        let (client_io, agent_io) = tokio::io::duplex(4096);
        let (client_r, client_w) = tokio::io::split(client_io);
        let (agent_r, mut agent_w) = tokio::io::split(agent_io);
        let (conn, mut incoming) = RpcConnection::spawn(client_r, client_w);

        // Fake agent: answer request id 0 with an echo, then send a notification.
        tokio::spawn(async move {
            let mut lines = BufReader::new(agent_r).lines();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(req["method"], "ping");
            let resp =
                json!({"jsonrpc": "2.0", "id": req["id"], "result": {"pong": req["params"]}});
            agent_w
                .write_all(format!("{resp}\nnot json\n").as_bytes())
                .await
                .unwrap();
            let note = json!({"jsonrpc": "2.0", "method": "hello", "params": {"x": 1}});
            agent_w
                .write_all(format!("{note}\n").as_bytes())
                .await
                .unwrap();
            let call = json!({"jsonrpc": "2.0", "id": "r1", "method": "ask", "params": {}});
            agent_w
                .write_all(format!("{call}\n").as_bytes())
                .await
                .unwrap();
        });

        let r = conn
            .request("ping", json!(7), Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(r, json!({"pong": 7}));
        assert_eq!(
            incoming.recv().await.unwrap(),
            Incoming::Notification {
                method: "hello".into(),
                params: json!({"x": 1})
            }
        );
        assert!(
            matches!(incoming.recv().await.unwrap(), Incoming::Request { method, .. } if method == "ask")
        );
        // Agent task ended and dropped its writer: the connection reports closure.
        assert_eq!(incoming.recv().await.unwrap(), Incoming::Closed);
    }

    #[tokio::test]
    async fn pending_requests_fail_on_close_and_timeout() {
        let (client_io, agent_io) = tokio::io::duplex(4096);
        let (client_r, client_w) = tokio::io::split(client_io);
        let (conn, _incoming) = RpcConnection::spawn(client_r, client_w);
        let err = conn
            .request("slow", Value::Null, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        assert_eq!(err.code, RpcError::TIMEOUT);
        let pending = tokio::spawn({
            let conn = conn.clone();
            async move { conn.request("never", Value::Null, None).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(agent_io);
        let err = pending.await.unwrap().unwrap_err();
        assert_eq!(err.code, RpcError::CONNECTION_CLOSED);
    }
}
