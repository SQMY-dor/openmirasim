//! SQLite persistence (W0-2): WAL mode, one dedicated writer thread, a small pool of
//! read-only connections.
//!
//! - All writes are serialized on the writer thread, so there is never lock contention
//!   between writers and every multi-row change is one transaction.
//! - Event appends assign `seq` inside the transaction (`head_seq + 1`) and keep the
//!   derived `sessions` columns (`head_seq`, `status`, `native_session_id`) in sync, so
//!   the stream and the session row can never disagree.
//! - Readers run on `spawn_blocking` with read-only connections; WAL lets them proceed
//!   while the writer commits.

mod audit;
mod members;
pub use audit::{ApprovalRule, AuditEntry};
pub use members::TOKEN_TOUCH_INTERVAL_MS;
mod goals;
mod migrations;
mod schedules;
#[cfg(test)]
mod tests;
mod workspaces;

pub use workspaces::{StoredWorkspace, StoredWorktree};

use om_core::approval::{ApprovalDecision, ApprovalRecord, ApprovalStatus};
use om_core::event::{EventPayload, SessionEvent};
use om_core::ids::{ApprovalId, MemberId, SessionId, SpaceId};
use om_core::session::{SessionConfig, SessionStatus, SessionSummary};
use om_core::{CoreError, CoreResult, now_ms};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::oneshot;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Default number of pooled read-only connections.
const DEFAULT_READERS: usize = 4;

/// Handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Mutex<Option<mpsc::Sender<WriteJob>>>,
    writer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    readers: Mutex<Vec<Connection>>,
    path: PathBuf,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing the channel stops the writer loop; then wait for it to flush.
        self.writer.lock().map(|mut w| w.take()).ok();
        if let Ok(mut t) = self.writer_thread.lock()
            && let Some(handle) = t.take()
        {
            let _ = handle.join();
        }
    }
}

/// An approval together with the harness-native request id it answers.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredApproval {
    pub record: ApprovalRecord,
    pub harness_request_id: String,
}

/// Result of resolving or cancelling an approval.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalOutcome {
    pub approval: StoredApproval,
    pub event: SessionEvent,
}

/// A page of events.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    pub events: Vec<SessionEvent>,
    pub head_seq: u64,
    pub has_more: bool,
}

fn storage<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Storage(e.to_string())
}

fn to_json<T: Serialize>(v: &T) -> rusqlite::Result<String> {
    serde_json::to_string(v).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn from_json<T: DeserializeOwned>(col: usize, s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Serializes a unit-like enum (e.g. `SessionStatus`) to its bare string form.
fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        other => panic!("expected string enum, got {other:?}"),
    }
}

