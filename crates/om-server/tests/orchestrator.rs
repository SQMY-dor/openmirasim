//! Orchestrator tests: real SQLite + real `AcpHarness` driving `om-fake-acp-agent`.

use om_core::CoreError;
use om_core::approval::{ApprovalDecision, ApprovalStatus};
use om_core::content::{ContentBlock, Prompt};
use om_core::event::{EventPayload, SessionEvent};
use om_core::ids::{MemberId, SessionId, SpaceId};
use om_core::sandbox::{CommandSpec, SandboxMode};
use om_core::session::{SessionConfig, SessionStatus, StopReason};
use om_server::acp::AcpHarness;
use om_server::db::Db;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig, Subscription};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    _dir: tempfile::TempDir,
    orch: Orchestrator,
    space: SpaceId,
}

fn registry() -> HarnessRegistry {
    let r = HarnessRegistry::new();
    r.register(Arc::new(AcpHarness::new(
        "fake",
        CommandSpec::new(env!("CARGO_BIN_EXE_om-fake-acp-agent")),
    )));
    r.register(Arc::new(AcpHarness::new(
        "broken",
        CommandSpec::new("/nonexistent/agent"),
    )));
    r
}

fn config(broadcast_capacity: usize) -> OrchestratorConfig {
    OrchestratorConfig {
        allow_direct_sandbox: true,
        broadcast_capacity,
        ..OrchestratorConfig::default()
    }
}

async fn fixture_with(capacity: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    Fixture {
        _dir: dir,
        orch: Orchestrator::new(db, registry(), config(capacity)),
        space,
    }
}

async fn fixture() -> Fixture {
    fixture_with(1024).await
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

async fn next(sub: &mut Subscription) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(10), sub.next())
        .await
        .expect("timed out waiting for session event")
        .unwrap()
        .expect("subscription closed")
}

/// Reads events until `pred` matches; returns everything read (inclusive).
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

fn is_status(to: SessionStatus) -> impl Fn(&EventPayload) -> bool {
    move |p| matches!(p, EventPayload::StatusChanged { to: t, .. } if *t == to)
}

fn assert_contiguous(events: &[SessionEvent], from: u64) {
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    let expected: Vec<u64> = (from..from + events.len() as u64).collect();
    assert_eq!(seqs, expected, "events must be gapless and ordered");
}

fn reply_text(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::AgentMessageChunk {
                content: ContentBlock::Text { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn ready_session(f: &Fixture) -> (SessionId, Subscription) {
    let s = f
        .orch
        .create_session(f.space.clone(), session_config("fake"), None)
        .await
        .unwrap();
    assert_eq!(s.status, SessionStatus::Starting);
    let mut sub = f.orch.subscribe(&s.id, 0).await.unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert_contiguous(&evs, 1);
    assert!(matches!(
        evs[0].payload,
        EventPayload::SessionCreated { .. }
    ));
    assert!(evs.iter().any(|e| matches!(
        &e.payload,
        EventPayload::HarnessReady { native_session_id: Some(n), .. } if n == "fake-session-1"
    )));
    (s.id, sub)
}

#[tokio::test]
async fn full_turn_is_persisted_and_streamed() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    let member = MemberId::from("alice");
    let turn = f
        .orch
        .submit_prompt(&id, Some(member.clone()), Prompt::text("hello"))
        .await
        .unwrap();

    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    match &evs[0].payload {
        EventPayload::PromptSubmitted {
            turn_id, member_id, ..
        } => {
            assert_eq!(turn_id, &turn);
            assert_eq!(member_id.as_ref(), Some(&member));
        }
        other => panic!("expected PromptSubmitted, got {other:?}"),
    }
    assert!(is_status(SessionStatus::Running)(&evs[1].payload));
    assert!(
        evs.iter()
            .any(|e| matches!(e.payload, EventPayload::AgentThoughtChunk { .. }))
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e.payload, EventPayload::Plan { .. }))
    );
    assert_eq!(reply_text(&evs), "echo: hello");
    assert!(evs.iter().any(|e| matches!(
        &e.payload,
        EventPayload::TurnEnded { turn_id, stop_reason: StopReason::EndTurn, .. } if turn_id == &turn
    )));

    // The stored history equals what was streamed.
    let summary = f.orch.db().get_session(&id).await.unwrap();
    assert_eq!(summary.status, SessionStatus::Idle);
    assert_eq!(summary.head_seq, evs.last().unwrap().seq);
    let stored = f.orch.db().list_events(&id, 0, 1000).await.unwrap().events;
    assert_contiguous(&stored, 1);
    assert_eq!(&stored[stored.len() - evs.len()..], &evs[..]);
}

