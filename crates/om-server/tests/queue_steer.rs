//! W2-3 Queue & Steer: orchestrator tests with an in-process scripted harness.
//!
//! The harness echoes each prompt as a message chunk. A prompt containing `hold`
//! keeps its turn open until it is cancelled, steered (native mode) or released.

use async_trait::async_trait;
use om_core::CoreError;
use om_core::approval::ApprovalDecision;
use om_core::content::{ContentBlock, Prompt};
use om_core::event::{EventPayload, SessionEvent};
use om_core::harness::{
    Harness, HarnessCapabilities, HarnessEvent, HarnessSession, StartContext, StartedSession,
};
use om_core::ids::{MemberId, QueuedPromptId, SessionId, SpaceId};
use om_core::queue::{MAX_QUEUED_PROMPTS, QueueRemoval, SubmitMode, SubmitOutcome};
use om_core::sandbox::SandboxMode;
use om_core::session::{SessionConfig, SessionStatus, StopReason};
use om_server::db::Db;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig, Subscription};
use om_server::stream::ReceiverStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

struct ScriptSession {
    tx: mpsc::UnboundedSender<HarnessEvent>,
    holding: Arc<Mutex<bool>>,
    /// Every prompt the agent received, in order (steers prefixed with `steer:`).
    received: Arc<Mutex<Vec<String>>>,
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

impl ScriptSession {
    fn end(&self, reason: StopReason) {
        *self.holding.lock().unwrap() = false;
        let _ = self.tx.send(HarnessEvent::TurnEnded {
            stop_reason: reason,
            error: None,
        });
    }
}

#[async_trait]
impl HarnessSession for ScriptSession {
    fn native_session_id(&self) -> Option<String> {
        None
    }

    async fn prompt(&self, prompt: Prompt) -> om_core::CoreResult<()> {
        let t = text(&prompt);
        if t.contains("refuse") {
            return Err(CoreError::Harness("refused by agent".into()));
        }
        self.received.lock().unwrap().push(t.clone());
        let _ = self.tx.send(HarnessEvent::MessageChunk(ContentBlock::Text {
            text: format!("echo:{t}"),
        }));
        if t.contains("hold") {
            *self.holding.lock().unwrap() = true;
        } else {
            self.end(StopReason::EndTurn);
        }
        Ok(())
    }

    async fn steer(&self, prompt: Prompt) -> om_core::CoreResult<()> {
        if !*self.holding.lock().unwrap() {
            return Err(CoreError::InvalidInput("no turn".into()));
        }
        let t = text(&prompt);
        self.received.lock().unwrap().push(format!("steer:{t}"));
        let _ = self.tx.send(HarnessEvent::MessageChunk(ContentBlock::Text {
            text: format!("steered:{t}"),
        }));
        self.end(StopReason::EndTurn);
        Ok(())
    }

    async fn respond_approval(&self, _: &str, _: ApprovalDecision) -> om_core::CoreResult<()> {
        Ok(())
    }

    async fn cancel(&self) -> om_core::CoreResult<()> {
        if *self.holding.lock().unwrap() {
            self.end(StopReason::Cancelled);
        }
        Ok(())
    }

    async fn shutdown(&self) -> om_core::CoreResult<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<String>>>);

struct ScriptHarness {
    native_steer: bool,
    received: Received,
}

#[async_trait]
impl Harness for ScriptHarness {
    fn id(&self) -> &str {
        if self.native_steer { "steer" } else { "plain" }
    }

    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            steer: self.native_steer,
            ..HarnessCapabilities::default()
        }
    }

    async fn start(&self, _ctx: StartContext) -> om_core::CoreResult<StartedSession> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(StartedSession {
            session: Box::new(ScriptSession {
                tx,
                holding: Arc::default(),
                received: self.received.0.clone(),
            }),
            events: Box::pin(ReceiverStream::new(rx)),
        })
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    orch: Orchestrator,
    space: SpaceId,
    received: Received,
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let received = Received::default();
    let r = HarnessRegistry::new();
    r.register(Arc::new(ScriptHarness {
        native_steer: false,
        received: received.clone(),
    }));
    r.register(Arc::new(ScriptHarness {
        native_steer: true,
        received: received.clone(),
    }));
    let orch = Orchestrator::new(
        db,
        r,
        OrchestratorConfig {
            allow_direct_sandbox: true,
            ..OrchestratorConfig::default()
        },
    );
    Fx {
        _dir: dir,
        orch,
        space,
        received,
    }
}

