//! W4-1 schedules: the scheduler driving a real orchestrator with an in-process harness.
//!
//! The harness echoes each prompt and ends the turn, except prompts containing `hold`,
//! which keep the turn open until cancelled.

use async_trait::async_trait;
use om_core::CoreError;
use om_core::approval::ApprovalDecision;
use om_core::auth::Role;
use om_core::content::{ContentBlock, Prompt};
use om_core::harness::{
    Harness, HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::ids::{MemberId, SessionId, SpaceId};
use om_core::now_ms;
use om_core::queue::{SubmitMode, SubmitOutcome};
use om_core::sandbox::{CommandSpec, SandboxMode};
use om_core::schedule::{
    MissedRunPolicy, NewSchedule, OverlapPolicy, RunOutcome, ScheduleSpec, ScheduleTarget,
    SkipReason,
};
use om_core::session::{SessionConfig, SessionStatus, StopReason};
use om_server::acp::AcpHarness;
use om_server::db::Db;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig};
use om_server::scheduler::Scheduler;
use om_server::stream::ReceiverStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<String>>>);

impl Received {
    fn get(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct HoldSession {
    tx: mpsc::UnboundedSender<HarnessEvent>,
    holding: Arc<Mutex<bool>>,
    received: Received,
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
impl HarnessSession for HoldSession {
    fn native_session_id(&self) -> Option<String> {
        None
    }

    async fn prompt(&self, prompt: Prompt) -> om_core::CoreResult<()> {
        let t = text(&prompt);
        self.received.0.lock().unwrap().push(t.clone());
        if t.contains("hold") {
            *self.holding.lock().unwrap() = true;
        } else {
            let _ = self.tx.send(HarnessEvent::TurnEnded {
                stop_reason: StopReason::EndTurn,
                error: None,
            });
        }
        Ok(())
    }

    async fn respond_approval(&self, _: &str, _: ApprovalDecision) -> om_core::CoreResult<()> {
        Ok(())
    }

    async fn cancel(&self) -> om_core::CoreResult<()> {
        let mut h = self.holding.lock().unwrap();
        if *h {
            *h = false;
            let _ = self.tx.send(HarnessEvent::TurnEnded {
                stop_reason: StopReason::Cancelled,
                error: None,
            });
        }
        Ok(())
    }

    async fn shutdown(&self) -> om_core::CoreResult<()> {
        Ok(())
    }
}

struct HoldHarness(Received);

#[async_trait]
impl Harness for HoldHarness {
    fn id(&self) -> &str {
        "hold"
    }

    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }

    async fn start(&self, _ctx: StartContext) -> om_core::CoreResult<StartedSession> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(StartedSession {
            session: Box::new(HoldSession {
                tx,
                holding: Arc::default(),
                received: self.0.clone(),
            }),
            events: Box::pin(ReceiverStream::new(rx)),
        })
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    db: Db,
    orch: Orchestrator,
    sched: Scheduler,
    space: SpaceId,
    member: MemberId,
    received: Received,
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
    let received = Received::default();
    let r = HarnessRegistry::new();
    r.register(Arc::new(HoldHarness(received.clone())));
    r.register(Arc::new(AcpHarness::new(
        "broken",
        CommandSpec::new("/nonexistent/agent"),
    )));
    let orch = Orchestrator::new(
        db.clone(),
        r,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..OrchestratorConfig::default()
        },
    );
    let sched = Scheduler::with_ready_timeout(orch.clone(), Duration::from_secs(10));
    Fx {
        _dir: dir,
        db,
        orch,
        sched,
        space,
        member,
        received,
    }
}

fn session_config(harness: &str) -> SessionConfig {
    SessionConfig {
        harness_id: harness.into(),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        workspace_id: None,
        model: None,
        sandbox: SandboxMode::Direct,
        title: None,
    }
}

fn every_minute(start_at: i64) -> ScheduleSpec {
    ScheduleSpec::Every {
        interval_secs: 60,
        start_at,
    }
}

fn new_schedule(name: &str, spec: ScheduleSpec, target: ScheduleTarget) -> NewSchedule {
    NewSchedule {
        name: name.into(),
        spec,
        target,
        enabled: true,
        missed: MissedRunPolicy::RunOnce,
        overlap: OverlapPolicy::Skip,
    }
}

fn new_session_target(harness: &str, prompt: &str) -> ScheduleTarget {
    ScheduleTarget::NewSession {
        config: session_config(harness),
        prompt: Prompt::text(prompt),
    }
}

async fn idle_session(f: &Fx) -> SessionId {
    let s = f
        .orch
        .create_session(
            f.space.clone(),
            session_config("hold"),
            Some(f.member.clone()),
        )
        .await
        .unwrap();
    wait_status(f, &s.id, SessionStatus::Idle).await;
    s.id
}

async fn wait_status(f: &Fx, id: &SessionId, want: SessionStatus) {
    for _ in 0..200 {
        if f.db.get_session(id).await.unwrap().status == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {id} never reached {want:?}");
}

async fn fire_all(f: &Fx, now: i64) -> Vec<om_core::schedule::ScheduleRun> {
    let mut out = Vec::new();
    for h in f.sched.tick(now).await.unwrap() {
        out.push(h.await.unwrap());
    }
    out
}

#[tokio::test]
async fn session_target_fires_once_per_slot_into_the_queue() {
    let f = fixture().await;
    let sid = idle_session(&f).await;
    let start = now_ms() + 1_000;
    let s = f
        .sched
        .create(
            f.space.clone(),
            &f.member,
            new_schedule(
                "ping",
                every_minute(start),
                ScheduleTarget::Session {
                    session_id: sid.clone(),
                    prompt: Prompt::text("ping"),
                    mode: SubmitMode::Queue,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(s.next_run_at, Some(start));
    assert_eq!(s.created_by, f.member);

    // Not due yet.
    assert!(fire_all(&f, start - 1).await.is_empty());

    let runs = fire_all(&f, start).await;
    assert_eq!(runs.len(), 1);
    assert!(matches!(
        &runs[0].outcome,
        RunOutcome::Delivered { session_id, submit: SubmitOutcome::Started { .. } } if *session_id == sid
    ));
    assert_eq!(runs[0].scheduled_for, start);
    // The same slot never fires twice.
    assert!(fire_all(&f, start + 10).await.is_empty());
    let s = f.sched.get(&s.id).await.unwrap();
    assert_eq!(s.next_run_at, Some(start + 60_000));
    assert_eq!(s.last_run_at, Some(start));

    // A slot that lands mid-turn queues behind it.
    wait_status(&f, &sid, SessionStatus::Idle).await;
    f.orch
        .send_prompt(&sid, None, Prompt::text("hold"), SubmitMode::Queue)
        .await
        .unwrap();
    let runs = fire_all(&f, start + 60_000).await;
    assert!(matches!(
        &runs[0].outcome,
        RunOutcome::Delivered {
            submit: SubmitOutcome::Queued { position: 0, .. },
            ..
        }
    ));
    assert!(runs[0].outcome.queued_id().is_some());
    assert_eq!(f.sched.runs(&s.id, 10).await.unwrap().len(), 2);
    assert_eq!(f.received.get(), ["ping", "hold"]);
    f.orch.cancel_turn(&sid).await.unwrap();
}

#[tokio::test]
async fn new_session_target_starts_a_session_and_respects_overlap() {
    let f = fixture().await;
    let start = now_ms() + 1_000;
    let s = f
        .sched
        .create(
            f.space.clone(),
            &f.member,
            new_schedule(
                "nightly",
                every_minute(start),
                new_session_target("hold", "hold review"),
            ),
        )
        .await
        .unwrap();

    let runs = fire_all(&f, start).await;
    let RunOutcome::SessionStarted {
        session_id: first,
        submit: SubmitOutcome::Started { .. },
    } = runs[0].outcome.clone()
    else {
        panic!("{:?}", runs[0].outcome)
    };
    let sess = f.db.get_session(&first).await.unwrap();
    assert_eq!(sess.config.title.as_deref(), Some("nightly"));
    assert_eq!(sess.status, SessionStatus::Running);

    // The previous run is still working: skipped.
    let runs = fire_all(&f, start + 60_000).await;
    assert_eq!(
        runs[0].outcome,
        RunOutcome::Skipped {
            reason: SkipReason::PreviousRunActive
        }
    );

    // With overlap allowed it starts another session.
    let mut edit = new_schedule(
        "nightly",
        every_minute(start),
        new_session_target("hold", "hold review"),
    );
    edit.overlap = OverlapPolicy::Allow;
    f.sched.update(&s.id, &f.member, edit).await.unwrap();
    let next = f.sched.get(&s.id).await.unwrap().next_run_at.unwrap();
    let runs = fire_all(&f, next).await;
    let RunOutcome::SessionStarted {
        session_id: second, ..
    } = runs[0].outcome.clone()
    else {
        panic!("{:?}", runs[0].outcome)
    };
    assert_ne!(first, second);
    assert_eq!(f.received.get(), ["hold review", "hold review"]);
}

#[tokio::test]
async fn owner_is_checked_at_create_and_at_every_fire() {
    let f = fixture().await;
    let viewer =
        f.db.create_member(&f.space, "vic", Role::Viewer, None)
            .await
            .unwrap()
            .id;
    let start = now_ms() + 1_000;
    let make =
        |name: &str| new_schedule(name, every_minute(start), new_session_target("hold", "hi"));
    assert!(matches!(
        f.sched.create(f.space.clone(), &viewer, make("v")).await,
        Err(CoreError::Forbidden(_))
    ));
    // Another space's session is not a valid target.
    let other_space = f.db.ensure_space("other").await.unwrap();
    let foreign = f
        .orch
        .create_session(other_space, session_config("hold"), None)
        .await
        .unwrap();
    let bad_target = new_schedule(
        "x",
        every_minute(start),
        ScheduleTarget::Session {
            session_id: foreign.id,
            prompt: Prompt::text("hi"),
            mode: SubmitMode::Queue,
        },
    );
    assert!(matches!(
        f.sched.create(f.space.clone(), &f.member, bad_target).await,
        Err(CoreError::NotFound(_))
    ));

    let s = f
        .sched
        .create(f.space.clone(), &f.member, make("job"))
        .await
        .unwrap();
    f.db.set_member_role(&f.member, Role::Viewer, None)
        .await
        .unwrap();
    let runs = fire_all(&f, start).await;
    assert_eq!(
        runs[0].outcome,
        RunOutcome::Skipped {
            reason: SkipReason::CreatorNotAllowed
        }
    );
    f.db.disable_member(&f.member, None).await.unwrap();
    let runs = fire_all(&f, start + 60_000).await;
    assert_eq!(
        runs[0].outcome,
        RunOutcome::Skipped {
            reason: SkipReason::CreatorDisabled
        }
    );
    assert!(f.received.get().is_empty());
    assert_eq!(f.sched.runs(&s.id, 50).await.unwrap().len(), 2);
}

#[tokio::test]
async fn missed_fires_collapse_or_skip() {
    let f = fixture().await;
    let sid = idle_session(&f).await;
    let start = now_ms() + 1_000;
    let target = ScheduleTarget::Session {
        session_id: sid.clone(),
        prompt: Prompt::text("tick"),
        mode: SubmitMode::Queue,
    };
    let once = f
        .sched
        .create(
            f.space.clone(),
            &f.member,
            new_schedule("a", every_minute(start), target.clone()),
        )
        .await
        .unwrap();
    let mut skip = new_schedule("b", every_minute(start), target);
    skip.missed = MissedRunPolicy::Skip;
    let skip = f
        .sched
        .create(f.space.clone(), &f.member, skip)
        .await
        .unwrap();

    // The server was down for ten slots.
    let later = start + 10 * 60_000 + 5;
    let runs = fire_all(&f, later).await;
    assert_eq!(runs.len(), 2);
    let by = |id| runs.iter().find(|r| r.schedule_id == id).unwrap();
    assert!(matches!(
        by(once.id.clone()).outcome,
        RunOutcome::Delivered { .. }
    ));
    assert_eq!(
        by(skip.id.clone()).outcome,
        RunOutcome::Skipped {
            reason: SkipReason::Missed
        }
    );
    for id in [&once.id, &skip.id] {
        assert_eq!(
            f.sched.get(id).await.unwrap().next_run_at,
            Some(start + 11 * 60_000)
        );
    }
    assert_eq!(f.received.get(), ["tick"]);
}

#[tokio::test]
async fn once_pause_trigger_delete_and_failures() {
    let f = fixture().await;
    let past = now_ms() - 1_000;
    assert!(matches!(
        f.sched
            .create(
                f.space.clone(),
                &f.member,
                new_schedule(
                    "old",
                    ScheduleSpec::Once { at: past },
                    new_session_target("hold", "x")
                )
            )
            .await,
        Err(CoreError::InvalidInput(_))
    ));

    let at = now_ms() + 1_000;
    let s = f
        .sched
        .create(
            f.space.clone(),
            &f.member,
            new_schedule(
                "once",
                ScheduleSpec::Once { at },
                new_session_target("broken", "x"),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        f.sched
            .create(
                f.space.clone(),
                &f.member,
                new_schedule(
                    "once",
                    ScheduleSpec::Once { at },
                    new_session_target("hold", "x")
                )
            )
            .await,
        Err(CoreError::InvalidInput(_))
    ));

    // Paused: nothing fires.
    let paused = f.sched.set_enabled(&s.id, false).await.unwrap();
    assert_eq!(paused.next_run_at, None);
    assert!(fire_all(&f, at).await.is_empty());
    let s = f.sched.set_enabled(&s.id, true).await.unwrap();
    assert_eq!(s.next_run_at, Some(at));

    // A harness that cannot start fails the run, naming the session it created.
    let runs = fire_all(&f, at).await;
    let RunOutcome::Failed { session_id, .. } = &runs[0].outcome else {
        panic!("{:?}", runs[0].outcome)
    };
    assert!(session_id.is_some());
    assert_eq!(f.sched.get(&s.id).await.unwrap().next_run_at, None);

    // Manual trigger does not need a slot.
    let mut edit = new_schedule(
        "once",
        ScheduleSpec::Once {
            at: now_ms() + 60_000,
        },
        new_session_target("hold", "manual"),
    );
    edit.enabled = true;
    let s = f.sched.update(&s.id, &f.member, edit).await.unwrap();
    let run = f.sched.trigger(&s.id, &f.member).await.unwrap();
    assert!(run.manual);
    assert!(matches!(run.outcome, RunOutcome::SessionStarted { .. }));
    assert_eq!(f.sched.get(&s.id).await.unwrap().next_run_at, s.next_run_at);
    assert_eq!(f.sched.runs(&s.id, 10).await.unwrap()[0], run);

    f.sched.delete(&s.id).await.unwrap();
    assert!(matches!(
        f.sched.get(&s.id).await,
        Err(CoreError::NotFound(_))
    ));
    assert!(f.sched.runs(&s.id, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn background_loop_fires_on_time() {
    let f = fixture().await;
    let sid = idle_session(&f).await;
    let handle = f.sched.spawn();
    let at = now_ms() + 300;
    let s = f
        .sched
        .create(
            f.space.clone(),
            &f.member,
            new_schedule(
                "soon",
                ScheduleSpec::Once { at },
                ScheduleTarget::Session {
                    session_id: sid,
                    prompt: Prompt::text("wake"),
                    mode: SubmitMode::Queue,
                },
            ),
        )
        .await
        .unwrap();
    let mut runs = Vec::new();
    for _ in 0..100 {
        runs = f.sched.runs(&s.id, 10).await.unwrap();
        if !runs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(runs.len(), 1);
    assert!(runs[0].fired_at >= at);
    assert_eq!(f.received.get(), ["wake"]);
    f.sched.shutdown();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}
