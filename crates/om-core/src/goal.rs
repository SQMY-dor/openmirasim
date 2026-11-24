//! Goals contract (W4-1).
//!
//! A goal keeps one session working in a loop until an objective is met:
//!
//! 1. Setting a goal sends the objective to the session as a prompt, with an instruction
//!    to print [`Goal::done_marker`] on its own line once the objective is met.
//! 2. Each time a turn ends, the server checks the turn's reply for the marker. If it is
//!    there, the goal is `achieved`; otherwise, while turns, time and permissions remain,
//!    it sends a short continuation prompt and the loop goes on.
//! 3. The loop stops on the first of: the marker, `max_turns` turns, `deadline_at`, a
//!    member stopping the goal, a cancelled or failed turn, or the session ending.
//!
//! The loop acts as the member who set the goal, re-checked before every continuation, so
//! a goal never outlives its owner's permission to prompt the session. A session has at
//! most one active goal. Goals do not survive a server restart: active goals are marked
//! `stopped` (reason `server_restarted`) when the server starts.

use crate::ids::{GoalId, MemberId, SessionId, SpaceId};
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Most turns a goal may run.
pub const MAX_GOAL_TURNS: u32 = 50;
/// Turns a goal runs when the request names none.
pub const DEFAULT_GOAL_TURNS: u32 = 10;
/// Longest objective, in characters.
pub const MAX_OBJECTIVE_CHARS: usize = 8_000;
/// The marker the agent prints when the objective is met.
pub const DEFAULT_DONE_MARKER: &str = "[[goal:done]]";

/// Where a goal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum GoalStatus {
    /// The loop is running.
    Active,
    /// The agent reported the objective met.
    Achieved,
    /// `max_turns` turns ran without the objective being met.
    Exhausted,
    /// `deadline_at` passed.
    Expired,
    /// A member stopped it, its owner lost permission, a turn was cancelled, or the server
    /// restarted (see `ended_reason`).
    Stopped,
    /// A turn failed or the session ended.
    Failed,
}

impl GoalStatus {
    pub fn is_active(self) -> bool {
        self == Self::Active
    }
}

/// A goal on a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Goal {
    pub id: GoalId,
    pub space_id: SpaceId,
    pub session_id: SessionId,
    pub objective: String,
    pub done_marker: String,
    pub max_turns: u32,
    /// Turns that ended since the goal was set.
    pub turns_used: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub deadline_at: Option<i64>,
    /// The member the loop acts as.
    pub created_by: MemberId,
    pub status: GoalStatus,
    /// Why the goal left `active`, in a word or two (`max_turns`, `turn_cancelled`,
    /// `stopped_by_member`, `owner_not_allowed`, `server_restarted`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ended_reason: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

impl Goal {
    /// The first prompt: the objective plus the completion contract.
    pub fn opening_prompt(&self) -> String {
        format!(
            "{}\n\nWork on this goal until it is fully met. When it is met, print `{}` on a \
             line of its own and stop. Do not print it before then.",
            self.objective.trim(),
            self.done_marker
        )
    }

    /// The prompt sent after a turn that did not meet the goal.
    pub fn continuation_prompt(&self) -> String {
        format!(
            "Continue working toward the goal (turn {} of {}). If it is now fully met, print \
             `{}` on a line of its own.",
            self.turns_used + 1,
            self.max_turns,
            self.done_marker
        )
    }

    /// Whether `reply` (the agent's text for one turn) reports the goal met: the marker on
    /// a line of its own outside fenced code blocks, ignoring surrounding whitespace and backticks.
    pub fn is_met_by(&self, reply: &str) -> bool {
        let mut fence: Option<char> = None;
        for raw in reply.lines() {
            let trimmed = raw.trim();
            if let Some(ch) = fence {
                if trimmed.starts_with(ch) && trimmed.chars().take_while(|&c| c == ch).count() >= 3 {
                    fence = None;
                }
                continue;
            }
            if trimmed.starts_with("```") {
                fence = Some('`');
                continue;
            }
            if trimmed.starts_with("~~~") {
                fence = Some('~');
                continue;
            }
            if trimmed.trim_matches('`').trim() == self.done_marker {
                return true;
            }
        }
        false
    }
}

