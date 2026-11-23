//! End-to-end tests of `AcpHarness` against the scripted `om-fake-acp-agent`.

use futures::StreamExt;
use om_core::approval::ApprovalDecision;
use om_core::content::{ContentBlock, Prompt};
use om_core::event::ToolCallStatus;
use om_core::harness::{Harness, HarnessEvent, HarnessEventStream, StartContext, StartedSession};
use om_core::ids::SessionId;
use om_core::sandbox::{CommandSpec, DirectSandbox, SandboxMode};
use om_core::session::{SessionConfig, StopReason};
use om_core::{CoreError, CoreResult};
use om_server::acp::AcpHarness;
use std::sync::Arc;
use std::time::Duration;

fn harness_with_env(env: &[(&str, &str)]) -> AcpHarness {
    let mut cmd = CommandSpec::new(env!("CARGO_BIN_EXE_om-fake-acp-agent"));
    for (k, v) in env {
        cmd = cmd.env(*k, *v);
    }
    AcpHarness::new("fake", cmd)
}

fn ctx(resume: Option<&str>) -> StartContext {
    StartContext {
        session_id: SessionId::new(),
        config: SessionConfig {
            harness_id: "fake".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            workspace_id: None,
            model: None,
            sandbox: SandboxMode::Direct,
            title: None,
        },
        resume_native_session_id: resume.map(str::to_owned),
        sandbox: Arc::new(DirectSandbox),
    }
}

async fn start(resume: Option<&str>) -> CoreResult<StartedSession> {
    harness_with_env(&[]).start(ctx(resume)).await
}

async fn next(events: &mut HarnessEventStream) -> HarnessEvent {
    tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .expect("timed out waiting for harness event")
        .expect("event stream ended unexpectedly")
}

/// Collects events until (and including) the first `TurnEnded`.
async fn until_turn_end(events: &mut HarnessEventStream) -> Vec<HarnessEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next(events).await;
        let done = matches!(ev, HarnessEvent::TurnEnded { .. });
        out.push(ev);
        if done {
            return out;
        }
    }
}

fn text(ev: &HarnessEvent) -> Option<&str> {
    match ev {
        HarnessEvent::MessageChunk(ContentBlock::Text { text }) => Some(text),
        _ => None,
    }
}

#[tokio::test]
async fn echo_turn_streams_and_settles() {
    let StartedSession {
        session,
        mut events,
    } = start(None).await.unwrap();
    assert_eq!(
        next(&mut events).await,
        HarnessEvent::NativeSessionId("fake-session-1".into())
    );
    assert_eq!(
        session.native_session_id().as_deref(),
        Some("fake-session-1")
    );

    session.prompt(Prompt::text("hello")).await.unwrap();
    let evs = until_turn_end(&mut events).await;
    assert!(matches!(evs[0], HarnessEvent::ThoughtChunk(_)));
    assert!(matches!(&evs[1], HarnessEvent::Plan(p) if p.len() == 1));
    let reply: String = evs.iter().filter_map(text).collect();
    assert_eq!(reply, "echo: hello");
    assert_eq!(
        evs.last().unwrap(),
        &HarnessEvent::TurnEnded {
            stop_reason: StopReason::EndTurn,
            error: None
        }
    );

    // A second turn works on the same session.
    session.prompt(Prompt::text("again")).await.unwrap();
    let evs = until_turn_end(&mut events).await;
    assert_eq!(
        evs.iter().filter_map(text).collect::<String>(),
        "echo: again"
    );
    session.shutdown().await.unwrap();
}

