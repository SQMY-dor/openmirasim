//! Schedules contract (W4-1).
//!
//! A schedule sends a prompt on a timetable, as the member who created it:
//!
//! - [`ScheduleTarget::NewSession`] starts a fresh session each time (a nightly review,
//!   a weekly dependency bump) and gives it the prompt once the agent is ready.
//! - [`ScheduleTarget::Session`] sends the prompt into one existing session with Queue &
//!   Steer semantics, so a run that lands mid-turn waits in that session's queue.
//!
//! Timetables are deliberately small and dependency-free ([`ScheduleSpec`]): a single
//! instant, a fixed interval, or a time of day on chosen weekdays at a fixed UTC offset.
//! Times are Unix milliseconds like everywhere else on the wire.
//!
//! Every fire is checked again at run time: a disabled creator, or one whose role no
//! longer allows the action, makes the run `skipped`, never silently privileged. Each
//! fire leaves one [`ScheduleRun`] row.

use crate::content::Prompt;
use crate::ids::{MemberId, QueuedPromptId, ScheduleId, ScheduleRunId, SessionId, SpaceId};
use crate::queue::{SubmitMode, SubmitOutcome};
use crate::session::SessionConfig;
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Shortest interval an `every` schedule may use.
pub const MIN_INTERVAL_SECS: u64 = 60;
/// Most schedules a space holds.
pub const MAX_SCHEDULES_PER_SPACE: usize = 200;
/// Longest schedule name, in characters.
pub const MAX_SCHEDULE_NAME_CHARS: usize = 120;

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

/// A day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    /// Monday first.
    pub const ALL: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];

    /// The weekday of a day counted from the Unix epoch (1970-01-01 was a Thursday).
    fn of_epoch_day(day: i64) -> Weekday {
        Self::ALL[(day + 3).rem_euclid(7) as usize]
    }
}

/// Latest timestamp a schedule accepts (9999-12-31T23:59:59.999Z), in Unix ms.
pub const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;

/// When a schedule fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum ScheduleSpec {
    /// Once, at `at`.
    Once {
        #[ts(type = "number")]
        at: i64,
    },
    /// Every `interval_secs`, counted from `start_at` (the first fire).
    Every {
        #[ts(type = "number")]
        interval_secs: u64,
        #[ts(type = "number")]
        start_at: i64,
    },
    /// At `minute_of_day` local time (0..1440) on `weekdays` (empty = every day), where
    /// local time is UTC shifted by `utc_offset_minutes` (e.g. 480 for UTC+8). A fixed
    /// offset has no daylight-saving changes; clients convert from the member's zone.
    Daily {
        minute_of_day: u16,
        #[serde(default)]
        utc_offset_minutes: i16,
        #[serde(default)]
        weekdays: Vec<Weekday>,
    },
}

impl ScheduleSpec {
    /// Checks the timetable's fields.
    pub fn validate(&self) -> CoreResult<()> {
        match self {
            ScheduleSpec::Once { at } if *at <= 0 || *at > MAX_TIMESTAMP_MS => Err(
                CoreError::InvalidInput("`at` must be a timestamp between 1970 and 9999".into()),
            ),
            ScheduleSpec::Every { start_at, .. }
                if *start_at <= 0 || *start_at > MAX_TIMESTAMP_MS =>
            {
                Err(CoreError::InvalidInput(
                    "`start_at` must be a timestamp between 1970 and 9999".into(),
                ))
            }
            ScheduleSpec::Every { interval_secs, .. } if *interval_secs < MIN_INTERVAL_SECS => {
                Err(CoreError::InvalidInput(format!(
                    "interval must be at least {MIN_INTERVAL_SECS} seconds"
                )))
            }
            ScheduleSpec::Every { interval_secs, .. } if *interval_secs > 366 * 86_400 => Err(
                CoreError::InvalidInput("interval must be at most 366 days".into()),
            ),
            ScheduleSpec::Daily { minute_of_day, .. } if *minute_of_day >= 1440 => Err(
                CoreError::InvalidInput("`minute_of_day` must be below 1440".into()),
            ),
            ScheduleSpec::Daily {
                utc_offset_minutes, ..
            } if utc_offset_minutes.unsigned_abs() > 14 * 60 => Err(CoreError::InvalidInput(
                "`utc_offset_minutes` must be within ±14 hours".into(),
            )),
            _ => Ok(()),
        }
    }

