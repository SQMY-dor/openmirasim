//! Goal and dispatch rows (schema v6, W4-1).

use super::{Db, enum_str, from_json, parse_enum, storage, to_json};
use om_core::dispatch::Dispatch;
use om_core::goal::{Goal, GoalStatus};
use om_core::ids::{DispatchId, GoalId, MemberId, SessionId, SpaceId, TurnId};
use om_core::{CoreError, CoreResult, now_ms};
use rusqlite::{OptionalExtension, params};

const GOAL_COLUMNS: &str = "SELECT id, space_id, session_id, objective, done_marker, max_turns,
                                   turns_used, deadline_at, created_by, status, ended_reason,
                                   created_at, updated_at FROM goals";
const DISPATCH_COLUMNS: &str = "SELECT id, space_id, from_session, to_session, member_id, depth,
                                       created_session, submit, created_at FROM dispatches";

fn row_to_goal(r: &rusqlite::Row<'_>) -> rusqlite::Result<Goal> {
    Ok(Goal {
        id: GoalId(r.get(0)?),
        space_id: SpaceId(r.get(1)?),
        session_id: SessionId(r.get(2)?),
        objective: r.get(3)?,
        done_marker: r.get(4)?,
        max_turns: r.get(5)?,
        turns_used: r.get(6)?,
        deadline_at: r.get(7)?,
        created_by: MemberId(r.get(8)?),
        status: parse_enum(9, &r.get::<_, String>(9)?)?,
        ended_reason: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

/// See [`Db::turn_context`].
#[derive(Debug, Clone, PartialEq)]
pub struct TurnContext {
    pub turn_id: TurnId,
    /// When the turn's prompt was submitted (Unix ms).
    pub started_at: i64,
    /// The turn's starter, then every member who steered into it; `None` for a prompt
    /// no member sent.
    pub members: Vec<Option<MemberId>>,
    /// Depth of the dispatch chain that delivered this turn; 0 for a member's own turn.
    pub depth: u32,
}

fn row_to_dispatch(r: &rusqlite::Row<'_>) -> rusqlite::Result<Dispatch> {
    Ok(Dispatch {
        id: DispatchId(r.get(0)?),
        space_id: SpaceId(r.get(1)?),
        from_session: SessionId(r.get(2)?),
        to_session: SessionId(r.get(3)?),
        member_id: MemberId(r.get(4)?),
        depth: r.get(5)?,
        created_session: r.get(6)?,
        submit: from_json(7, &r.get::<_, String>(7)?)?,
        created_at: r.get(8)?,
    })
}

impl Db {
    /// Stores a new active goal. A session holds at most one active goal.
    pub async fn create_goal(&self, g: Goal) -> CoreResult<Goal> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO goals (id, space_id, session_id, objective, done_marker, max_turns,
                                    turns_used, deadline_at, created_by, status, ended_reason,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    g.id.as_str(),
                    g.space_id.as_str(),
                    g.session_id.as_str(),
                    g.objective,
                    g.done_marker,
                    g.max_turns,
                    g.turns_used,
                    g.deadline_at,
                    g.created_by.as_str(),
                    enum_str(&g.status),
                    g.ended_reason,
                    g.created_at,
                    g.updated_at
                ],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                        || f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
                {
                    CoreError::InvalidInput("the session already has an active goal".into())
                }
                rusqlite::Error::SqliteFailure(f, _)
                    if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
                {
                    CoreError::NotFound("referenced space or session".into())
                }
                other => storage(other),
            })?;
            Ok(g)
        })
        .await
    }

    pub async fn get_goal(&self, id: &GoalId) -> CoreResult<Goal> {
        let id = id.clone();
        self.read(move |conn| {
            conn.query_row(
                &format!("{GOAL_COLUMNS} WHERE id = ?1"),
                [id.as_str()],
                row_to_goal,
            )
            .optional()
            .map_err(storage)?
            .ok_or_else(|| CoreError::NotFound(format!("goal {id}")))
        })
        .await
    }

    /// A session's goals, newest first.
    pub async fn list_goals(&self, session_id: &SessionId) -> CoreResult<Vec<Goal>> {
        let session_id = session_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{GOAL_COLUMNS} WHERE session_id = ?1 ORDER BY created_at DESC, id DESC"
                ))
                .map_err(storage)?;
            stmt.query_map([session_id.as_str()], row_to_goal)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    /// Every goal still `active`, across all sessions.
    pub async fn list_active_goals(&self) -> CoreResult<Vec<Goal>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{GOAL_COLUMNS} WHERE status = 'active' ORDER BY created_at, id"
                ))
                .map_err(storage)?;
            stmt.query_map([], row_to_goal)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    /// Records one more finished turn on an active goal. Returns `false` when the goal is
    /// no longer active.
    pub async fn bump_goal_turns(&self, id: &GoalId, turns_used: u32) -> CoreResult<bool> {
        let id = id.clone();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE goals SET turns_used = ?2, updated_at = ?3
                     WHERE id = ?1 AND status = 'active'",
                    params![id.as_str(), turns_used, now_ms()],
                )
                .map_err(storage)?;
            Ok(n == 1)
        })
        .await
    }

    /// Moves an active goal to a final status. Returns `false` when it already ended
    /// (exactly one caller wins).
    pub async fn end_goal(
        &self,
        id: &GoalId,
        status: GoalStatus,
        reason: &str,
    ) -> CoreResult<bool> {
        let (id, reason) = (id.clone(), reason.to_owned());
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE goals SET status = ?2, ended_reason = ?3, updated_at = ?4
                     WHERE id = ?1 AND status = 'active'",
                    params![id.as_str(), enum_str(&status), reason, now_ms()],
                )
                .map_err(storage)?;
            Ok(n == 1)
        })
        .await
    }

    /// Stops every active goal (used at server start). Returns how many it stopped.
    pub async fn stop_active_goals(&self, reason: &str) -> CoreResult<usize> {
        let reason = reason.to_owned();
        self.write(move |conn| {
            conn.execute(
                "UPDATE goals SET status = 'stopped', ended_reason = ?1, updated_at = ?2
                 WHERE status = 'active'",
                params![reason, now_ms()],
            )
            .map_err(storage)
        })
        .await
    }

    /// The member whose prompt started the session's latest turn, if a member did.
    pub async fn last_turn_member(&self, session_id: &SessionId) -> CoreResult<Option<MemberId>> {
        Ok(self
            .turn_context(session_id)
            .await?
            .and_then(|t| t.members.into_iter().next().flatten()))
    }

    /// Who instructed the session's latest turn and how deep in a dispatch chain it is
    /// (review P1-14). `None` when the session never had a turn.
    pub async fn turn_context(&self, session_id: &SessionId) -> CoreResult<Option<TurnContext>> {
        let session_id = session_id.clone();
        self.read(move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage)?;
            let latest: Option<(i64, i64, String, Option<String>)> = tx
                .query_row(
                    "SELECT seq, ts, json_extract(payload, '$.turn_id'),
                            json_extract(payload, '$.member_id')
                     FROM events WHERE session_id = ?1 AND type = 'prompt_submitted'
                     ORDER BY seq DESC LIMIT 1",
                    [session_id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(storage)?;
            let Some((seq, ts, turn_id, starter)) = latest else {
                return Ok(None);
            };
            // Everyone who instructed this turn: its starter, then each steer.
            let mut members = vec![starter.map(MemberId)];
            {
                let mut stmt = tx
                    .prepare(
                        "SELECT json_extract(payload, '$.member_id') FROM events
                         WHERE session_id = ?1 AND type = 'prompt_steered' AND seq > ?2
                         ORDER BY seq",
                    )
                    .map_err(storage)?;
                let steers = stmt
                    .query_map(params![session_id.as_str(), seq], |r| {
                        r.get::<_, Option<String>>(0)
                    })
                    .map_err(storage)?;
                for m in steers {
                    members.push(m.map_err(storage)?.map(MemberId));
                }
            }
            // The deepest dispatch that delivered this turn (directly, via the queue, or
            // as a steer); a dispatch still being delivered counts too.
            let depth: Option<u32> = tx
                .query_row(
                    "SELECT MAX(depth) FROM dispatches
                     WHERE to_session = ?1 AND (
                         submit = 'null'
                         OR json_extract(submit, '$.turn_id') = ?2
                         OR json_extract(submit, '$.queued_id') IN (
                             SELECT json_extract(payload, '$.queued_id') FROM events
                             WHERE session_id = ?1 AND type = 'queue_item_removed'
                               AND json_extract(payload, '$.reason.turn_id') = ?2))",
                    params![session_id.as_str(), turn_id],
                    |r| r.get(0),
                )
                .map_err(storage)?;
            Ok(Some(TurnContext {
                turn_id: TurnId(turn_id),
                started_at: ts,
                members,
                depth: depth.unwrap_or(0),
            }))
        })
        .await
    }

    /// Reserves a dispatch row before its prompt is sent, so its depth counts at once,
    /// and enforces the per-turn fan-out cap keyed by `from_turn_id` (immune to wall-clock
    /// adjustments and separate from manual dispatches). Complete it with
    /// [`Db::complete_dispatch`] or drop it with [`Db::delete_dispatch`].
    pub async fn reserve_dispatch(
        &self,
        d: Dispatch,
        fanout: Option<(TurnId, u32)>,
    ) -> CoreResult<Dispatch> {
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let from_turn = fanout.as_ref().map(|(tid, _)| tid.as_str().to_owned());
            if let Some((ref tid, max)) = fanout {
                let n: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM dispatches
                         WHERE from_session = ?1 AND from_turn_id = ?2",
                        params![d.from_session.as_str(), tid.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(storage)?;
                if n >= i64::from(max) {
                    return Err(CoreError::LimitExceeded(format!(
                        "a turn may dispatch at most {max} times"
                    )));
                }
            }
            tx.execute(
                "INSERT INTO dispatches (id, space_id, from_session, to_session, member_id, depth,
                                         created_session, submit, created_at, from_turn_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'null', ?8, ?9)",
                params![
                    d.id.as_str(),
                    d.space_id.as_str(),
                    d.from_session.as_str(),
                    d.to_session.as_str(),
                    d.member_id.as_str(),
                    d.depth,
                    d.created_session,
                    d.created_at,
                    from_turn
                ],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
            Ok(d)
        })
        .await
    }

    /// Records how a reserved dispatch's prompt was taken.
    pub async fn complete_dispatch(&self, d: Dispatch) -> CoreResult<Dispatch> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE dispatches SET submit = ?2 WHERE id = ?1",
                    params![d.id.as_str(), to_json(&d.submit).map_err(storage)?],
                )
                .map_err(storage)?;
            if n == 0 {
                return Err(CoreError::NotFound(format!("dispatch {}", d.id)));
            }
            Ok(d)
        })
        .await
    }

    /// Drops a reserved dispatch whose prompt was never delivered.
    pub async fn delete_dispatch(&self, id: &DispatchId) -> CoreResult<()> {
        let id = id.clone();
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM dispatches WHERE id = ?1 AND submit = 'null'",
                [id.as_str()],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    pub async fn insert_dispatch(&self, d: Dispatch) -> CoreResult<Dispatch> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO dispatches (id, space_id, from_session, to_session, member_id, depth,
                                         created_session, submit, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    d.id.as_str(),
                    d.space_id.as_str(),
                    d.from_session.as_str(),
                    d.to_session.as_str(),
                    d.member_id.as_str(),
                    d.depth,
                    d.created_session,
                    to_json(&d.submit).map_err(storage)?,
                    d.created_at
                ],
            )
            .map_err(storage)?;
            Ok(d)
        })
        .await
    }

    /// Dispatches sent from or received by a session, newest first.
    pub async fn list_dispatches(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> CoreResult<Vec<Dispatch>> {
        let session_id = session_id.clone();
        let clamped = i64::try_from(limit).unwrap_or(500).clamp(1, 500);
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{DISPATCH_COLUMNS} WHERE (from_session = ?1 OR to_session = ?1)
                       AND submit != 'null'
                     ORDER BY created_at DESC, id DESC LIMIT ?2"
                ))
                .map_err(storage)?;
            stmt.query_map(params![session_id.as_str(), clamped], row_to_dispatch)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }
}
