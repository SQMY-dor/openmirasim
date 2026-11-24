//! W4-1 goals and Session-to-Session dispatch against a real orchestrator.
//!
//! The script harness answers every prompt with one text chunk and ends the turn:
//! - a prompt containing `done-on-K` makes the session print the done marker on its K-th
//!   prompt (counting that one);
//! - `refuse` ends the turn with `refusal`;
//! - `hold` keeps the turn open until cancelled (`soft-hold`: the cancel ends it
//!   normally, standing in for a long turn that finishes).

use async_trait::async_trait;
use om_core::CoreError;
use om_core::approval::ApprovalDecision;
use om_core::auth::Role;
use om_core::content::{ContentBlock, Prompt};
use om_core::dispatch::{DispatchTarget, MAX_DISPATCH_DEPTH, NewDispatch};
use om_core::goal::{GoalStatus, NewGoal};
use om_core::harness::{
    Harness, HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::ids::{GoalId, MemberId, SessionId, SpaceId};
use om_core::now_ms;
use om_core::queue::{SubmitMode, SubmitOutcome};
use om_core::sandbox::SandboxMode;
use om_core::session::{SessionConfig, SessionStatus, StopReason};
use om_server::db::Db;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig};
use om_server::scheduler::{Dispatcher, GoalRunner};
use om_server::stream::ReceiverStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Default)]
struct State {
    prompts: Vec<String>,
    done_on: Option<usize>,
    holding: bool,
    /// Cancelling a held `soft-hold` turn ends it normally instead.
    soft: bool,
}

struct ScriptSession {
    tx: mpsc::UnboundedSender<HarnessEvent>,
    state: Mutex<State>,
}

fn text(p: &Prompt) -> String {
    p.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[async_trait]
impl HarnessSession for ScriptSession {
    fn native_session_id(&self) -> Option<String> {
        None
    }

    async fn prompt(&self, prompt: Prompt) -> om_core::CoreResult<()> {
        let t = text(&prompt);
        let mut st = self.state.lock().unwrap();
        st.prompts.push(t.clone());
        if let Some(i) = t.find("done-on-") {
            let k: String = t[i + 8..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            st.done_on = k.parse().ok();
        }
        if t.contains("hold") {
            st.holding = true;
            st.soft = t.contains("soft-hold");
            return Ok(());
        }
        let n = st.prompts.len();
        let reply = if st.done_on.is_some_and(|k| n >= k) {
            // Extract the per-goal done marker instructed by the latest goal prompt.
            let marker = st
                .prompts
                .iter()
                .rev()
                .find_map(|p| {
                    let start = p.find("[[goal:done")?;
                    let rest = &p[start..];
                    let end = rest.find("]]")? + 2;
                    Some(rest[..end].to_string())
                })
                .unwrap_or_else(|| "[[goal:done]]".into());
            format!("finished on prompt {n}\n{marker}\n")
        } else {
            format!("still working ({n})")
        };
        let _ = self.tx.send(HarnessEvent::MessageChunk(ContentBlock::Text {
            text: reply,
        }));
        let stop_reason = if t.contains("refuse") {
            StopReason::Refusal
        } else {
            StopReason::EndTurn
        };
        let _ = self.tx.send(HarnessEvent::TurnEnded {
            stop_reason,
            error: None,
        });
        Ok(())
    }

    async fn respond_approval(&self, _: &str, _: ApprovalDecision) -> om_core::CoreResult<()> {
        Ok(())
    }

    async fn cancel(&self) -> om_core::CoreResult<()> {
        let mut st = self.state.lock().unwrap();
        if st.holding {
            st.holding = false;
            let _ = self.tx.send(HarnessEvent::TurnEnded {
                stop_reason: if st.soft {
                    StopReason::EndTurn
                } else {
                    StopReason::Cancelled
                },
                error: None,
            });
        }
        Ok(())
    }

    async fn shutdown(&self) -> om_core::CoreResult<()> {
        Ok(())
    }
}

struct ScriptHarness;

#[async_trait]
impl Harness for ScriptHarness {
    fn id(&self) -> &str {
        "script"
    }

    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }

    async fn start(&self, _ctx: StartContext) -> om_core::CoreResult<StartedSession> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(StartedSession {
            session: Box::new(ScriptSession {
                tx,
                state: Mutex::default(),
            }),
            events: Box::pin(ReceiverStream::new(rx)),
        })
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    db: Db,
    orch: Orchestrator,
    goals: GoalRunner,
    dispatcher: Dispatcher,
    space: SpaceId,
    member: MemberId,
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let member = db
        .create_member(&space, "alice", Role::Member, None)
        .await
        .unwrap()
        .id;
    let r = HarnessRegistry::new();
    r.register(Arc::new(ScriptHarness));
    let orch = Orchestrator::new(
        db.clone(),
        r,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..OrchestratorConfig::default()
        },
    );
    Fx {
        _dir: dir,
        db,
        goals: GoalRunner::new(orch.clone()),
        dispatcher: Dispatcher::with_ready_timeout(orch.clone(), Duration::from_secs(10)),
        orch,
        space,
        member,
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        harness_id: "script".into(),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        workspace_id: None,
        model: None,
        sandbox: SandboxMode::Direct,
        title: None,
    }
}