async fn next(sub: &mut Subscription) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(10), sub.next())
        .await
        .expect("timed out waiting for session event")
        .unwrap()
        .expect("subscription closed")
}

async fn until(sub: &mut Subscription, pred: impl Fn(&EventPayload) -> bool) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next(sub).await;
        let done = pred(&ev.payload);
        out.push(ev);
        if done {
            return out;
        }
    }
}

fn idle(p: &EventPayload) -> bool {
    matches!(
        p,
        EventPayload::StatusChanged {
            to: SessionStatus::Idle,
            ..
        }
    )
}

async fn ready(f: &Fx, harness: &str) -> (SessionId, Subscription) {
    let s = f
        .orch
        .create_session(
            f.space.clone(),
            SessionConfig {
                harness_id: harness.into(),
                cwd: std::env::temp_dir().to_string_lossy().into_owned(),
                workspace_id: None,
                model: None,
                sandbox: SandboxMode::Direct,
                title: None,
            },
            None,
        )
        .await
        .unwrap();
    let mut sub = f.orch.subscribe(&s.id, 0).await.unwrap();
    until(&mut sub, idle).await;
    (s.id, sub)
}

fn p(t: &str) -> Prompt {
    Prompt::text(t)
}

/// Turn ends seen so far, as `(prompt text, stop reason)`.
fn turns(events: &[SessionEvent]) -> Vec<StopReason> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::TurnEnded { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn idle_send_starts_and_busy_sends_queue_in_order() {
    let f = fixture().await;
    let (id, mut sub) = ready(&f, "plain").await;
    let m = Some(MemberId::from("m1"));

    let first = f
        .orch
        .send_prompt(&id, m.clone(), p("hold one"), SubmitMode::Queue)
        .await
        .unwrap();
    assert!(matches!(first, SubmitOutcome::Started { .. }));

    let a = f
        .orch
        .send_prompt(&id, m.clone(), p("a"), SubmitMode::Queue)
        .await
        .unwrap();
    let b = f
        .orch
        .send_prompt(&id, m.clone(), p("b"), SubmitMode::Queue)
        .await
        .unwrap();
    let c = f
        .orch
        .send_prompt(&id, m.clone(), p("c"), SubmitMode::Next)
        .await
        .unwrap();
    assert!(matches!(a, SubmitOutcome::Queued { position: 0, .. }));
    assert!(matches!(b, SubmitOutcome::Queued { position: 1, .. }));
    assert!(matches!(c, SubmitOutcome::Queued { position: 0, .. }));
    let q = f.orch.queue(&id).await.unwrap();
    let order: Vec<String> = q.iter().map(|q| text(&q.prompt)).collect();
    assert_eq!(order, ["c", "a", "b"]);
    assert_eq!(q[0].member_id, m);

    // The legacy single-turn API refuses while prompts are queued.
    assert!(f.orch.submit_prompt(&id, None, p("x")).await.is_err());

    // Steer on a harness without native steer: cancel + run first.
    let s = f
        .orch
        .send_prompt(&id, m.clone(), p("s"), SubmitMode::Steer)
        .await
        .unwrap();
    let SubmitOutcome::Interrupting { .. } = s else {
        panic!("expected degraded steer, got {s:?}")
    };

    // Everything drains: hold one (cancelled), s, c, a, b.
    let mut events = Vec::new();
    while turns(&events).len() < 5 {
        events.extend(until(&mut sub, idle).await);
    }
    assert_eq!(
        *f.received.0.lock().unwrap(),
        ["hold one", "s", "c", "a", "b"]
    );
    let ends = turns(&events);
    assert_eq!(ends[0], StopReason::Cancelled);
    assert!(ends[1..].iter().all(|r| *r == StopReason::EndTurn));

    // Each started queue item is announced before its PromptSubmitted.
    let started: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e.payload,
                EventPayload::QueueItemRemoved {
                    reason: QueueRemoval::Started { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(started.len(), 4);
    for ev in started {
        let following = events.iter().find(|e| e.seq == ev.seq + 1).unwrap();
        assert!(matches!(
            following.payload,
            EventPayload::PromptSubmitted { .. }
        ));
    }
}

#[tokio::test]
async fn native_steer_injects_into_the_running_turn() {
    let f = fixture().await;
    let (id, mut sub) = ready(&f, "steer").await;
    let SubmitOutcome::Started { turn_id } = f
        .orch
        .send_prompt(&id, None, p("hold"), SubmitMode::Queue)
        .await
        .unwrap()
    else {
        panic!()
    };
    let out = f
        .orch
        .send_prompt(
            &id,
            Some(MemberId::from("m2")),
            p("go left"),
            SubmitMode::Steer,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        SubmitOutcome::Steered {
            turn_id: turn_id.clone()
        }
    );
    let events = until(&mut sub, idle).await;
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::PromptSteered { turn_id: t, member_id: Some(m), .. }
            if *t == turn_id && m.as_str() == "m2"
    )));
    assert_eq!(turns(&events), [StopReason::EndTurn]);
    assert_eq!(*f.received.0.lock().unwrap(), ["hold", "steer:go left"]);
    assert!(f.orch.queue(&id).await.unwrap().is_empty());
}