    /// The first fire time strictly after `after`, or `None` when the timetable is over.
    pub fn next_after(&self, after: i64) -> Option<i64> {
        match *self {
            ScheduleSpec::Once { at } => (at > after).then_some(at),
            ScheduleSpec::Every {
                interval_secs,
                start_at,
            } => {
                if after < start_at {
                    return Some(start_at);
                }
                let step = i64::try_from(interval_secs).ok()?.checked_mul(1000)?;
                if step <= 0 {
                    return None;
                }
                // Checked throughout: a hostile `start_at` must end the timetable, never
                // wrap into the past (which would fire in a tight loop).
                let n = after
                    .checked_sub(start_at)?
                    .checked_div(step)?
                    .checked_add(1)?;
                let next = start_at.checked_add(n.checked_mul(step)?)?;
                (next > after).then_some(next)
            }
            ScheduleSpec::Daily {
                minute_of_day,
                utc_offset_minutes,
                ref weekdays,
            } => {
                let offset = utc_offset_minutes as i64 * MINUTE_MS;
                let local_day = (after + offset).div_euclid(DAY_MS);
                // Today's slot may still be ahead; otherwise one of the next 7 days is.
                (0..=7).find_map(|d| {
                    let day = local_day + d;
                    let at = day * DAY_MS + minute_of_day as i64 * MINUTE_MS - offset;
                    let wanted =
                        weekdays.is_empty() || weekdays.contains(&Weekday::of_epoch_day(day));
                    (at > after && wanted).then_some(at)
                })
            }
        }
    }
}

/// What a schedule does when it fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum ScheduleTarget {
    /// Start a new session with `config` and send `prompt` once it is ready.
    NewSession {
        config: SessionConfig,
        prompt: Prompt,
    },
    /// Send `prompt` to an existing session of the same space.
    Session {
        session_id: SessionId,
        prompt: Prompt,
        #[serde(default)]
        mode: SubmitMode,
    },
}

impl ScheduleTarget {
    pub fn prompt(&self) -> &Prompt {
        match self {
            ScheduleTarget::NewSession { prompt, .. } | ScheduleTarget::Session { prompt, .. } => {
                prompt
            }
        }
    }
}

/// What happens to fires missed while the server was down. Either way several missed
/// fires collapse into at most one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MissedRunPolicy {
    /// Run once as soon as the server is back.
    #[default]
    RunOnce,
    /// Record the missed fire as skipped and wait for the next one.
    Skip,
}

/// What happens when a `new_session` schedule fires while the session its previous run
/// started is still working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum OverlapPolicy {
    /// Skip this fire.
    #[default]
    Skip,
    /// Start another session anyway.
    Allow,
}

/// A schedule as stored and listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Schedule {
    pub id: ScheduleId,
    pub space_id: SpaceId,
    pub name: String,
    pub spec: ScheduleSpec,
    pub target: ScheduleTarget,
    /// The member the schedule runs as.
    pub created_by: MemberId,
    pub enabled: bool,
    pub missed: MissedRunPolicy,
    pub overlap: OverlapPolicy,
    /// Next fire, or `None` when disabled or the timetable is over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub next_run_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub last_run_at: Option<i64>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

/// Request body to create a schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewSchedule {
    pub name: String,
    pub spec: ScheduleSpec,
    pub target: ScheduleTarget,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub missed: MissedRunPolicy,
    #[serde(default)]
    pub overlap: OverlapPolicy,
}

fn enabled_default() -> bool {
    true
}

