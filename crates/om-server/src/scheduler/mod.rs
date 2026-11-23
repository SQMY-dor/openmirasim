//! Schedules runtime (W4-1). Goals ([`GoalRunner`]) and Session-to-Session dispatch
//! ([`Dispatcher`]) live in submodules.
//!
//! [`Scheduler`] fires schedules on their timetable as the member who owns
//! them.
//!
//! - Due schedules are found in the database (`next_run_at <= now`) and claimed with a
//!   compare-and-swap on `next_run_at`, so a fire happens at most once even if two ticks
//!   overlap, and an edit or delete racing a fire wins cleanly.
//! - The next slot is computed from *now*, not from the missed slot, so any number of
//!   fires missed while the server was down collapse into one run (`missed: run_once`)
//!   or one skipped record (`missed: skip`).
//! - Each fire re-checks its owner: a disabled member, or one whose role no longer
//!   allows the action, makes the run `skipped`. A schedule never runs with more
//!   privilege than its owner has at that moment.
//! - Editing a schedule makes the editor its owner, so nobody can make a schedule run as
//!   someone else by editing it.
//! - `new_session` targets wait (bounded) for the agent to become idle, then send the
//!   prompt. `session` targets go through Queue & Steer, so a run that lands mid-turn
//!   waits in the session's queue.

use crate::db::Db;
use crate::orchestrator::Orchestrator;
use om_core::auth::{Action, MemberRecord};
use om_core::event::EventPayload;
use om_core::ids::{MemberId, ScheduleId, ScheduleRunId, SessionId, SpaceId};
use om_core::queue::SubmitMode;
use om_core::schedule::{
    MissedRunPolicy, NewSchedule, OverlapPolicy, RunOutcome, Schedule, ScheduleRun, ScheduleTarget,
    SkipReason,
};
use om_core::session::{SessionConfig, SessionStatus};
use om_core::{CoreError, CoreResult, now_ms};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

mod dispatch;
mod goals;

pub use dispatch::Dispatcher;
pub use goals::GoalRunner;

/// A fire later than this after its slot counts as missed (server was down or busy).
pub const MISSED_GRACE_MS: i64 = 60_000;
/// Longest the loop sleeps between checks, so wall-clock jumps are noticed.
const MAX_SLEEP_MS: i64 = 60_000;
/// Default bound on waiting for a new session's agent to become idle.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Most runs `runs` returns.
pub const MAX_RUNS_PAGE: usize = 200;

/// Handle to the scheduler. Cheap to clone.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    orch: Orchestrator,
    wake: Notify,
    stopped: AtomicBool,
    ready_timeout: Duration,
    /// Schedules with a `NewSession` fire in progress (review P1-12): the overlap check
    /// cannot rely on the recorded run, which only exists once the fire finishes.
    starting: std::sync::Mutex<std::collections::HashSet<ScheduleId>>,
    /// Bounds fires running at once.
    fire_slots: Arc<tokio::sync::Semaphore>,
}

/// Fires running at once across all schedules; more wait for a slot.
pub const MAX_CONCURRENT_FIRES: usize = 8;

/// Marks a schedule as starting a session until dropped.
struct StartingGuard<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<ScheduleId>>,
    id: ScheduleId,
}

impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.id);
        }
    }
}

impl Scheduler {
    pub fn new(orch: Orchestrator) -> Self {
        Self::with_ready_timeout(orch, DEFAULT_READY_TIMEOUT)
    }