#[tokio::test]
async fn cancel_pauses_the_queue_until_resumed_or_sent_again() {
    let f = fixture().await;
    let (id, mut sub) = ready(&f, "plain").await;
    f.orch
        .send_prompt(&id, None, p("hold 1"), SubmitMode::Queue)
        .await
        .unwrap();
    f.orch
        .send_prompt(&id, None, p("hold 2"), SubmitMode::Queue)
        .await
        .unwrap();
    f.orch
        .send_prompt(&id, None, p("three"), SubmitMode::Queue)
        .await
        .unwrap();

    f.orch.cancel_turn(&id).await.unwrap();
    let events = until(&mut sub, idle).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, EventPayload::QueuePaused { paused: true }))
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.orch.queue(&id).await.unwrap().len(), 2, "nothing started");
    assert_eq!(*f.received.0.lock().unwrap(), ["hold 1"]);

    // Resume starts the next one.
    f.orch.resume_queue(&id).await.unwrap();
    until(&mut sub, |p| {
        matches!(p, EventPayload::PromptSubmitted { .. })
    })
    .await;
    assert_eq!(*f.received.0.lock().unwrap(), ["hold 1", "hold 2"]);

    // Cancel again, then a plain send resumes and still waits behind `three`.
    f.orch.cancel_turn(&id).await.unwrap();
    until(&mut sub, idle).await;
    let out = f
        .orch
        .send_prompt(&id, None, p("four"), SubmitMode::Queue)
        .await
        .unwrap();
    assert!(
        matches!(out, SubmitOutcome::Queued { position: 0, .. }),
        "{out:?}"
    );
    until(&mut sub, |p| {
        matches!(
            p,
            EventPayload::QueueItemRemoved {
                reason: QueueRemoval::Started { .. },
                ..
            }
        )
    })
    .await;
    // `three` started; `four` runs after it.
    while f.received.0.lock().unwrap().len() < 4 {
        until(&mut sub, idle).await;
    }
    assert_eq!(
        *f.received.0.lock().unwrap(),
        ["hold 1", "hold 2", "three", "four"]
    );
}

