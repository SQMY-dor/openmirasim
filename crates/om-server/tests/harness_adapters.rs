//! Integration tests for W0-6 (`HarnessRegistry`, `MockHarness`, and `opencode` launcher/probe).

use futures::StreamExt;
use om_core::CoreError;
use om_core::approval::ApprovalDecision;
use om_core::content::{ContentBlock, Prompt};
use om_core::harness::{Harness, HarnessEvent, HarnessEventStream, StartContext};
use om_core::ids::SessionId;
use om_core::sandbox::{DirectSandbox, SandboxMode};
use om_core::session::{SessionConfig, StopReason};
use om_server::harness::{
    MockHarness, OpenCodeConfig, default_phase0_registry, describe_harnesses, opencode_harness,
    probe_opencode,
};
use std::sync::Arc;
use std::time::Duration;

fn start_ctx(harness_id: &str, resume_id: Option<&str>) -> StartContext {
    StartContext {
        session_id: SessionId::new(),
        config: SessionConfig {
            harness_id: harness_id.into(),
            cwd: "/tmp".into(),
            workspace_id: None,
            model: None,
            sandbox: SandboxMode::Direct,
            title: Some("test".into()),
        },
        resume_native_session_id: resume_id.map(ToOwned::to_owned),
        sandbox: Arc::new(DirectSandbox),
    }
}

async fn next_ev(stream: &mut HarnessEventStream) -> HarnessEvent {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timed out waiting for HarnessEvent")
        .expect("event stream ended unexpectedly")
}

#[tokio::test]
async fn registry_registers_phase0_defaults() {
    let reg = default_phase0_registry();
    let list = describe_harnesses(&reg);
    let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, vec!["mock", "opencode"]);
    assert!(reg.get("mock").is_some());
    assert!(reg.get("opencode").is_some());
    assert!(reg.get("unknown").is_none());
}

#[tokio::test]
async fn mock_harness_streams_plan_tool_and_reply_and_shuts_down() {
    let harness = MockHarness::instant();
    let mut started = harness.start(start_ctx("mock", None)).await.unwrap();

    let first = next_ev(&mut started.events).await;
    assert!(matches!(first, HarnessEvent::NativeSessionId(_)));
    assert!(started.session.native_session_id().is_some());

    started
        .session
        .prompt(Prompt::text("hello world"))
        .await
        .unwrap();

    let mut saw_plan = false;
    let mut saw_tool = false;
    let mut text = String::new();
    loop {
        match next_ev(&mut started.events).await {
            HarnessEvent::Plan(_) => saw_plan = true,
            HarnessEvent::ToolCall(_) => saw_tool = true,
            HarnessEvent::MessageChunk(ContentBlock::Text { text: t }) => text.push_str(&t),
            HarnessEvent::TurnEnded { stop_reason, error } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert!(error.is_none());
                break;
            }
            _ => {}
        }
    }
    assert!(saw_plan);
    assert!(saw_tool);
    assert!(text.contains("hello world"));

    started.session.shutdown().await.unwrap();
    assert!(matches!(
        next_ev(&mut started.events).await,
        HarnessEvent::Exited { error: None }
    ));
    assert!(started.events.next().await.is_none());
}

#[tokio::test]
async fn mock_harness_handles_approval_allow_and_reject() {
    let harness = MockHarness::instant();
    let mut started = harness.start(start_ctx("mock", None)).await.unwrap();
    let _ = next_ev(&mut started.events).await; // NativeSessionId

    started
        .session
        .prompt(Prompt::text("please approve deploy"))
        .await
        .unwrap();

    let req_id = loop {
        if let HarnessEvent::PermissionRequested {
            request_id,
            options,
            ..
        } = next_ev(&mut started.events).await
        {
            assert!(options.iter().any(|o| o.option_id == "allow_once"));
            break request_id;
        }
    };

    started
        .session
        .respond_approval(
            &req_id,
            ApprovalDecision::Selected {
                option_id: "allow_once".into(),
            },
        )
        .await
        .unwrap();

    let mut reply = String::new();
    loop {
        match next_ev(&mut started.events).await {
            HarnessEvent::MessageChunk(ContentBlock::Text { text }) => reply.push_str(&text),
            HarnessEvent::TurnEnded { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    assert!(reply.contains("审批已通过"));
    started.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn mock_harness_cancels_slow_turn_and_rejects_overlap() {
    let harness = MockHarness::instant();
    let mut started = harness.start(start_ctx("mock", None)).await.unwrap();
    let _ = next_ev(&mut started.events).await;

    started
        .session
        .prompt(Prompt::text("slow stream"))
        .await
        .unwrap();
    let overlap = started.session.prompt(Prompt::text("second")).await;
    assert!(matches!(overlap, Err(CoreError::InvalidInput(_))));

    let _ = next_ev(&mut started.events).await; // first chunk
    started.session.cancel().await.unwrap();

    loop {
        if let HarnessEvent::TurnEnded { stop_reason, .. } = next_ev(&mut started.events).await {
            assert_eq!(stop_reason, StopReason::Cancelled);
            break;
        }
    }
    started.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn mock_harness_preserves_native_id_on_resume() {
    let harness = MockHarness::instant();
    let mut started = harness
        .start(start_ctx("mock", Some("prev-native-42")))
        .await
        .unwrap();
    assert_eq!(
        started.session.native_session_id().as_deref(),
        Some("prev-native-42")
    );
    assert_eq!(
        next_ev(&mut started.events).await,
        HarnessEvent::NativeSessionId("prev-native-42".into())
    );
    started.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn opencode_config_and_probe_work() {
    let cfg = OpenCodeConfig {
        binary: "/usr/bin/opencode".into(),
        extra_args: vec!["--log-level".into(), "error".into()],
        env: vec![("OPENAI_API_KEY".into(), "test-placeholder".into())],
        ..Default::default()
    };
    let spec = cfg.command_spec();
    assert_eq!(spec.program, "/usr/bin/opencode");
    assert_eq!(spec.args, vec!["acp", "--log-level", "error"]);
    assert_eq!(
        spec.env.get("OPENAI_API_KEY").map(String::as_str),
        Some("test-placeholder")
    );

    let harness = opencode_harness(cfg);
    assert_eq!(harness.id(), "opencode");
    assert!(harness.capabilities().native_approval);
    assert!(harness.capabilities().structured_tools);

    let missing = probe_opencode("definitely-nonexistent-opencode-bin-xyz").await;
    assert!(!missing.available);
    assert!(missing.error.is_some());
}