#[tokio::test]
async fn late_subscriber_resumes_from_last_seq() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    f.orch
        .submit_prompt(&id, None, Prompt::text("one"))
        .await
        .unwrap();
    let first = until(&mut sub, is_status(SessionStatus::Idle)).await;
    let cut = first[2].seq;

    // A client that saw up to `cut` reconnects while a second turn runs.
    f.orch
        .submit_prompt(&id, None, Prompt::text("two"))
        .await
        .unwrap();
    let mut resumed = f.orch.subscribe(&id, cut).await.unwrap();
    assert!(resumed.head_seq() >= cut);
    // Read through the end of turn one, then through the end of turn two.
    let mut all = until(&mut resumed, is_status(SessionStatus::Idle)).await;
    all.extend(until(&mut resumed, is_status(SessionStatus::Idle)).await);
    assert_eq!(all[0].seq, cut + 1);
    assert_contiguous(&all, cut + 1);
    assert!(reply_text(&all).ends_with("echo: two"));
}

#[tokio::test]
async fn lagging_subscriber_replays_from_storage_without_gaps() {
    let f = fixture_with(2).await;
    let (id, _sub) = ready_session(&f).await;
    let mut slow = f.orch.subscribe(&id, 0).await.unwrap();
    for text in ["a", "b", "c"] {
        f.orch
            .submit_prompt(&id, None, Prompt::text(text))
            .await
            .unwrap();
        // Wait for the turn to finish without reading `slow`.
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if f.orch.db().get_session(&id).await.unwrap().status == SessionStatus::Idle {
                break;
            }
        }
    }
    let head = f.orch.db().get_session(&id).await.unwrap().head_seq;
    let mut all = Vec::new();
    while all.last().map(|e: &SessionEvent| e.seq) != Some(head) {
        all.push(next(&mut slow).await);
    }
    assert_contiguous(&all, 1);
    assert_eq!(reply_text(&all), "echo: aecho: becho: c");
}

#[tokio::test]
async fn approval_flow_and_double_resolution() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    f.orch
        .submit_prompt(&id, None, Prompt::text("tool"))
        .await
        .unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::WaitingApproval)).await;
    let approval = evs
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ApprovalRequested { approval, turn_id } => {
                assert!(turn_id.is_some());
                Some(approval.clone())
            }
            _ => None,
        })
        .expect("approval requested");
    assert_eq!(approval.title, "rm -rf build");
    assert_eq!(
        f.orch.db().get_session(&id).await.unwrap().status,
        SessionStatus::WaitingApproval
    );

    // Two members race; exactly one wins.
    let allow = ApprovalDecision::Selected {
        option_id: "allow".into(),
    };
    let (a, b) = tokio::join!(
        f.orch
            .resolve_approval(&approval.id, allow.clone(), Some(MemberId::from("alice"))),
        f.orch
            .resolve_approval(&approval.id, allow.clone(), Some(MemberId::from("bob"))),
    );
    let (winner, loser) = if a.is_ok() { (a, b) } else { (b, a) };
    let record = winner.unwrap();
    assert_eq!(record.status, ApprovalStatus::Resolved);
    assert!(matches!(loser, Err(CoreError::ApprovalAlreadyResolved(_))));

    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert!(matches!(
        evs[0].payload,
        EventPayload::ApprovalResolved { .. }
    ));
    assert!(is_status(SessionStatus::Running)(&evs[1].payload));
    assert_eq!(reply_text(&evs), "ran it");
}

#[tokio::test]
async fn cancel_during_approval_cancels_it_and_settles() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    f.orch
        .submit_prompt(&id, None, Prompt::text("tool"))
        .await
        .unwrap();
    until(&mut sub, is_status(SessionStatus::WaitingApproval)).await;
    f.orch.cancel_turn(&id).await.unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert!(evs.iter().any(|e| matches!(
        e.payload,
        EventPayload::ApprovalResolved {
            decision: ApprovalDecision::Cancelled,
            ..
        }
    )));
    assert!(evs.iter().any(|e| matches!(
        e.payload,
        EventPayload::TurnEnded {
            stop_reason: StopReason::Cancelled,
            ..
        }
    )));
    assert!(
        f.orch
            .db()
            .list_approvals(&id, true)
            .await
            .unwrap()
            .is_empty()
    );
    // The session is usable again.
    f.orch
        .submit_prompt(&id, None, Prompt::text("after"))
        .await
        .unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert_eq!(reply_text(&evs), "echo: after");
}

#[tokio::test]
async fn busy_session_rejects_second_prompt_and_empty_prompts() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    assert!(matches!(
        f.orch.submit_prompt(&id, None, Prompt::text("  ")).await,
        Err(CoreError::InvalidInput(_))
    ));
    f.orch
        .submit_prompt(&id, None, Prompt::text("slow"))
        .await
        .unwrap();
    let err = f
        .orch
        .submit_prompt(&id, None, Prompt::text("again"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CoreError::InvalidTransition {
            from: SessionStatus::Running,
            ..
        }
    ));
    f.orch.cancel_turn(&id).await.unwrap();
    until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert!(matches!(
        f.orch.cancel_turn(&id).await,
        Err(CoreError::InvalidTransition { .. })
    ));
}