#[tokio::test]
async fn withdraw_move_limits_failures_and_close() {
    let f = fixture().await;
    let (id, mut sub) = ready(&f, "plain").await;
    f.orch
        .send_prompt(&id, None, p("hold"), SubmitMode::Queue)
        .await
        .unwrap();
    let mut ids: Vec<QueuedPromptId> = Vec::new();
    for t in ["a", "refuse", "b"] {
        let SubmitOutcome::Queued { queued_id, .. } = f
            .orch
            .send_prompt(&id, None, p(t), SubmitMode::Queue)
            .await
            .unwrap()
        else {
            panic!()
        };
        ids.push(queued_id);
    }

    // Move `b` to the front, withdraw `a`.
    let q = f.orch.move_queued(&id, ids[2].clone(), 0).await.unwrap();
    assert_eq!(
        q.iter().map(|q| text(&q.prompt)).collect::<Vec<_>>(),
        ["b", "a", "refuse"]
    );
    f.orch
        .withdraw_queued(&id, ids[0].clone(), Some(MemberId::from("m1")))
        .await
        .unwrap();
    assert!(matches!(
        f.orch.withdraw_queued(&id, ids[0].clone(), None).await,
        Err(CoreError::NotFound(_))
    ));
    assert!(matches!(
        f.orch
            .move_queued(&id, QueuedPromptId::from("nope"), 0)
            .await,
        Err(CoreError::NotFound(_))
    ));
    assert!(matches!(
        f.orch
            .send_prompt(&id, None, p(""), SubmitMode::Queue)
            .await,
        Err(CoreError::InvalidInput(_))
    ));

    // Fill to the cap.
    let room = MAX_QUEUED_PROMPTS - f.orch.queue(&id).await.unwrap().len();
    for i in 0..room {
        f.orch
            .send_prompt(&id, None, p(&format!("hold fill {i}")), SubmitMode::Queue)
            .await
            .unwrap();
    }
    assert!(matches!(
        f.orch
            .send_prompt(&id, None, p("over"), SubmitMode::Queue)
            .await,
        Err(CoreError::LimitExceeded(_))
    ));
    // Trim back to `b`, `refuse`.
    for q in f.orch.queue(&id).await.unwrap().into_iter().skip(2) {
        f.orch.withdraw_queued(&id, q.id, None).await.unwrap();
    }
    // Add one held item so the queue is non-empty at close time.
    f.orch
        .send_prompt(&id, None, p("hold last"), SubmitMode::Queue)
        .await
        .unwrap();

    // Release the running turn by steering (degraded) with a prompt that ends at once.
    f.orch
        .send_prompt(&id, None, p("go"), SubmitMode::Steer)
        .await
        .unwrap();
    // go, b run; refuse fails; `hold last` runs and holds.
    let events = until(&mut sub, |p| {
        matches!(
            p,
            EventPayload::QueueItemRemoved {
                reason: QueueRemoval::Failed { .. },
                ..
            }
        )
    })
    .await;
    until(&mut sub, |p| matches!(p, EventPayload::PromptSubmitted { prompt, .. } if text(prompt) == "hold last")).await;
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::QueueItemRemoved { queued_id, reason: QueueRemoval::Failed { .. } } if *queued_id == ids[1]
    )));
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::QueueItemRemoved { queued_id, reason: QueueRemoval::Withdrawn { by: Some(_) } } if *queued_id == ids[0]
    )));
    let got = f.received.0.lock().unwrap().clone();
    assert_eq!(&got[..3], ["hold", "go", "b"]);

    // Queue something behind the held turn, then close: it is dropped.
    let SubmitOutcome::Queued { queued_id, .. } = f
        .orch
        .send_prompt(&id, None, p("late"), SubmitMode::Queue)
        .await
        .unwrap()
    else {
        panic!()
    };
    f.orch.close_session(&id).await.unwrap();
    let tail = until(&mut sub, |p| {
        matches!(
            p,
            EventPayload::StatusChanged {
                to: SessionStatus::Closed,
                ..
            }
        )
    })
    .await;
    assert!(tail.iter().any(|e| matches!(
        &e.payload,
        EventPayload::QueueItemRemoved { queued_id: q, reason: QueueRemoval::Dropped } if *q == queued_id
    )));
}