impl NewSchedule {
    /// Checks everything that does not need the database.
    pub fn validate(&self) -> CoreResult<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(CoreError::InvalidInput("schedule name is empty".into()));
        }
        if name.chars().count() > MAX_SCHEDULE_NAME_CHARS {
            return Err(CoreError::InvalidInput(format!(
                "schedule name is longer than {MAX_SCHEDULE_NAME_CHARS} characters"
            )));
        }
        if self.target.prompt().is_empty() {
            return Err(CoreError::InvalidInput("schedule prompt is empty".into()));
        }
        self.spec.validate()
    }
}

/// Why a fire did not send anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SkipReason {
    /// The creator has been disabled since.
    CreatorDisabled,
    /// The creator's role no longer allows the action.
    CreatorNotAllowed,
    /// The session the previous run started is still working (`overlap: skip`).
    PreviousRunActive,
    /// The fire was missed while the server was down (`missed: skip`).
    Missed,
}

/// What one fire did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[ts(export)]
pub enum RunOutcome {
    /// A new session was started and given the prompt.
    SessionStarted {
        session_id: SessionId,
        submit: SubmitOutcome,
    },
    /// The prompt went to the target session (started, queued or steered).
    Delivered {
        session_id: SessionId,
        submit: SubmitOutcome,
    },
    Skipped {
        reason: SkipReason,
    },
    /// Something refused the run; `session_id` is set when a session had been created.
    Failed {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<SessionId>,
    },
}