async fn wait_status(f: &Fx, id: &SessionId, want: SessionStatus) {
    for _ in 0..250 {
        if f.db.get_session(id).await.unwrap().status == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {id} never reached {want:?}");
}

async fn idle_session(f: &Fx) -> SessionId {
    let s = f
        .orch
        .create_session(f.space.clone(), config(), Some(f.member.clone()))
        .await
        .unwrap();
    wait_status(f, &s.id, SessionStatus::Idle).await;
    s.id
}

async fn wait_goal_end(f: &Fx, id: &GoalId) -> om_core::goal::Goal {
    for _ in 0..250 {
        let g = f.goals.get(id).await.unwrap();
        if !g.status.is_active() {
            return g;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("goal {id} never ended");
}

fn goal(objective: &str, max_turns: u32) -> NewGoal {
    NewGoal {
        objective: objective.into(),
        max_turns: Some(max_turns),
        deadline_at: None,
    }
}

/// The member on each `prompt_submitted` event of a session, in order.
async fn prompt_members(f: &Fx, id: &SessionId) -> Vec<Option<MemberId>> {
    f.db.list_events(id, 0, 10_000)
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e.payload {
            om_core::event::EventPayload::PromptSubmitted { member_id, .. } => Some(member_id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn goal_loops_until_the_marker() {
    let f = fixture().await;
    let sid = idle_session(&f).await;
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("ship it done-on-3", 5))
        .await
        .unwrap();
    assert_eq!(g.status, GoalStatus::Active);
    // One active goal per session.
    assert!(matches!(
        f.goals.set_goal(&sid, &f.member, goal("again", 5)).await,
        Err(CoreError::InvalidInput(_))
    ));
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!(g.status, GoalStatus::Achieved);
    assert_eq!(g.turns_used, 3);
    assert_eq!(g.ended_reason.as_deref(), Some("done_marker"));
    // Opening prompt plus two continuations, all as the goal's owner.
    assert_eq!(
        prompt_members(&f, &sid).await,
        vec![Some(f.member.clone()); 3]
    );
    assert_eq!(f.goals.list(&sid).await.unwrap()[0], g);
}

#[tokio::test]
async fn goal_stops_on_budget_refusal_owner_and_member_stop() {
    let f = fixture().await;

    let sid = idle_session(&f).await;
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("never", 2))
        .await
        .unwrap();
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!((g.status, g.turns_used), (GoalStatus::Exhausted, 2));

    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("refuse", 5))
        .await
        .unwrap();
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!(
        (g.status, g.ended_reason.as_deref()),
        (GoalStatus::Stopped, Some("agent_refused"))
    );

    // A member stop ends the loop; the running turn keeps going.
    wait_status(&f, &sid, SessionStatus::Idle).await;
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("hold", 5))
        .await
        .unwrap();
    wait_status(&f, &sid, SessionStatus::Running).await;
    let g = f.goals.stop_goal(&g.id, &f.member).await.unwrap();
    assert_eq!(
        (g.status, g.ended_reason.as_deref()),
        (GoalStatus::Stopped, Some("stopped_by_member"))
    );
    f.orch.cancel_turn(&sid).await.unwrap();
    wait_status(&f, &sid, SessionStatus::Idle).await;
    assert_eq!(f.goals.get(&g.id).await.unwrap().turns_used, 0);

    // The owner losing permission stops the loop before the next continuation.
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("soft-hold", 5))
        .await
        .unwrap();
    wait_status(&f, &sid, SessionStatus::Running).await;
    f.db.set_member_role(&f.member, Role::Viewer, None)
        .await
        .unwrap();
    f.orch.cancel_turn(&sid).await.unwrap();
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!(
        (g.status, g.turns_used, g.ended_reason.as_deref()),
        (GoalStatus::Stopped, 1, Some("owner_not_allowed"))
    );
    assert!(matches!(
        f.goals.set_goal(&sid, &f.member, goal("x", 5)).await,
        Err(CoreError::Forbidden(_))
    ));
}

#[tokio::test]
async fn goal_validation_and_restart_recovery() {
    let f = fixture().await;
    let sid = idle_session(&f).await;
    assert!(matches!(
        f.goals
            .set_goal(
                &sid,
                &f.member,
                NewGoal {
                    objective: "x".into(),
                    max_turns: None,
                    deadline_at: Some(now_ms() - 1),
                }
            )
            .await,
        Err(CoreError::InvalidInput(_))
    ));
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("hold", 5))
        .await
        .unwrap();
    assert_eq!(f.goals.recover().await.unwrap(), 1);
    let g = f.goals.get(&g.id).await.unwrap();
    assert_eq!(
        (g.status, g.ended_reason.as_deref()),
        (GoalStatus::Stopped, Some("server_restarted"))
    );
    f.orch.cancel_turn(&sid).await.unwrap();
}