fn parse_enum<T: DeserializeOwned>(col: usize, s: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (idx, sql) in migrations::MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

impl Db {
    /// Opens (creating if needed) the database at `path` and applies migrations.
    pub fn open(path: impl AsRef<Path>) -> CoreResult<Self> {
        Self::open_with_readers(path, DEFAULT_READERS)
    }

    /// Like [`Db::open`] with an explicit read pool size (at least 1).
    pub fn open_with_readers(path: impl AsRef<Path>, readers: usize) -> CoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let mut writer = Connection::open(&path).map_err(storage)?;
        let mode: String = writer
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .map_err(storage)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(CoreError::Storage(format!(
                "failed to enable WAL mode (got `{mode}`)"
            )));
        }
        writer
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(storage)?;
        configure(&writer).map_err(storage)?;
        migrate(&mut writer).map_err(storage)?;

        let mut pool = Vec::with_capacity(readers.max(1));
        for _ in 0..readers.max(1) {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(storage)?;
            configure(&conn).map_err(storage)?;
            pool.push(conn);
        }

        let (tx, rx) = mpsc::channel::<WriteJob>();
        let handle = std::thread::Builder::new()
            .name("om-db-writer".into())
            .spawn(move || {
                let mut conn = writer;
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })?;

        Ok(Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(Some(tx)),
                writer_thread: Mutex::new(Some(handle)),
                readers: Mutex::new(pool),
                path,
            }),
        })
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs `f` on the writer thread.
    pub async fn write<T, F>(&self, f: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> CoreResult<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn| {
            let _ = tx.send(f(conn));
        });
        {
            let guard = self.inner.writer.lock().map_err(storage)?;
            let sender = guard
                .as_ref()
                .ok_or_else(|| CoreError::Storage("database is closed".into()))?;
            sender
                .send(job)
                .map_err(|_| CoreError::Storage("writer thread stopped".into()))?;
        }
        rx.await
            .map_err(|_| CoreError::Storage("writer dropped the job".into()))?
    }

    /// Runs `f` on a pooled read-only connection.
    pub async fn read<T, F>(&self, f: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> CoreResult<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let conn = inner.readers.lock().map_err(storage)?.pop();
            let conn = match conn {
                Some(c) => c,
                None => {
                    // Pool exhausted: open a temporary extra reader.
                    let c = Connection::open_with_flags(
                        &inner.path,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )
                    .map_err(storage)?;
                    configure(&c).map_err(storage)?;
                    c
                }
            };
            let result = f(&conn);
            if let Ok(mut pool) = inner.readers.lock()
                && pool.len() < DEFAULT_READERS.max(1) * 2
            {
                pool.push(conn);
            }
            result
        })
        .await
        .map_err(storage)?
    }

    // ---------------------------------------------------------------- spaces

    /// Returns the id of the space named `name`, creating it if absent.
    pub async fn ensure_space(&self, name: &str) -> CoreResult<SpaceId> {
        let name = name.to_owned();
        self.write(move |conn| {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM spaces WHERE name = ?1 ORDER BY created_at LIMIT 1",
                    [&name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            if let Some(id) = existing {
                return Ok(SpaceId(id));
            }
            let id = SpaceId::new();
            conn.execute(
                "INSERT INTO spaces (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![id.as_str(), name, now_ms()],
            )
            .map_err(storage)?;
            Ok(id)
        })
        .await
    }

    // -------------------------------------------------------------- sessions

    /// Creates a session in `Starting` state and appends its `SessionCreated` event (seq 1).
    pub async fn create_session(
        &self,
        space_id: SpaceId,
        config: SessionConfig,
        created_by: Option<MemberId>,
    ) -> CoreResult<(SessionSummary, SessionEvent)> {
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let id = SessionId::new();
            let now = now_ms();
            tx.execute(
                "INSERT INTO sessions (id, space_id, config, status, head_seq, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
                params![
                    id.as_str(),
                    space_id.as_str(),
                    to_json(&config).map_err(storage)?,
                    enum_str(&SessionStatus::Starting),
                    now
                ],
            )
            .map_err(storage)?;
            let mut events = append_in_tx(
                &tx,
                &id,
                vec![EventPayload::SessionCreated {
                    config: config.clone(),
                    created_by,
                }],
            )?;
            let summary = load_session(&tx, &id)?;
            tx.commit().map_err(storage)?;
            Ok((summary, events.remove(0)))
        })
        .await
    }

    pub async fn get_session(&self, id: &SessionId) -> CoreResult<SessionSummary> {
        let id = id.clone();
        self.read(move |conn| load_session(conn, &id)).await
    }

    /// Lists a space's sessions, newest first.
    pub async fn list_sessions(&self, space_id: &SpaceId) -> CoreResult<Vec<SessionSummary>> {
        let space_id = space_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{SESSION_COLUMNS} WHERE space_id = ?1 ORDER BY created_at DESC, id DESC"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map([space_id.as_str()], row_to_session)
                .map_err(storage)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage)
        })
        .await
    }

    // ---------------------------------------------------------------- events

    /// Appends events atomically, assigning consecutive seqs. Returns them in order.
    pub async fn append_events(
        &self,
        session_id: &SessionId,
        payloads: Vec<EventPayload>,
    ) -> CoreResult<Vec<SessionEvent>> {
        let session_id = session_id.clone();
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let events = append_in_tx(&tx, &session_id, payloads)?;
            tx.commit().map_err(storage)?;
            Ok(events)
        })
        .await
    }

    /// Returns events with `seq > after_seq`, at most `limit` of them.
    pub async fn list_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: u32,
    ) -> CoreResult<EventPage> {
        let session_id = session_id.clone();
        let limit = limit.max(1);
        self.read(move |conn| {
            let head_seq = head_seq(conn, &session_id)?;
            let mut stmt = conn
                .prepare(
                    "SELECT seq, ts, payload FROM events
                     WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(storage)?;
            let rows = stmt
                .query_map(
                    params![session_id.as_str(), after_seq as i64, limit as i64 + 1],
                    |r| {
                        let seq: i64 = r.get(0)?;
                        let ts: i64 = r.get(1)?;
                        let payload: String = r.get(2)?;
                        Ok(SessionEvent {
                            session_id: session_id.clone(),
                            seq: seq as u64,
                            ts,
                            payload: from_json(2, &payload)?,
                        })
                    },
                )
                .map_err(storage)?;
            let mut events = rows.collect::<Result<Vec<_>, _>>().map_err(storage)?;
            let has_more = events.len() > limit as usize;
            events.truncate(limit as usize);
            Ok(EventPage {
                events,
                head_seq,
                has_more,
            })
        })
        .await
    }

    // ------------------------------------------------------------- approvals

    /// Stores a pending approval and appends its `ApprovalRequested` event atomically.
    pub async fn create_approval(
        &self,
        approval: StoredApproval,
        turn_id: Option<om_core::ids::TurnId>,
    ) -> CoreResult<SessionEvent> {
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let r = &approval.record;
            tx.execute(
                "INSERT INTO approvals (id, session_id, harness_request_id, tool_call_id, title,
                                        options, status, created_at, tool_kind, locations)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    r.id.as_str(),
                    r.session_id.as_str(),
                    approval.harness_request_id,
                    r.tool_call_id,
                    r.title,
                    to_json(&r.options).map_err(storage)?,
                    enum_str(&ApprovalStatus::Pending),
                    r.created_at,
                    r.tool_kind.as_ref().map(enum_str),
                    to_json(&r.locations).map_err(storage)?
                ],
            )
            .map_err(storage)?;
            audit::insert(
                &tx,
                None,
                "approval.requested",
                r.id.as_str(),
                &serde_json::json!({
                    "session_id": r.session_id,
                    "title": r.title,
                    "tool_kind": r.tool_kind,
                    "locations": r.locations,
                }),
            )?;
            let mut events = append_in_tx(
                &tx,
                &r.session_id,
                vec![EventPayload::ApprovalRequested {
                    turn_id,
                    approval: r.clone(),
                }],
            )?;
            tx.commit().map_err(storage)?;
            Ok(events.remove(0))
        })
        .await
    }

    /// Atomically resolves a pending approval (first writer wins) and appends
    /// `ApprovalResolved`. Fails with [`CoreError::ApprovalAlreadyResolved`] if another
    /// member resolved it first.
    pub async fn resolve_approval(
        &self,
        approval_id: &ApprovalId,
        decision: ApprovalDecision,
        resolved_by: Option<MemberId>,
    ) -> CoreResult<ApprovalOutcome> {
        let approval_id = approval_id.clone();
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let outcome = resolve_in_tx(&tx, &approval_id, decision, resolved_by, None)?;
            tx.commit().map_err(storage)?;
            Ok(outcome)
        })
        .await
    }

    /// Like [`Db::resolve_approval`], but records that the server's approval policy
    /// (not a member) made the decision, naming the rule that matched.
    pub async fn resolve_approval_by_policy(
        &self,
        approval_id: &ApprovalId,
        decision: ApprovalDecision,
        rule: String,
    ) -> CoreResult<ApprovalOutcome> {
        let approval_id = approval_id.clone();
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let outcome = resolve_in_tx(&tx, &approval_id, decision, None, Some(rule))?;
            tx.commit().map_err(storage)?;
            Ok(outcome)
        })
        .await
    }

    /// Cancels every pending approval of a session (turn cancelled, session closed).
    pub async fn cancel_pending_approvals(
        &self,
        session_id: &SessionId,
    ) -> CoreResult<Vec<ApprovalOutcome>> {
        let session_id = session_id.clone();
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let ids: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM approvals WHERE session_id = ?1 AND status = 'pending'
                         ORDER BY created_at",
                    )
                    .map_err(storage)?;
                stmt.query_map([session_id.as_str()], |r| r.get(0))
                    .map_err(storage)?
                    .collect::<Result<_, _>>()
                    .map_err(storage)?
            };
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                out.push(resolve_in_tx(
                    &tx,
                    &ApprovalId(id),
                    ApprovalDecision::Cancelled,
                    None,
                    None,
                )?);
            }
            tx.commit().map_err(storage)?;
            Ok(out)
        })
        .await
    }

    pub async fn get_approval(&self, id: &ApprovalId) -> CoreResult<StoredApproval> {
        let id = id.clone();
        self.read(move |conn| load_approval(conn, &id)).await
    }

    /// Lists a session's approvals, oldest first.
    pub async fn list_approvals(
        &self,
        session_id: &SessionId,
        pending_only: bool,
    ) -> CoreResult<Vec<StoredApproval>> {
        let session_id = session_id.clone();
        self.read(move |conn| {
            let sql = if pending_only {
                format!(
                    "{APPROVAL_COLUMNS} WHERE session_id = ?1 AND status = 'pending' ORDER BY created_at, id"
                )
            } else {
                format!("{APPROVAL_COLUMNS} WHERE session_id = ?1 ORDER BY created_at, id")
            };
            let mut stmt = conn.prepare(&sql).map_err(storage)?;
            let rows = stmt
                .query_map([session_id.as_str()], row_to_approval)
                .map_err(storage)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage)
        })
        .await
    }

    // -------------------------------------------------------------- recovery

    /// Called once at startup. Agent processes do not survive a server restart, so
    /// every session that was live is moved to `Failed` (resumable), its pending
    /// approvals are cancelled, and an `Error` event explains why. Returns the ids of
    /// recovered sessions.
    pub async fn recover_after_restart(&self) -> CoreResult<Vec<SessionId>> {
        self.write(|conn| {
            let tx = conn.transaction().map_err(storage)?;
            let live: Vec<(String, String)> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id, status FROM sessions
                         WHERE status IN ('starting', 'idle', 'running', 'waiting_approval')
                         ORDER BY created_at",
                    )
                    .map_err(storage)?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(storage)?
                    .collect::<Result<_, _>>()
                    .map_err(storage)?
            };
            let mut recovered = Vec::with_capacity(live.len());
            for (id, status) in live {
                let session_id = SessionId(id);
                let from: SessionStatus = parse_enum(1, &status).map_err(storage)?;
                let pending: Vec<String> = {
                    let mut stmt = tx
                        .prepare(
                            "SELECT id FROM approvals WHERE session_id = ?1 AND status = 'pending'",
                        )
                        .map_err(storage)?;
                    stmt.query_map([session_id.as_str()], |r| r.get(0))
                        .map_err(storage)?
                        .collect::<Result<_, _>>()
                        .map_err(storage)?
                };
                for approval_id in pending {
                    resolve_in_tx(
                        &tx,
                        &ApprovalId(approval_id),
                        ApprovalDecision::Cancelled,
                        None,
                        Some("server_restart".into()),
                    )?;
                }
                append_in_tx(
                    &tx,
                    &session_id,
                    vec![
                        EventPayload::Error {
                            message: "server restarted; agent process was lost".into(),
                        },
                        EventPayload::StatusChanged {
                            from,
                            to: SessionStatus::Failed,
                        },
                    ],
                )?;
                recovered.push(session_id);
            }
            tx.commit().map_err(storage)?;
            Ok(recovered)
        })
        .await
    }
}