async fn tool_turn(decision: ApprovalDecision) -> Vec<HarnessEvent> {
    let StartedSession {
        session,
        mut events,
    } = start(None).await.unwrap();
    next(&mut events).await; // native id
    session.prompt(Prompt::text("use a tool")).await.unwrap();
    assert!(
        matches!(next(&mut events).await, HarnessEvent::ToolCall(info) if info.title == "rm -rf build")
    );
    let HarnessEvent::PermissionRequested {
        request_id,
        tool_call_id,
        title,
        options,
    } = next(&mut events).await
    else {
        panic!("expected permission request");
    };
    assert_eq!(tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(
        title, "rm -rf build",
        "title is filled in from the earlier tool_call"
    );
    assert_eq!(options.len(), 2);
    session
        .respond_approval(&request_id, decision)
        .await
        .unwrap();
    let evs = until_turn_end(&mut events).await;
    session.shutdown().await.unwrap();
    evs
}

#[tokio::test]
async fn tool_call_allowed() {
    let evs = tool_turn(ApprovalDecision::Selected {
        option_id: "allow".into(),
    })
    .await;
    assert!(
        matches!(&evs[0], HarnessEvent::ToolCallUpdate(u) if u.status == Some(ToolCallStatus::Completed))
    );
    assert_eq!(evs.iter().filter_map(text).collect::<String>(), "ran it");
    assert!(matches!(
        evs.last().unwrap(),
        HarnessEvent::TurnEnded {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
}

#[tokio::test]
async fn tool_call_rejected() {
    let evs = tool_turn(ApprovalDecision::Selected {
        option_id: "reject".into(),
    })
    .await;
    assert!(
        matches!(&evs[0], HarnessEvent::ToolCallUpdate(u) if u.status == Some(ToolCallStatus::Failed))
    );
    assert_eq!(evs.iter().filter_map(text).collect::<String>(), "skipped");
}

#[tokio::test]
async fn cancel_while_waiting_for_approval() {
    let StartedSession {
        session,
        mut events,
    } = start(None).await.unwrap();
    next(&mut events).await;
    session.prompt(Prompt::text("tool")).await.unwrap();
    next(&mut events).await; // tool call
    let HarnessEvent::PermissionRequested { request_id, .. } = next(&mut events).await else {
        panic!("expected permission request");
    };
    session.cancel().await.unwrap();
    let evs = until_turn_end(&mut events).await;
    assert!(matches!(
        evs.last().unwrap(),
        HarnessEvent::TurnEnded {
            stop_reason: StopReason::Cancelled,
            ..
        }
    ));
    // The orchestrator's later Cancelled answer is idempotent; a late selection is not.
    session
        .respond_approval(&request_id, ApprovalDecision::Cancelled)
        .await
        .unwrap();
    let late = session
        .respond_approval(
            &request_id,
            ApprovalDecision::Selected {
                option_id: "allow".into(),
            },
        )
        .await;
    assert!(matches!(late, Err(CoreError::NotFound(_))));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_streaming_turn_and_reject_overlapping_prompt() {
    let StartedSession {
        session,
        mut events,
    } = start(None).await.unwrap();
    next(&mut events).await;
    session.prompt(Prompt::text("slow")).await.unwrap();
    assert!(text(&next(&mut events).await).unwrap().starts_with("tick"));
    let overlap = session.prompt(Prompt::text("second")).await;
    assert!(matches!(overlap, Err(CoreError::InvalidInput(_))));
    session.cancel().await.unwrap();
    let evs = until_turn_end(&mut events).await;
    assert!(matches!(
        evs.last().unwrap(),
        HarnessEvent::TurnEnded {
            stop_reason: StopReason::Cancelled,
            ..
        }
    ));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_mid_turn_reports_failure_then_exit() {
    let StartedSession {
        session,
        mut events,
    } = start(None).await.unwrap();
    next(&mut events).await;
    session.prompt(Prompt::text("crash")).await.unwrap();
    let evs = until_turn_end(&mut events).await;
    assert!(matches!(
        evs.last().unwrap(),
        HarnessEvent::TurnEnded {
            stop_reason: StopReason::Failed,
            error: Some(_)
        }
    ));
    match next(&mut events).await {
        HarnessEvent::Exited { error: Some(e) } => assert!(e.contains('3'), "{e}"),
        other => panic!("expected Exited, got {other:?}"),
    }
    let end = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .unwrap();
    assert!(end.is_none(), "stream ends after Exited");
}

#[tokio::test]
async fn shutdown_ends_stream() {
    let StartedSession {
        session,
        mut events,
    } = start(None).await.unwrap();
    next(&mut events).await;
    session.shutdown().await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        HarnessEvent::Exited { .. }
    ));
}

#[tokio::test]
async fn resume_loads_without_replaying_history() {
    let StartedSession {
        session,
        mut events,
    } = start(Some("old-native")).await.unwrap();
    assert_eq!(
        next(&mut events).await,
        HarnessEvent::NativeSessionId("old-native".into())
    );
    session.prompt(Prompt::text("hi")).await.unwrap();
    let evs = until_turn_end(&mut events).await;
    assert!(
        evs.iter().all(|e| text(e) != Some("old reply")),
        "replayed history must not leak into the live stream"
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn protocol_version_mismatch_fails_start() {
    let err = harness_with_env(&[("FAKE_ACP_VERSION", "2")])
        .start(ctx(None))
        .await
        .err()
        .expect("start must fail");
    assert!(matches!(err, CoreError::Harness(m) if m.contains("version")));
}

#[tokio::test]
async fn relative_cwd_and_missing_binary_are_rejected() {
    let mut c = ctx(None);
    c.config.cwd = "relative/dir".into();
    assert!(matches!(
        harness_with_env(&[]).start(c).await,
        Err(CoreError::InvalidInput(_))
    ));

    let missing = AcpHarness::new("missing", CommandSpec::new("/nonexistent/agent-binary"));
    assert!(matches!(
        missing.start(ctx(None)).await,
        Err(CoreError::Harness(_))
    ));
}