    /// Like [`Scheduler::new`] with a custom bound on waiting for new sessions.
    pub fn with_ready_timeout(orch: Orchestrator, ready_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                db: orch.db().clone(),
                orch,
                wake: Notify::new(),
                stopped: AtomicBool::new(false),
                ready_timeout,
                starting: std::sync::Mutex::default(),
                fire_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FIRES)),
            }),
        }
    }

    fn db(&self) -> &Db {
        &self.inner.db
    }

    /// Creates a schedule owned by `by`.
    pub async fn create(
        &self,
        space_id: SpaceId,
        by: &MemberId,
        new: NewSchedule,
    ) -> CoreResult<Schedule> {
        new.validate()?;
        self.check_owner(&space_id, by, &new.target).await?;
        let now = now_ms();
        let next_run_at = Self::first_fire(&new, now)?;
        let s = Schedule {
            id: ScheduleId::new(),
            space_id,
            name: new.name.trim().to_owned(),
            spec: new.spec,
            target: new.target,
            created_by: by.clone(),
            enabled: new.enabled,
            missed: new.missed,
            overlap: new.overlap,
            next_run_at: if new.enabled { next_run_at } else { None },
            last_run_at: None,
            created_at: now,
            updated_at: now,
        };
        let s = self.db().create_schedule(s).await?;
        self.inner.wake.notify_one();
        Ok(s)
    }

    /// Replaces a schedule's definition. The editor becomes its owner.
    pub async fn update(
        &self,
        id: &ScheduleId,
        by: &MemberId,
        new: NewSchedule,
    ) -> CoreResult<Schedule> {
        new.validate()?;
        let old = self.schedule_for(id, by).await?;
        self.check_owner(&old.space_id, by, &new.target).await?;
        let now = now_ms();
        let next_run_at = Self::first_fire(&new, now)?;
        let s = Schedule {
            name: new.name.trim().to_owned(),
            spec: new.spec,
            target: new.target,
            created_by: by.clone(),
            enabled: new.enabled,
            missed: new.missed,
            overlap: new.overlap,
            next_run_at: if new.enabled { next_run_at } else { None },
            updated_at: now,
            ..old
        };
        let s = self.db().update_schedule(s).await?;
        self.inner.wake.notify_one();
        Ok(s)
    }

    /// Pauses or resumes a schedule. Resuming picks the next slot after now; slots that
    /// passed while it was paused are not run.
    /// `by` must be the schedule's owner or an admin of its space; resuming also
    /// needs the right to do what the schedule does.
    pub async fn set_enabled(
        &self,
        id: &ScheduleId,
        by: &MemberId,
        enabled: bool,
    ) -> CoreResult<Schedule> {
        let old = self.schedule_for(id, by).await?;
        self.check_manages(&old, by).await?;
        if enabled {
            self.check_owner(&old.space_id, by, &old.target).await?;
        }
        if old.enabled == enabled {
            return Ok(old);
        }
        let now = now_ms();
        let next = if enabled {
            old.spec.next_after(now)
        } else {
            None
        };
        let s = self
            .db()
            .set_schedule_enabled(id, enabled, next, now)
            .await?;
        self.audit(
            by,
            if enabled {
                "schedule.resumed"
            } else {
                "schedule.paused"
            },
            &s,
        )
        .await;
        self.inner.wake.notify_one();
        Ok(s)
    }

    /// Deletes a schedule; `by` must be its owner or an admin of its space.
    pub async fn delete(&self, id: &ScheduleId, by: &MemberId) -> CoreResult<()> {
        let s = self.schedule_for(id, by).await?;
        self.check_manages(&s, by).await?;
        self.db().delete_schedule(id).await?;
        self.audit(by, "schedule.deleted", &s).await;
        Ok(())
    }

    /// Loads a schedule for `by`; a schedule of another space looks like `NotFound`.
    async fn schedule_for(&self, id: &ScheduleId, by: &MemberId) -> CoreResult<Schedule> {
        let s = self.db().get_schedule(id).await?;
        match self.db().get_member(by).await {
            Ok(m) if m.space_id == s.space_id => Ok(s),
            Ok(_) | Err(CoreError::NotFound(_)) => {
                Err(CoreError::NotFound(format!("schedule {id}")))
            }
            Err(e) => Err(e),
        }
    }

    /// The owner, or an active admin of the schedule's space.
    async fn check_manages(&self, s: &Schedule, by: &MemberId) -> CoreResult<()> {
        let m = self.db().get_member(by).await?;
        if !m.is_active() {
            return Err(CoreError::Forbidden("member is not active".into()));
        }
        if &s.created_by == by || m.role.can(Action::ManageMembers) {
            Ok(())
        } else {
            Err(CoreError::Forbidden(
                "only the schedule's owner or an admin may change it".into(),
            ))
        }
    }

    async fn audit(&self, by: &MemberId, action: &str, s: &Schedule) {
        let detail = serde_json::json!({"name": s.name, "enabled": s.enabled});
        if let Err(e) = self
            .db()
            .audit(Some(by.clone()), action, s.id.as_str(), detail)
            .await
        {
            tracing::warn!(schedule = %s.id, error = %e, "schedule audit entry not written");
        }
    }

    pub async fn get(&self, id: &ScheduleId) -> CoreResult<Schedule> {
        self.db().get_schedule(id).await
    }

    pub async fn list(&self, space_id: &SpaceId) -> CoreResult<Vec<Schedule>> {
        self.db().list_schedules(space_id).await
    }

    /// A schedule's runs, newest first (at most [`MAX_RUNS_PAGE`]).
    pub async fn runs(&self, id: &ScheduleId, limit: usize) -> CoreResult<Vec<ScheduleRun>> {
        self.db()
            .list_schedule_runs(id, limit.clamp(1, MAX_RUNS_PAGE))
            .await
    }

    /// Fires a schedule now, outside its timetable, without moving its next slot. `by`
    /// must be allowed to do what the schedule does; the run still acts as the owner.
    pub async fn trigger(&self, id: &ScheduleId, by: &MemberId) -> CoreResult<ScheduleRun> {
        let s = self.schedule_for(id, by).await?;
        self.check_owner(&s.space_id, by, &s.target).await?;
        let now = now_ms();
        Ok(self.fire(s, now, true).await)
    }

    /// Fires every schedule due at `now`. Each fire runs on its own task; the handles
    /// resolve to the recorded runs.
    pub async fn tick(&self, now: i64) -> CoreResult<Vec<JoinHandle<ScheduleRun>>> {
        let mut fired = Vec::new();
        for s in self.db().due_schedules(now).await? {
            let Some(slot) = s.next_run_at else { continue };
            let next = s.spec.next_after(now);
            if !self
                .db()
                .claim_schedule_fire(&s.id, slot, next, now)
                .await?
            {
                continue;
            }
            let this = self.clone();
            let slots = self.inner.fire_slots.clone();
            fired.push(tokio::spawn(async move {
                let _slot = slots.acquire_owned().await;
                if now - slot > MISSED_GRACE_MS && s.missed == MissedRunPolicy::Skip {
                    let run = ScheduleRun {
                        id: ScheduleRunId::new(),
                        schedule_id: s.id.clone(),
                        scheduled_for: slot,
                        fired_at: now,
                        outcome: RunOutcome::Skipped {
                            reason: SkipReason::Missed,
                        },
                        manual: false,
                    };
                    this.record(run).await
                } else {
                    this.fire(s, slot, false).await
                }
            }));
        }
        Ok(fired)
    }

    /// Runs the timetable until [`Scheduler::shutdown`].
    pub fn spawn(&self) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            while !this.inner.stopped.load(Ordering::SeqCst) {
                if let Err(e) = this.tick(now_ms()).await {
                    tracing::warn!(error = %e, "schedule tick failed");
                }
                let now = now_ms();
                let wait = match this.db().next_schedule_due().await {
                    Ok(Some(at)) => (at - now).clamp(0, MAX_SLEEP_MS),
                    Ok(None) => MAX_SLEEP_MS,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading next schedule failed");
                        MAX_SLEEP_MS
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(wait as u64)) => {}
                    _ = this.inner.wake.notified() => {}
                }
            }
        })
    }

    /// Stops the loop started by [`Scheduler::spawn`]. Fires already running finish.
    pub fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
    }

    fn first_fire(new: &NewSchedule, now: i64) -> CoreResult<Option<i64>> {
        match new.spec.next_after(now) {
            Some(t) => Ok(Some(t)),
            None => Err(CoreError::InvalidInput(
                "the timetable has no fire in the future".into(),
            )),
        }
    }

    fn action_for(target: &ScheduleTarget) -> Action {
        match target {
            ScheduleTarget::NewSession { .. } => Action::CreateSession,
            ScheduleTarget::Session { .. } => Action::PromptSession,
        }
    }

    /// The member `by` must be active in `space_id` and allowed the target's action, and
    /// a session target must belong to the same space.
    async fn check_owner(
        &self,
        space_id: &SpaceId,
        by: &MemberId,
        target: &ScheduleTarget,
    ) -> CoreResult<MemberRecord> {
        let m = self
            .db()
            .get_member(by)
            .await
            .map_err(|_| CoreError::Forbidden("unknown member".into()))?;
        if !m.is_active() || &m.space_id != space_id {
            return Err(CoreError::Forbidden(
                "member is not active in this space".into(),
            ));
        }
        if !m.role.can(Self::action_for(target)) {
            return Err(CoreError::Forbidden(
                "role does not allow what this schedule does".into(),
            ));
        }
        if let ScheduleTarget::Session { session_id, .. } = target {
            let s = self.db().get_session(session_id).await?;
            if &s.space_id != space_id {
                return Err(CoreError::NotFound(format!("session {session_id}")));
            }
        }
        Ok(m)
    }

    async fn record(&self, run: ScheduleRun) -> ScheduleRun {
        if let Err(e) = self.db().record_schedule_run(run.clone()).await {
            tracing::warn!(schedule = %run.schedule_id, error = %e, "recording schedule run failed");
        }
        run
    }

    async fn fire(&self, s: Schedule, slot: i64, manual: bool) -> ScheduleRun {
        let outcome = self.outcome(&s).await;
        let run = ScheduleRun {
            id: ScheduleRunId::new(),
            schedule_id: s.id.clone(),
            scheduled_for: slot,
            fired_at: now_ms(),
            outcome,
            manual,
        };
        tracing::info!(schedule = %s.id, manual, outcome = ?run.outcome, "schedule fired");
        self.record(run).await
    }

    async fn outcome(&self, s: &Schedule) -> RunOutcome {
        let skipped = |reason| RunOutcome::Skipped { reason };
        let failed = |e: CoreError, session_id| RunOutcome::Failed {
            error: e.to_string(),
            session_id,
        };
        // Re-check the owner at fire time.
        match self.db().get_member(&s.created_by).await {
            Ok(m) if !m.is_active() || m.space_id != s.space_id => {
                return skipped(SkipReason::CreatorDisabled);
            }
            Ok(m) if !m.role.can(Self::action_for(&s.target)) => {
                return skipped(SkipReason::CreatorNotAllowed);
            }
            Ok(_) => {}
            Err(CoreError::NotFound(_)) => return skipped(SkipReason::CreatorDisabled),
            Err(e) => return failed(e, None),
        }
        let owner = Some(s.created_by.clone());
        match &s.target {
            ScheduleTarget::Session {
                session_id,
                prompt,
                mode,
            } => {
                match self.db().get_session(session_id).await {
                    Ok(sess) if sess.space_id == s.space_id => {}
                    Ok(_) => {
                        return failed(CoreError::NotFound(format!("session {session_id}")), None);
                    }
                    Err(e) => return failed(e, None),
                }
                match self
                    .inner
                    .orch
                    .send_prompt(session_id, owner, prompt.clone(), *mode)
                    .await
                {
                    Ok(submit) => RunOutcome::Delivered {
                        session_id: session_id.clone(),
                        submit,
                    },
                    Err(e) => failed(e, Some(session_id.clone())),
                }
            }
            ScheduleTarget::NewSession { config, prompt } => {
                // Claim the "starting" mark first, atomically, so two fires of one
                // schedule cannot both pass the overlap check.
                let _starting = {
                    let mut set = self
                        .inner
                        .starting
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !set.insert(s.id.clone()) && s.overlap == OverlapPolicy::Skip {
                        return skipped(SkipReason::PreviousRunActive);
                    }
                    StartingGuard {
                        set: &self.inner.starting,
                        id: s.id.clone(),
                    }
                };
                if s.overlap == OverlapPolicy::Skip && self.previous_run_active(&s.id).await {
                    return skipped(SkipReason::PreviousRunActive);
                }
                let mut config: SessionConfig = config.clone();
                if config.title.is_none() {
                    config.title = Some(s.name.clone());
                }
                let created = self
                    .inner
                    .orch
                    .create_session(s.space_id.clone(), config, owner.clone())
                    .await;
                let session_id = match created {
                    Ok(summary) => summary.id,
                    Err(e) => return failed(e, None),
                };
                let started =
                    match wait_ready(&self.inner.orch, &session_id, self.inner.ready_timeout).await
                    {
                        Ok(()) => {
                            self.inner
                                .orch
                                .send_prompt(&session_id, owner, prompt.clone(), SubmitMode::Queue)
                                .await
                        }
                        Err(e) => Err(e),
                    };
                match started {
                    Ok(submit) => RunOutcome::SessionStarted { session_id, submit },
                    Err(e) => {
                        // Close the session this run created so it neither holds sandbox
                        // resources nor blocks later fires as an "active" previous run.
                        if let Err(ce) = self.inner.orch.close_session(&session_id).await {
                            tracing::warn!(session = %session_id, error = %ce, "closing failed scheduled session");
                        }
                        failed(e, Some(session_id))
                    }
                }
            }
        }
    }

    async fn previous_run_active(&self, id: &ScheduleId) -> bool {
        let Ok(Some(prev)) = self.db().last_schedule_session(id).await else {
            return false;
        };
        matches!(
            self.db().get_session(&prev).await.map(|s| s.status),
            Ok(SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval)
        )
    }
}

/// Waits until the new session's agent is idle.
pub(crate) async fn wait_ready(
    orch: &Orchestrator,
    id: &SessionId,
    timeout: Duration,
) -> CoreResult<()> {
    let mut sub = orch.subscribe(id, 0).await?;
    let wait = async {
        loop {
            let Some(ev) = sub.next().await? else {
                return Err(CoreError::Harness(
                    "session ended before it was ready".into(),
                ));
            };
            if let EventPayload::StatusChanged { to, .. } = ev.payload {
                match to {
                    SessionStatus::Idle => return Ok(()),
                    SessionStatus::Failed | SessionStatus::Closed => {
                        return Err(CoreError::Harness(format!(
                            "session became {} before it was ready",
                            serde_json::to_value(to)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .unwrap_or_default()
                        )));
                    }
                    _ => {}
                }
            }
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| CoreError::Harness("agent did not become ready in time".into()))?
}