#[tokio::test]
async fn member_dispatch_routes_with_their_permissions() {
    let f = fixture().await;
    let a = idle_session(&f).await;
    let b = idle_session(&f).await;

    let d = f
        .dispatcher
        .dispatch_as(
            &f.member,
            NewDispatch {
                from_session: a.clone(),
                target: DispatchTarget::Session {
                    session_id: b.clone(),
                    mode: SubmitMode::Queue,
                },
                prompt: Prompt::text("review this"),
            },
        )
        .await
        .unwrap();
    assert_eq!(d.depth, 1);
    assert!(!d.created_session);
    assert!(matches!(d.submit, SubmitOutcome::Started { .. }));
    assert_eq!(prompt_members(&f, &b).await, vec![Some(f.member.clone())]);

    // New session target.
    let d = f
        .dispatcher
        .dispatch_as(
            &f.member,
            NewDispatch {
                from_session: a.clone(),
                target: DispatchTarget::NewSession { config: config() },
                prompt: Prompt::text("start over"),
            },
        )
        .await
        .unwrap();
    assert!(d.created_session);
    assert_eq!(
        prompt_members(&f, &d.to_session).await,
        vec![Some(f.member.clone())]
    );
    assert_eq!(f.dispatcher.list(&a, 10).await.unwrap().len(), 2);

    // A viewer cannot dispatch; another space's session is not found.
    let viewer =
        f.db.create_member(&f.space, "vic", Role::Viewer, None)
            .await
            .unwrap()
            .id;
    let to_b = NewDispatch {
        from_session: a.clone(),
        target: DispatchTarget::Session {
            session_id: b.clone(),
            mode: SubmitMode::Queue,
        },
        prompt: Prompt::text("hi"),
    };
    assert!(matches!(
        f.dispatcher.dispatch_as(&viewer, to_b.clone()).await,
        Err(CoreError::Forbidden(_))
    ));
    let other = f.db.ensure_space("other").await.unwrap();
    let foreign = f.orch.create_session(other, config(), None).await.unwrap();
    let mut to_foreign = to_b.clone();
    to_foreign.target = DispatchTarget::Session {
        session_id: foreign.id,
        mode: SubmitMode::Queue,
    };
    assert!(matches!(
        f.dispatcher.dispatch_as(&f.member, to_foreign).await,
        Err(CoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn agent_dispatch_acts_as_the_turns_member_and_is_depth_limited() {
    let f = fixture().await;
    let s: Vec<SessionId> = {
        let mut v = Vec::new();
        for _ in 0..5 {
            v.push(idle_session(&f).await);
        }
        v
    };
    // Delivered prompts hold their turn open, so each target can dispatch in turn.
    let hop = |from: &SessionId, to: &SessionId| NewDispatch {
        from_session: from.clone(),
        target: DispatchTarget::Session {
            session_id: to.clone(),
            mode: SubmitMode::Queue,
        },
        prompt: Prompt::text("hold: pass it on"),
    };

    // No turn runs in s0: an agent there cannot dispatch.
    assert!(matches!(
        f.dispatcher.dispatch_from_agent(hop(&s[0], &s[1])).await,
        Err(CoreError::InvalidInput(_))
    ));
    // A turn nobody started does not count either.
    f.orch
        .send_prompt(&s[0], None, Prompt::text("hold system"), SubmitMode::Queue)
        .await
        .unwrap();
    wait_status(&f, &s[0], SessionStatus::Running).await;
    assert!(matches!(
        f.dispatcher.dispatch_from_agent(hop(&s[0], &s[1])).await,
        Err(CoreError::Forbidden(_))
    ));
    f.orch.cancel_turn(&s[0]).await.unwrap();
    wait_status(&f, &s[0], SessionStatus::Idle).await;

    f.orch
        .send_prompt(
            &s[0],
            Some(f.member.clone()),
            Prompt::text("hold go"),
            SubmitMode::Queue,
        )
        .await
        .unwrap();
    wait_status(&f, &s[0], SessionStatus::Running).await;
    for i in 0..MAX_DISPATCH_DEPTH as usize {
        let d = f
            .dispatcher
            .dispatch_from_agent(hop(&s[i], &s[i + 1]))
            .await
            .unwrap();
        assert_eq!(d.member_id, f.member);
        assert_eq!(d.depth, i as u32 + 1);
        wait_status(&f, &s[i + 1], SessionStatus::Running).await;
    }
    let last = MAX_DISPATCH_DEPTH as usize;
    assert!(matches!(
        f.dispatcher
            .dispatch_from_agent(hop(&s[last], &s[last + 1]))
            .await,
        Err(CoreError::LimitExceeded(_))
    ));
    // A member dispatching by hand starts a new chain.
    let d = f
        .dispatcher
        .dispatch_as(&f.member, hop(&s[last], &s[last + 1]))
        .await
        .unwrap();
    assert_eq!(d.depth, 1);

    // A member's own new turn in a deep session starts again at depth 0.
    f.orch.cancel_turn(&s[last]).await.unwrap();
    wait_status(&f, &s[last], SessionStatus::Idle).await;
    f.orch
        .send_prompt(
            &s[last],
            Some(f.member.clone()),
            Prompt::text("hold again"),
            SubmitMode::Queue,
        )
        .await
        .unwrap();
    wait_status(&f, &s[last], SessionStatus::Running).await;
    let fresh = idle_session(&f).await;
    let d = f
        .dispatcher
        .dispatch_from_agent(hop(&s[last], &fresh))
        .await
        .unwrap();
    assert_eq!(
        d.depth, 1,
        "a member's turn is not part of an earlier chain"
    );

    // Permission is the turn member's, checked now.
    f.db.set_member_role(&f.member, Role::Viewer, None)
        .await
        .unwrap();
    assert!(matches!(
        f.dispatcher.dispatch_from_agent(hop(&s[1], &s[0])).await,
        Err(CoreError::Forbidden(_))
    ));
}

#[tokio::test]
async fn one_turn_cannot_fan_out_without_limit() {
    let f = fixture().await;
    let src = idle_session(&f).await;
    f.orch
        .send_prompt(
            &src,
            Some(f.member.clone()),
            Prompt::text("hold fan out"),
            SubmitMode::Queue,
        )
        .await
        .unwrap();
    wait_status(&f, &src, SessionStatus::Running).await;
    let target = idle_session(&f).await;
    let hop = || NewDispatch {
        from_session: src.clone(),
        target: DispatchTarget::Session {
            session_id: target.clone(),
            mode: SubmitMode::Queue,
        },
        prompt: Prompt::text("work"),
    };
    for _ in 0..om_core::dispatch::MAX_DISPATCH_FANOUT {
        f.dispatcher.dispatch_from_agent(hop()).await.unwrap();
    }
    assert!(matches!(
        f.dispatcher.dispatch_from_agent(hop()).await,
        Err(CoreError::LimitExceeded(_))
    ));
}

/// Every `goal_updated` goal on a session, in stream order.
async fn goal_events(f: &Fx, id: &SessionId) -> Vec<om_core::goal::Goal> {
    f.db.list_events(id, 0, 10_000)
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e.payload {
            om_core::event::EventPayload::GoalUpdated { goal } => Some(goal),
            _ => None,
        })
        .collect()
}

async fn audit_actions(f: &Fx, target: &str) -> Vec<(Option<MemberId>, String)> {
    let mut v: Vec<_> =
        f.db.list_audit(&f.space, Some(target.to_owned()), 100)
            .await
            .unwrap()
            .into_iter()
            .map(|a| (a.actor, a.action))
            .collect();
    v.reverse();
    v
}

#[tokio::test]
async fn goals_and_dispatches_are_announced_and_audited() {
    let f = fixture().await;
    let sid = idle_session(&f).await;

    // set → progress after turn 1 → achieved after turn 2, all on the session stream.
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("ship it done-on-2", 5))
        .await
        .unwrap();
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!(g.status, GoalStatus::Achieved);
    let seen: Vec<_> = goal_events(&f, &sid)
        .await
        .into_iter()
        .map(|g| (g.status, g.turns_used))
        .collect();
    assert_eq!(
        seen,
        vec![
            (GoalStatus::Active, 0),
            (GoalStatus::Active, 1),
            (GoalStatus::Achieved, 2)
        ]
    );
    assert_eq!(
        audit_actions(&f, g.id.as_str()).await,
        vec![
            (Some(f.member.clone()), "goal.set".to_owned()),
            (None, "goal.ended".to_owned())
        ]
    );

    // Stopping needs the right to prompt in the goal's space.
    wait_status(&f, &sid, SessionStatus::Idle).await;
    let held = f
        .goals
        .set_goal(&sid, &f.member, goal("hold", 5))
        .await
        .unwrap();
    wait_status(&f, &sid, SessionStatus::Running).await;
    let viewer =
        f.db.create_member(&f.space, "vic", Role::Viewer, None)
            .await
            .unwrap()
            .id;
    let err = f.goals.stop_goal(&held.id, &viewer).await.unwrap_err();
    assert!(matches!(err, CoreError::Forbidden(_)), "{err:?}");
    let other_space = f.db.ensure_space("other").await.unwrap();
    let outsider =
        f.db.create_member(&other_space, "olga", Role::Owner, None)
            .await
            .unwrap()
            .id;
    let err = f.goals.stop_goal(&held.id, &outsider).await.unwrap_err();
    assert!(matches!(err, CoreError::NotFound(_)), "{err:?}");
    let stopped = f.goals.stop_goal(&held.id, &f.member).await.unwrap();
    assert_eq!(stopped.status, GoalStatus::Stopped);
    assert_eq!(
        audit_actions(&f, held.id.as_str()).await.last().unwrap(),
        &(Some(f.member.clone()), "goal.ended".to_owned())
    );
    assert_eq!(
        goal_events(&f, &sid).await.last().unwrap().status,
        GoalStatus::Stopped
    );
    f.orch.cancel_turn(&sid).await.unwrap();
    wait_status(&f, &sid, SessionStatus::Idle).await;

    // A dispatch shows up on both streams and in the audit log.
    let other = idle_session(&f).await;
    let d = f
        .dispatcher
        .dispatch_as(
            &f.member,
            NewDispatch {
                from_session: sid.clone(),
                target: DispatchTarget::Session {
                    session_id: other.clone(),
                    mode: SubmitMode::Queue,
                },
                prompt: Prompt::text("review this"),
            },
        )
        .await
        .unwrap();
    let kinds = |id: SessionId| {
        let db = f.db.clone();
        async move {
            db.list_events(&id, 0, 10_000)
                .await
                .unwrap()
                .events
                .into_iter()
                .filter_map(|e| match e.payload {
                    om_core::event::EventPayload::DispatchSent { dispatch } => {
                        Some(("sent", dispatch.id))
                    }
                    om_core::event::EventPayload::DispatchReceived { dispatch } => {
                        Some(("received", dispatch.id))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(kinds(sid.clone()).await, vec![("sent", d.id.clone())]);
    assert_eq!(kinds(other.clone()).await, vec![("received", d.id.clone())]);
    assert_eq!(
        audit_actions(&f, d.id.as_str()).await,
        vec![(Some(f.member.clone()), "session.dispatch".to_owned())]
    );

    // Only announcements may be appended from outside the session.
    let err = f
        .orch
        .announce(
            &sid,
            om_core::event::EventPayload::Error {
                message: "spoof".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidInput(_)), "{err:?}");

    // An ended session still records announcements (written straight to the log).
    f.orch.close_session(&other).await.unwrap();
    wait_status(&f, &other, SessionStatus::Closed).await;
    f.orch
        .announce(
            &other,
            om_core::event::EventPayload::GoalUpdated { goal: g.clone() },
        )
        .await
        .unwrap();
    assert_eq!(goal_events(&f, &other).await, vec![g]);
}

#[tokio::test]
async fn goals_follow_only_their_own_turns_and_expire_mid_turn() {
    let f = fixture().await;
    // Another member's prompt runs first; it must not count as the goal's turn.
    let sid = idle_session(&f).await;
    f.orch
        .send_prompt(
            &sid,
            Some(f.member.clone()),
            Prompt::text("hold busy"),
            SubmitMode::Queue,
        )
        .await
        .unwrap();
    wait_status(&f, &sid, SessionStatus::Running).await;
    let g = f
        .goals
        .set_goal(&sid, &f.member, goal("ship done-on-3", 5))
        .await
        .unwrap();
    // End the unrelated turn; the goal's queued opening prompt runs next.
    f.orch.cancel_turn(&sid).await.unwrap();
    f.orch.resume_queue(&sid).await.unwrap();
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!(g.status, GoalStatus::Achieved, "{g:?}");
    assert_eq!(
        g.turns_used, 2,
        "the unrelated, cancelled turn is not counted"
    );

    // A deadline ends the goal even while its turn is still running.
    let sid = idle_session(&f).await;
    let g = f
        .goals
        .set_goal(
            &sid,
            &f.member,
            NewGoal {
                objective: "hold forever".into(),
                max_turns: Some(5),
                deadline_at: Some(now_ms() + 300),
            },
        )
        .await
        .unwrap();
    let g = wait_goal_end(&f, &g.id).await;
    assert_eq!(
        (g.status, g.ended_reason.as_deref()),
        (GoalStatus::Expired, Some("deadline"))
    );
}
