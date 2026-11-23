//! Schedule rows (schema v5, W4-1).

use super::{Db, enum_str, from_json, parse_enum, storage, to_json};
use om_core::ids::{ScheduleId, SessionId, SpaceId};
use om_core::schedule::{MAX_SCHEDULES_PER_SPACE, Schedule, ScheduleRun};
use om_core::{CoreError, CoreResult};
use rusqlite::{OptionalExtension, params};

const SCHEDULE_COLUMNS: &str = "SELECT id, space_id, name, spec, target, created_by, enabled,
                                       missed, overlap, next_run_at, last_run_at, created_at,
                                       updated_at FROM schedules";
const RUN_COLUMNS: &str = "SELECT id, schedule_id, scheduled_for, fired_at, outcome, manual
                                  FROM schedule_runs";

fn row_to_schedule(r: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: ScheduleId(r.get(0)?),
        space_id: SpaceId(r.get(1)?),
        name: r.get(2)?,
        spec: from_json(3, &r.get::<_, String>(3)?)?,
        target: from_json(4, &r.get::<_, String>(4)?)?,
        created_by: om_core::ids::MemberId(r.get(5)?),
        enabled: r.get(6)?,
        missed: parse_enum(7, &r.get::<_, String>(7)?)?,
        overlap: parse_enum(8, &r.get::<_, String>(8)?)?,
        next_run_at: r.get(9)?,
        last_run_at: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleRun> {
    Ok(ScheduleRun {
        id: om_core::ids::ScheduleRunId(r.get(0)?),
        schedule_id: ScheduleId(r.get(1)?),
        scheduled_for: r.get(2)?,
        fired_at: r.get(3)?,
        outcome: from_json(4, &r.get::<_, String>(4)?)?,
        manual: r.get(5)?,
    })
}

fn name_taken(e: rusqlite::Error, name: &str) -> CoreError {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            CoreError::InvalidInput(format!(
                "a schedule named `{name}` already exists in this space"
            ))
        }
        other => storage(other),
    }
}