/// Body for setting a goal on a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewGoal {
    pub objective: String,
    /// Defaults to [`DEFAULT_GOAL_TURNS`]; at most [`MAX_GOAL_TURNS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub deadline_at: Option<i64>,
}

impl NewGoal {
    /// Checks the request against `now`; returns the effective turn budget.
    pub fn validate(&self, now: i64) -> CoreResult<u32> {
        let objective = self.objective.trim();
        if objective.is_empty() {
            return Err(CoreError::InvalidInput("goal objective is empty".into()));
        }
        if objective.chars().count() > MAX_OBJECTIVE_CHARS {
            return Err(CoreError::InvalidInput(format!(
                "goal objective is longer than {MAX_OBJECTIVE_CHARS} characters"
            )));
        }
        if objective.contains(DEFAULT_DONE_MARKER) {
            return Err(CoreError::InvalidInput(
                "goal objective must not contain the done marker".into(),
            ));
        }
        let turns = self.max_turns.unwrap_or(DEFAULT_GOAL_TURNS);
        if !(1..=MAX_GOAL_TURNS).contains(&turns) {
            return Err(CoreError::InvalidInput(format!(
                "max_turns must be between 1 and {MAX_GOAL_TURNS}"
            )));
        }
        if let Some(d) = self.deadline_at {
            if d <= now {
                return Err(CoreError::InvalidInput("deadline_at is in the past".into()));
            }
            let max_deadline = now.saturating_add(366_i64 * 86_400 * 1000);
            if d > crate::schedule::MAX_TIMESTAMP_MS || d > max_deadline {
                return Err(CoreError::InvalidInput(
                    "deadline_at must be within 366 days".into(),
                ));
            }
        }
        Ok(turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal {
            id: GoalId::from("g"),
            space_id: SpaceId::from("sp"),
            session_id: SessionId::from("s"),
            objective: "make the tests pass".into(),
            done_marker: DEFAULT_DONE_MARKER.into(),
            max_turns: 3,
            turns_used: 0,
            deadline_at: None,
            created_by: MemberId::from("m"),
            status: GoalStatus::Active,
            ended_reason: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn marker_must_be_on_its_own_line() {
        let g = goal();
        assert!(g.is_met_by("all green\n[[goal:done]]\n"));
        assert!(g.is_met_by("all green\n  `[[goal:done]]`  "));
        assert!(!g.is_met_by("I will print [[goal:done]] when finished"));
        assert!(!g.is_met_by("not yet"));
        assert!(g.opening_prompt().contains("make the tests pass"));
        assert!(g.continuation_prompt().contains("turn 1 of 3"));
    }

    #[test]
    fn validation() {
        let ok = NewGoal {
            objective: " fix it ".into(),
            max_turns: None,
            deadline_at: Some(100),
        };
        assert_eq!(ok.validate(50).unwrap(), DEFAULT_GOAL_TURNS);
        assert!(ok.validate(100).is_err());
        for bad in [
            NewGoal {
                objective: "  ".into(),
                ..ok.clone()
            },
            NewGoal {
                max_turns: Some(0),
                ..ok.clone()
            },
            NewGoal {
                max_turns: Some(MAX_GOAL_TURNS + 1),
                ..ok.clone()
            },
            NewGoal {
                objective: "say [[goal:done]]".into(),
                ..ok.clone()
            },
        ] {
            assert!(matches!(bad.validate(0), Err(CoreError::InvalidInput(_))));
        }
    }

    #[test]
    fn wire_shape() {
        let v = serde_json::to_value(goal()).unwrap();
        assert_eq!(v["status"], "active");
        assert!(v.get("deadline_at").is_none());
        let n: NewGoal = serde_json::from_str(r#"{"objective":"x"}"#).unwrap();
        assert_eq!(n.max_turns, None);
    }
}