#[tokio::test]
async fn crash_fails_session_and_resume_recovers() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    f.orch
        .submit_prompt(&id, None, Prompt::text("crash"))
        .await
        .unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Failed)).await;
    assert!(evs.iter().any(|e| matches!(
        e.payload,
        EventPayload::TurnEnded {
            stop_reason: StopReason::Failed,
            ..
        }
    )));
    assert!(
        evs.iter()
            .any(|e| matches!(e.payload, EventPayload::Error { .. }))
    );
    // Wait for the actor to exit.
    for _ in 0..100 {
        if !f.orch.is_live(&id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!f.orch.is_live(&id));
    assert!(matches!(
        f.orch.submit_prompt(&id, None, Prompt::text("x")).await,
        Err(CoreError::InvalidTransition {
            from: SessionStatus::Failed,
            ..
        })
    ));

    f.orch.resume_session(&id).await.unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert!(is_status(SessionStatus::Starting)(&evs[0].payload));
    f.orch
        .submit_prompt(&id, None, Prompt::text("back"))
        .await
        .unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Idle)).await;
    assert_eq!(reply_text(&evs), "echo: back");
}

#[tokio::test]
async fn close_is_terminal() {
    let f = fixture().await;
    let (id, mut sub) = ready_session(&f).await;
    f.orch
        .submit_prompt(&id, None, Prompt::text("slow"))
        .await
        .unwrap();
    until(&mut sub, is_status(SessionStatus::Running)).await;
    f.orch.close_session(&id).await.unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Closed)).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e.payload, EventPayload::TurnEnded { .. }))
    );
    assert_eq!(
        f.orch.db().get_session(&id).await.unwrap().status,
        SessionStatus::Closed
    );
    assert!(
        f.orch
            .submit_prompt(&id, None, Prompt::text("x"))
            .await
            .is_err()
    );
    assert!(f.orch.resume_session(&id).await.is_err());
    assert!(f.orch.close_session(&id).await.is_err());
}

#[tokio::test]
async fn harness_start_failure_marks_session_failed() {
    let f = fixture().await;
    let s = f
        .orch
        .create_session(f.space.clone(), session_config("broken"), None)
        .await
        .unwrap();
    let mut sub = f.orch.subscribe(&s.id, 0).await.unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Failed)).await;
    assert!(evs.iter().any(
        |e| matches!(&e.payload, EventPayload::Error { message } if message.contains("broken"))
    ));
}

#[tokio::test]
async fn validation_of_harness_and_sandbox() {
    let f = fixture().await;
    assert!(matches!(
        f.orch
            .create_session(f.space.clone(), session_config("nope"), None)
            .await,
        Err(CoreError::NotFound(_))
    ));
    let mut bwrap = session_config("fake");
    bwrap.sandbox = SandboxMode::Bwrap;
    assert!(matches!(
        f.orch.create_session(f.space.clone(), bwrap, None).await,
        Err(CoreError::Unsupported { .. })
    ));

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("strict.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let strict = Orchestrator::new(db, registry(), OrchestratorConfig::default());
    assert!(matches!(
        strict.create_session(space, session_config("fake"), None).await,
        Err(CoreError::InvalidInput(m)) if m.contains("direct")
    ));
    assert!(matches!(
        f.orch.subscribe(&SessionId::from("missing"), 0).await,
        Err(CoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn restart_recovery_keeps_full_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("om.db");
    let (id, head) = {
        let db = Db::open(&path).unwrap();
        let space = db.ensure_space("default").await.unwrap();
        let f = Fixture {
            _dir: tempfile::tempdir().unwrap(),
            orch: Orchestrator::new(db, registry(), config(64)),
            space,
        };
        let (id, mut sub) = ready_session(&f).await;
        f.orch
            .submit_prompt(&id, None, Prompt::text("remember"))
            .await
            .unwrap();
        until(&mut sub, is_status(SessionStatus::Idle)).await;
        let head = f.orch.db().get_session(&id).await.unwrap().head_seq;
        (id, head)
        // Orchestrator dropped here: actors shut their agents down.
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    let db = Db::open(Path::new(&path)).unwrap();
    let orch = Orchestrator::new(db, registry(), config(64));
    assert_eq!(orch.recover().await.unwrap(), vec![id.clone()]);
    let mut sub = orch.subscribe(&id, 0).await.unwrap();
    let evs = until(&mut sub, is_status(SessionStatus::Failed)).await;
    assert_contiguous(&evs, 1);
    assert!(evs.len() as u64 > head);
    assert_eq!(reply_text(&evs), "echo: remember");

    orch.resume_session(&id).await.unwrap();
    until(&mut sub, is_status(SessionStatus::Idle)).await;
}