impl Db {
    /// Stores a new schedule. Names are unique per space; a space holds at most
    /// [`MAX_SCHEDULES_PER_SPACE`] schedules.
    pub async fn create_schedule(&self, s: Schedule) -> CoreResult<Schedule> {
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let count: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM schedules WHERE space_id = ?1",
                    [s.space_id.as_str()],
                    |r| r.get(0),
                )
                .map_err(storage)?;
            if count as usize >= MAX_SCHEDULES_PER_SPACE {
                return Err(CoreError::LimitExceeded(format!(
                    "a space holds at most {MAX_SCHEDULES_PER_SPACE} schedules"
                )));
            }
            tx.execute(
                "INSERT INTO schedules (id, space_id, name, spec, target, created_by, enabled,
                                        missed, overlap, next_run_at, last_run_at, created_at,
                                        updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    s.id.as_str(),
                    s.space_id.as_str(),
                    s.name,
                    to_json(&s.spec).map_err(storage)?,
                    to_json(&s.target).map_err(storage)?,
                    s.created_by.as_str(),
                    s.enabled,
                    enum_str(&s.missed),
                    enum_str(&s.overlap),
                    s.next_run_at,
                    s.last_run_at,
                    s.created_at,
                    s.updated_at
                ],
            )
            .map_err(|e| name_taken(e, &s.name))?;
            tx.commit().map_err(storage)?;
            Ok(s)
        })
        .await
    }

    /// Replaces a schedule's editable fields (name, spec, target, enabled, policies and
    /// `next_run_at`).
    pub async fn update_schedule(&self, s: Schedule) -> CoreResult<Schedule> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE schedules SET name = ?2, spec = ?3, target = ?4, enabled = ?5,
                            missed = ?6, overlap = ?7, next_run_at = ?8, updated_at = ?9,
                            created_by = ?10
                     WHERE id = ?1",
                    params![
                        s.id.as_str(),
                        s.name,
                        to_json(&s.spec).map_err(storage)?,
                        to_json(&s.target).map_err(storage)?,
                        s.enabled,
                        enum_str(&s.missed),
                        enum_str(&s.overlap),
                        s.next_run_at,
                        s.updated_at,
                        s.created_by.as_str()
                    ],
                )
                .map_err(|e| name_taken(e, &s.name))?;
            if n == 0 {
                return Err(CoreError::NotFound(format!("schedule {}", s.id)));
            }
            Ok(s)
        })
        .await
    }

    pub async fn get_schedule(&self, id: &ScheduleId) -> CoreResult<Schedule> {
        let id = id.clone();
        self.read(move |conn| {
            conn.query_row(
                &format!("{SCHEDULE_COLUMNS} WHERE id = ?1"),
                [id.as_str()],
                row_to_schedule,
            )
            .optional()
            .map_err(storage)?
            .ok_or_else(|| CoreError::NotFound(format!("schedule {id}")))
        })
        .await
    }

    /// A space's schedules by name.
    pub async fn list_schedules(&self, space_id: &SpaceId) -> CoreResult<Vec<Schedule>> {
        let space_id = space_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{SCHEDULE_COLUMNS} WHERE space_id = ?1 ORDER BY name"
                ))
                .map_err(storage)?;
            stmt.query_map([space_id.as_str()], row_to_schedule)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    /// Pauses or resumes a schedule, touching only those columns so a concurrent
    /// edit of its definition is not overwritten (review P2-7).
    pub async fn set_schedule_enabled(
        &self,
        id: &ScheduleId,
        enabled: bool,
        next_run_at: Option<i64>,
        updated_at: i64,
    ) -> CoreResult<Schedule> {
        let id = id.clone();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE schedules SET enabled = ?2, next_run_at = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![id.as_str(), enabled, next_run_at, updated_at],
                )
                .map_err(storage)?;
            if n == 0 {
                return Err(CoreError::NotFound(format!("schedule {id}")));
            }
            conn.query_row(
                &format!("{SCHEDULE_COLUMNS} WHERE id = ?1"),
                [id.as_str()],
                row_to_schedule,
            )
            .map_err(storage)
        })
        .await
    }

    /// Deletes a schedule and its runs.
    pub async fn delete_schedule(&self, id: &ScheduleId) -> CoreResult<()> {
        let id = id.clone();
        self.write(move |conn| {
            let n = conn
                .execute("DELETE FROM schedules WHERE id = ?1", [id.as_str()])
                .map_err(storage)?;
            if n == 0 {
                return Err(CoreError::NotFound(format!("schedule {id}")));
            }
            Ok(())
        })
        .await
    }

    /// Enabled schedules due at or before `now`, earliest first.
    pub async fn due_schedules(&self, now: i64) -> CoreResult<Vec<Schedule>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{SCHEDULE_COLUMNS} WHERE enabled = 1 AND next_run_at IS NOT NULL
                       AND next_run_at <= ?1 ORDER BY next_run_at"
                ))
                .map_err(storage)?;
            stmt.query_map([now], row_to_schedule)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    /// The earliest upcoming fire of any enabled schedule.
    pub async fn next_schedule_due(&self) -> CoreResult<Option<i64>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT MIN(next_run_at) FROM schedules WHERE enabled = 1",
                [],
                |r| r.get(0),
            )
            .map_err(storage)
        })
        .await
    }

    /// Claims the fire due at `expected` by moving the schedule to `next` (compare and
    /// swap). Returns `false` when another tick, an edit or a delete got there first, in
    /// which case the caller must not fire.
    pub async fn claim_schedule_fire(
        &self,
        id: &ScheduleId,
        expected: i64,
        next: Option<i64>,
        now: i64,
    ) -> CoreResult<bool> {
        let id = id.clone();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE schedules SET next_run_at = ?3, last_run_at = ?4, updated_at = ?4
                     WHERE id = ?1 AND enabled = 1 AND next_run_at = ?2",
                    params![id.as_str(), expected, next, now],
                )
                .map_err(storage)?;
            Ok(n == 1)
        })
        .await
    }

    /// Records a fire. A run for a schedule deleted meanwhile is dropped silently.
    pub async fn record_schedule_run(&self, run: ScheduleRun) -> CoreResult<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO schedule_runs (id, schedule_id, scheduled_for, fired_at, outcome,
                                            session_id, manual)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
                 WHERE EXISTS (SELECT 1 FROM schedules WHERE id = ?2)",
                params![
                    run.id.as_str(),
                    run.schedule_id.as_str(),
                    run.scheduled_for,
                    run.fired_at,
                    to_json(&run.outcome).map_err(storage)?,
                    run.outcome.session_id().map(|s| s.as_str().to_owned()),
                    run.manual
                ],
            )
            .map_err(storage)?;
            if run.manual {
                conn.execute(
                    "UPDATE schedules SET last_run_at = ?2 WHERE id = ?1",
                    params![run.schedule_id.as_str(), run.fired_at],
                )
                .map_err(storage)?;
            }
            Ok(())
        })
        .await
    }

    /// A schedule's runs, newest first.
    pub async fn list_schedule_runs(
        &self,
        id: &ScheduleId,
        limit: usize,
    ) -> CoreResult<Vec<ScheduleRun>> {
        let id = id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{RUN_COLUMNS} WHERE schedule_id = ?1 ORDER BY fired_at DESC, id DESC
                     LIMIT ?2"
                ))
                .map_err(storage)?;
            stmt.query_map(params![id.as_str(), limit as i64], row_to_run)
                .map_err(storage)?
                .collect::<Result<_, _>>()
                .map_err(storage)
        })
        .await
    }

    /// The session the schedule's most recent run created or prompted.
    pub async fn last_schedule_session(&self, id: &ScheduleId) -> CoreResult<Option<SessionId>> {
        let id = id.clone();
        self.read(move |conn| {
            conn.query_row(
                "SELECT session_id FROM schedule_runs
                 WHERE schedule_id = ?1 AND session_id IS NOT NULL
                 ORDER BY fired_at DESC, id DESC LIMIT 1",
                [id.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map(|s| s.map(SessionId))
            .map_err(storage)
        })
        .await
    }
}