// -------------------------------------------------------------------- helpers

const SESSION_COLUMNS: &str = "SELECT id, space_id, config, status, head_seq, native_session_id,
                                      created_at, updated_at FROM sessions";

const APPROVAL_COLUMNS: &str = "SELECT id, session_id, harness_request_id, tool_call_id, title,
                                       options, status, decision, resolved_by, created_at,
                                       resolved_at, tool_kind, locations, auto_rule
                                FROM approvals";

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    let config: String = r.get(2)?;
    let status: String = r.get(3)?;
    let head_seq: i64 = r.get(4)?;
    Ok(SessionSummary {
        id: SessionId(r.get(0)?),
        space_id: SpaceId(r.get(1)?),
        config: from_json(2, &config)?,
        status: parse_enum(3, &status)?,
        head_seq: head_seq as u64,
        native_session_id: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

fn row_to_approval(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredApproval> {
    let options: String = r.get(5)?;
    let status: String = r.get(6)?;
    let decision: Option<String> = r.get(7)?;
    let resolved_by: Option<String> = r.get(8)?;
    let tool_kind: Option<String> = r.get(11)?;
    let locations: Option<String> = r.get(12)?;
    Ok(StoredApproval {
        harness_request_id: r.get(2)?,
        record: ApprovalRecord {
            id: ApprovalId(r.get(0)?),
            session_id: SessionId(r.get(1)?),
            tool_call_id: r.get(3)?,
            title: r.get(4)?,
            tool_kind: tool_kind.map(|k| parse_enum(11, &k)).transpose()?,
            locations: locations
                .map(|l| from_json(12, &l))
                .transpose()?
                .unwrap_or_default(),
            options: from_json(5, &options)?,
            status: parse_enum(6, &status)?,
            decision: decision.map(|d| from_json(7, &d)).transpose()?,
            resolved_by: resolved_by.map(MemberId),
            created_at: r.get(9)?,
            resolved_at: r.get(10)?,
            auto_rule: r.get(13)?,
        },
    })
}

fn load_session(conn: &Connection, id: &SessionId) -> CoreResult<SessionSummary> {
    conn.query_row(
        &format!("{SESSION_COLUMNS} WHERE id = ?1"),
        [id.as_str()],
        row_to_session,
    )
    .optional()
    .map_err(storage)?
    .ok_or_else(|| CoreError::NotFound(format!("session {id}")))
}

fn load_approval(conn: &Connection, id: &ApprovalId) -> CoreResult<StoredApproval> {
    conn.query_row(
        &format!("{APPROVAL_COLUMNS} WHERE id = ?1"),
        [id.as_str()],
        row_to_approval,
    )
    .optional()
    .map_err(storage)?
    .ok_or_else(|| CoreError::NotFound(format!("approval {id}")))
}

fn head_seq(conn: &Connection, id: &SessionId) -> CoreResult<u64> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT head_seq FROM sessions WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    seq.map(|s| s as u64)
        .ok_or_else(|| CoreError::NotFound(format!("session {id}")))
}

/// Appends payloads inside an open transaction and updates derived session columns.
fn append_in_tx(
    tx: &Transaction<'_>,
    session_id: &SessionId,
    payloads: Vec<EventPayload>,
) -> CoreResult<Vec<SessionEvent>> {
    let (mut seq, mut status): (u64, SessionStatus) = {
        let (seq, status): (i64, String) = tx
            .query_row(
                "SELECT head_seq, status FROM sessions WHERE id = ?1",
                [session_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(storage)?
            .ok_or_else(|| CoreError::NotFound(format!("session {session_id}")))?;
        (seq as u64, parse_enum(1, &status).map_err(storage)?)
    };
    let mut native: Option<String> = None;
    let mut out = Vec::with_capacity(payloads.len());
    let ts = now_ms();
    {
        let mut insert = tx
            .prepare_cached(
                "INSERT INTO events (session_id, seq, ts, type, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(storage)?;
        for payload in payloads {
            match &payload {
                EventPayload::StatusChanged { from, to } => {
                    if *from != status {
                        return Err(CoreError::InvalidInput(format!(
                            "status_changed.from {from:?} does not match current status {status:?}"
                        )));
                    }
                    status = from.transition(*to)?;
                }
                EventPayload::HarnessReady {
                    native_session_id: Some(id),
                    ..
                } => native = Some(id.clone()),
                _ => {}
            }
            seq += 1;
            insert
                .execute(params![
                    session_id.as_str(),
                    seq as i64,
                    ts,
                    payload.kind(),
                    to_json(&payload).map_err(storage)?
                ])
                .map_err(storage)?;
            out.push(SessionEvent {
                session_id: session_id.clone(),
                seq,
                ts,
                payload,
            });
        }
    }
    tx.execute(
        "UPDATE sessions SET head_seq = ?2, status = ?3, updated_at = ?4,
                native_session_id = COALESCE(?5, native_session_id)
         WHERE id = ?1",
        params![
            session_id.as_str(),
            seq as i64,
            enum_str(&status),
            ts,
            native
        ],
    )
    .map_err(storage)?;
    Ok(out)
}

fn resolve_in_tx(
    tx: &Transaction<'_>,
    approval_id: &ApprovalId,
    decision: ApprovalDecision,
    resolved_by: Option<MemberId>,
    auto_rule: Option<String>,
) -> CoreResult<ApprovalOutcome> {
    let current = load_approval(tx, approval_id)?;
    if let ApprovalDecision::Selected { option_id } = &decision
        && !current
            .record
            .options
            .iter()
            .any(|o| &o.option_id == option_id)
    {
        return Err(CoreError::InvalidInput(format!(
            "unknown option `{option_id}` for approval {approval_id}"
        )));
    }
    let new_status = match decision {
        ApprovalDecision::Selected { .. } => ApprovalStatus::Resolved,
        ApprovalDecision::Cancelled => ApprovalStatus::Cancelled,
    };
    let now = now_ms();
    let changed = tx
        .execute(
            "UPDATE approvals SET status = ?2, decision = ?3, resolved_by = ?4, resolved_at = ?5,
                                  auto_rule = ?6
             WHERE id = ?1 AND status = 'pending'",
            params![
                approval_id.as_str(),
                enum_str(&new_status),
                to_json(&decision).map_err(storage)?,
                resolved_by.as_ref().map(|m| m.as_str()),
                now,
                auto_rule
            ],
        )
        .map_err(storage)?;
    if changed == 0 {
        return Err(CoreError::ApprovalAlreadyResolved(approval_id.to_string()));
    }
    let action = match (&decision, &auto_rule) {
        (ApprovalDecision::Cancelled, _) => "approval.cancelled",
        (_, Some(_)) => "approval.auto_resolved",
        (_, None) => "approval.resolved",
    };
    audit::insert(
        tx,
        resolved_by.as_ref(),
        action,
        approval_id.as_str(),
        &serde_json::json!({
            "session_id": current.record.session_id,
            "title": current.record.title,
            "decision": decision,
            "rule": auto_rule,
        }),
    )?;
    let mut events = append_in_tx(
        tx,
        &current.record.session_id,
        vec![EventPayload::ApprovalResolved {
            approval_id: approval_id.clone(),
            decision,
            resolved_by,
            auto_rule,
        }],
    )?;
    let approval = load_approval(tx, approval_id)?;
    Ok(ApprovalOutcome {
        approval,
        event: events.remove(0),
    })
}
