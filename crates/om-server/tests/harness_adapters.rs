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
    HarnessRegistry, MockHarness, OpenCodeConfig, opencode_harness, probe_opencode,
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
    let reg = HarnessRegistry::with_phase0_defaults();
    let list = reg.list();
    let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, vec!["mock", "opencode"]);
    assert!(reg.require("mock").is_ok());
    assert!(reg.require("opencode").is_ok());
    assert!(matches!(
        reg.require("unknown"),
        Err(CoreError::NotFound(_))
    ));
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
    assert!(spec.clear_env);
    assert!(!spec.env.contains_key("OM_TOKEN"));
    assert_eq!(spec.args, vec!["acp", "--log-level", "error"]);
    assert_eq!(
        spec.env.get("OPENAI_API_KEY").map(String::as_str),
        Some("test-placeholder")
    );

    let proxy_cfg = OpenCodeConfig::from_env_for_proxy();
    let proxy_spec = proxy_cfg.command_spec();
    assert!(proxy_spec.clear_env);
    assert!(!proxy_spec.env.contains_key("OPENAI_API_KEY"));
    assert!(!proxy_spec.env.contains_key("ANTHROPIC_API_KEY"));

    let harness = opencode_harness(cfg);
    assert_eq!(harness.id(), "opencode");
    assert!(harness.capabilities().native_approval);
    assert!(harness.capabilities().structured_tools);

    let missing = probe_opencode("definitely-nonexistent-opencode-bin-xyz").await;
    assert!(!missing.available);
    assert!(missing.error.is_some());
}

#[cfg(unix)]
fn write_executable_script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[tokio::test]
async fn phase1_registry_and_dsh_config_work() {
    use om_server::harness::{DshConfig, dsh_harness, probe_dsh, probe_registered_harnesses};

    let reg = HarnessRegistry::with_phase1_defaults(true);
    let ids: Vec<_> = reg.list().into_iter().map(|d| d.id).collect();
    assert_eq!(ids, vec!["cc", "codexcli", "dsh", "mock", "opencode"]);

    let dsh_cfg = DshConfig::from_env_for_proxy();
    let spec = dsh_cfg.command_spec();
    assert!(spec.clear_env);
    assert!(!spec.env.contains_key("DEEPSEEK_API_KEY"));
    assert_eq!(spec.args, vec!["acp"]);
    assert_eq!(dsh_harness(dsh_cfg).id(), "dsh");

    let missing = probe_dsh("definitely-nonexistent-dsh-xyz").await;
    assert!(!missing.available);

    let reports = probe_registered_harnesses(&reg).await;
    assert_eq!(reports.len(), 5);
    let mock_report = reports.iter().find(|r| r.id == "mock").unwrap();
    assert!(mock_report.available);
}