impl RunOutcome {
    /// The session this run created or prompted, if any.
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            RunOutcome::SessionStarted { session_id, .. }
            | RunOutcome::Delivered { session_id, .. } => Some(session_id),
            RunOutcome::Failed { session_id, .. } => session_id.as_ref(),
            RunOutcome::Skipped { .. } => None,
        }
    }

    /// The queued prompt this run left behind, if it was queued.
    pub fn queued_id(&self) -> Option<&QueuedPromptId> {
        match self {
            RunOutcome::SessionStarted { submit, .. } | RunOutcome::Delivered { submit, .. } => {
                match submit {
                    SubmitOutcome::Queued { queued_id, .. }
                    | SubmitOutcome::Interrupting { queued_id } => Some(queued_id),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// One fire of a schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ScheduleRun {
    pub id: ScheduleRunId,
    pub schedule_id: ScheduleId,
    /// The timetable slot this run is for.
    #[ts(type = "number")]
    pub scheduled_for: i64,
    /// When it actually fired (later than `scheduled_for` after downtime, or for a
    /// `new_session` run, when the prompt was sent).
    #[ts(type = "number")]
    pub fired_at: i64,
    pub outcome: RunOutcome,
    /// `true` when a member fired it by hand rather than the timetable.
    #[serde(default)]
    pub manual: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    // 2026-11-23 00:00 UTC, a Monday.
    const MON: i64 = 1_795_392_000_000;

    #[test]
    fn epoch_day_weekdays() {
        assert_eq!(Weekday::of_epoch_day(0), Weekday::Thu);
        assert_eq!(Weekday::of_epoch_day(MON / DAY_MS), Weekday::Mon);
        assert_eq!(Weekday::of_epoch_day(-1), Weekday::Wed);
    }

    #[test]
    fn once_and_every() {
        let once = ScheduleSpec::Once { at: MON };
        assert_eq!(once.next_after(MON - 1), Some(MON));
        assert_eq!(once.next_after(MON), None);

        let every = ScheduleSpec::Every {
            interval_secs: 3600,
            start_at: MON,
        };
        assert_eq!(every.next_after(MON - 5), Some(MON));
        assert_eq!(every.next_after(MON), Some(MON + H));
        assert_eq!(every.next_after(MON + H - 1), Some(MON + H));
        // Long downtime collapses to the next slot after `after`.
        assert_eq!(every.next_after(MON + 10 * H + 5), Some(MON + 11 * H));
    }

    #[test]
    fn daily_with_offset_and_weekdays() {
        // 09:30 at UTC+8 is 01:30 UTC.
        let daily = ScheduleSpec::Daily {
            minute_of_day: 9 * 60 + 30,
            utc_offset_minutes: 480,
            weekdays: vec![],
        };
        let slot = MON + H + 30 * 60_000;
        assert_eq!(daily.next_after(MON), Some(slot));
        assert_eq!(daily.next_after(slot), Some(slot + DAY_MS));

        // Weekdays only: from Friday's slot the next is Monday's.
        let workdays = ScheduleSpec::Daily {
            minute_of_day: 9 * 60 + 30,
            utc_offset_minutes: 480,
            weekdays: vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ],
        };
        let fri = slot + 4 * DAY_MS;
        assert_eq!(workdays.next_after(fri), Some(slot + 7 * DAY_MS));

        // A local day differs from the UTC day: 23:00 at UTC-5 on Monday is 04:00 UTC
        // Tuesday, and only Monday is allowed.
        let late = ScheduleSpec::Daily {
            minute_of_day: 23 * 60,
            utc_offset_minutes: -300,
            weekdays: vec![Weekday::Mon],
        };
        assert_eq!(late.next_after(MON), Some(MON + DAY_MS + 4 * H));
        assert_eq!(
            late.next_after(MON + DAY_MS + 4 * H),
            Some(MON + 8 * DAY_MS + 4 * H)
        );
    }

    #[test]
    fn validation() {
        let bad = [
            ScheduleSpec::Once { at: 0 },
            ScheduleSpec::Every {
                interval_secs: 59,
                start_at: MON,
            },
            ScheduleSpec::Daily {
                minute_of_day: 1440,
                utc_offset_minutes: 0,
                weekdays: vec![],
            },
            ScheduleSpec::Daily {
                minute_of_day: 0,
                utc_offset_minutes: 15 * 60,
                weekdays: vec![],
            },
        ];
        for s in bad {
            assert!(s.validate().is_err(), "{s:?}");
        }
        let n = NewSchedule {
            name: "  ".into(),
            spec: ScheduleSpec::Once { at: MON },
            target: ScheduleTarget::Session {
                session_id: SessionId::from("s"),
                prompt: Prompt::text("hi"),
                mode: SubmitMode::Queue,
            },
            enabled: true,
            missed: MissedRunPolicy::RunOnce,
            overlap: OverlapPolicy::Skip,
        };
        assert!(n.validate().is_err());
    }

    #[test]
    fn wire_shapes() {
        let spec: ScheduleSpec = serde_json::from_value(serde_json::json!({
            "kind": "daily", "minute_of_day": 570, "utc_offset_minutes": 480,
            "weekdays": ["mon", "fri"]
        }))
        .unwrap();
        assert!(matches!(spec, ScheduleSpec::Daily { ref weekdays, .. } if weekdays.len() == 2));
        let new: NewSchedule = serde_json::from_value(serde_json::json!({
            "name": "nightly",
            "spec": {"kind": "every", "interval_secs": 3600, "start_at": MON},
            "target": {"kind": "session", "session_id": "s1",
                       "prompt": {"content": [{"type": "text", "text": "go"}]}}
        }))
        .unwrap();
        assert!(new.enabled);
        assert_eq!(new.missed, MissedRunPolicy::RunOnce);
        assert_eq!(new.overlap, OverlapPolicy::Skip);
        let out = serde_json::to_value(RunOutcome::Skipped {
            reason: SkipReason::CreatorDisabled,
        })
        .unwrap();
        assert_eq!(
            out,
            serde_json::json!({"outcome": "skipped", "reason": "creator_disabled"})
        );
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    #[test]
    fn hostile_every_specs_end_instead_of_wrapping() {
        let bad = ScheduleSpec::Every {
            interval_secs: 60,
            start_at: i64::MIN,
        };
        assert!(bad.validate().is_err());
        assert_eq!(bad.next_after(1_700_000_000_000), None);
        let huge = ScheduleSpec::Every {
            interval_secs: u64::MAX,
            start_at: 1,
        };
        assert_eq!(huge.next_after(10), None);
        let ok = ScheduleSpec::Every {
            interval_secs: 60,
            start_at: 1_000,
        };
        assert_eq!(ok.next_after(1_000), Some(61_000));
        assert!(ScheduleSpec::Once { at: i64::MAX }.validate().is_err());
    }
}