#[cfg(unix)]
#[tokio::test]
async fn cc_harness_parses_stream_json_and_resumes_native_session() {
    use om_server::harness::{ClaudeCodeConfig, cc_harness, probe_cc};

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-claude.sh");
    write_executable_script(
        &script,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "claude 2.0.0-test"
  exit 0
fi
ARGS="$*"
echo '{"type":"system","subtype":"init","session_id":"cc-sess-99"}'
echo '{"type":"assistant","session_id":"cc-sess-99","message":{"content":[{"type":"thinking","thinking":"analyzing"},{"type":"tool_use","id":"tu1","name":"Read","input":{"file_path":"/tmp/a.rs"}}]}}'
echo '{"type":"user","session_id":"cc-sess-99","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"fn main() {}"}]}}'
printf '{"type":"assistant","session_id":"cc-sess-99","message":{"content":[{"type":"text","text":"cc reply args=%s"}]}}\n' "$ARGS"
echo '{"type":"result","subtype":"success","session_id":"cc-sess-99","is_error":false,"result":"cc reply"}'
"#,
    );

    let probe = probe_cc(script.to_str().unwrap()).await;
    assert!(probe.available);
    assert_eq!(probe.version.as_deref(), Some("claude 2.0.0-test"));

    let harness = cc_harness(ClaudeCodeConfig {
        binary: script.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let mut started = harness
        .start(StartContext {
            session_id: SessionId::new(),
            config: SessionConfig {
                harness_id: "cc".into(),
                cwd: dir.path().to_string_lossy().into_owned(),
                workspace_id: None,
                model: Some("claude-sonnet".into()),
                sandbox: SandboxMode::Direct,
                title: None,
            },
            resume_native_session_id: None,
            sandbox: Arc::new(DirectSandbox),
        })
        .await
        .unwrap();

    // Turn 1: captures native session id, thought, tool call + update, and text
    started
        .session
        .prompt(Prompt::text("first turn"))
        .await
        .unwrap();

    let mut saw_thought = false;
    let mut saw_tool = false;
    let mut saw_tool_done = false;
    let mut reply1 = String::new();
    loop {
        match next_ev(&mut started.events).await {
            HarnessEvent::ThoughtChunk(_) => saw_thought = true,
            HarnessEvent::ToolCall(tc) => {
                assert_eq!(tc.tool_call_id, "tu1");
                assert_eq!(tc.locations, vec!["/tmp/a.rs"]);
                saw_tool = true;
            }
            HarnessEvent::ToolCallUpdate(u) => {
                assert_eq!(u.tool_call_id, "tu1");
                saw_tool_done = true;
            }
            HarnessEvent::MessageChunk(ContentBlock::Text { text }) => reply1.push_str(&text),
            HarnessEvent::TurnEnded { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_thought && saw_tool && saw_tool_done);
    assert_eq!(
        started.session.native_session_id().as_deref(),
        Some("cc-sess-99")
    );
    assert!(reply1.contains("--model claude-sonnet"));

    // Turn 2: automatically passes `--resume cc-sess-99`
    started
        .session
        .prompt(Prompt::text("second turn"))
        .await
        .unwrap();
    let mut reply2 = String::new();
    loop {
        match next_ev(&mut started.events).await {
            HarnessEvent::MessageChunk(ContentBlock::Text { text }) => reply2.push_str(&text),
            HarnessEvent::TurnEnded { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    assert!(reply2.contains("--resume cc-sess-99"));
    started.session.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn codexcli_harness_parses_json_events_and_resumes_thread() {
    use om_server::harness::{CodexCliConfig, codexcli_harness, probe_codexcli};

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-codex.sh");
    write_executable_script(
        &script,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "codex-cli 0.50.0-test"
  exit 0
fi
ARGS="$*"
echo '{"type":"thread.started","thread_id":"thr_777"}'
echo '{"type":"item.completed","item":{"id":"p1","type":"todo_list","items":[{"text":"run tests","completed":true}]}}'
echo '{"type":"item.started","item":{"id":"cmd1","type":"command_execution","command":"cargo test"}}'
echo '{"type":"item.completed","item":{"id":"cmd1","type":"command_execution","command":"cargo test","aggregated_output":"ok 10 passed","exit_code":0,"status":"completed"}}'
printf '{"type":"item.completed","item":{"id":"msg1","type":"agent_message","text":"codex ok args=%s"}}\n' "$ARGS"
"#,
    );

    let probe = probe_codexcli(script.to_str().unwrap()).await;
    assert!(probe.available);
    assert_eq!(probe.version.as_deref(), Some("codex-cli 0.50.0-test"));

    let harness = codexcli_harness(CodexCliConfig {
        binary: script.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let mut started = harness
        .start(StartContext {
            session_id: SessionId::new(),
            config: SessionConfig {
                harness_id: "codexcli".into(),
                cwd: dir.path().to_string_lossy().into_owned(),
                workspace_id: None,
                model: None,
                sandbox: SandboxMode::Direct,
                title: None,
            },
            resume_native_session_id: None,
            sandbox: Arc::new(DirectSandbox),
        })
        .await
        .unwrap();

    started
        .session
        .prompt(Prompt::text("hello codex"))
        .await
        .unwrap();

    let mut saw_plan = false;
    let mut saw_cmd = false;
    let mut reply1 = String::new();
    loop {
        match next_ev(&mut started.events).await {
            HarnessEvent::Plan(entries) => {
                assert_eq!(entries.len(), 1);
                saw_plan = true;
            }
            HarnessEvent::ToolCall(tc) => {
                assert_eq!(tc.title, "cargo test");
                saw_cmd = true;
            }
            HarnessEvent::MessageChunk(ContentBlock::Text { text }) => reply1.push_str(&text),
            HarnessEvent::TurnEnded { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_plan && saw_cmd);
    assert_eq!(
        started.session.native_session_id().as_deref(),
        Some("thr_777")
    );

    // Turn 2: automatically invokes `exec resume thr_777 --json`
    started
        .session
        .prompt(Prompt::text("follow up"))
        .await
        .unwrap();
    let mut reply2 = String::new();
    loop {
        match next_ev(&mut started.events).await {
            HarnessEvent::MessageChunk(ContentBlock::Text { text }) => reply2.push_str(&text),
            HarnessEvent::TurnEnded { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    assert!(reply2.contains("exec resume thr_777 --json"));
    started.session.shutdown().await.unwrap();
}
