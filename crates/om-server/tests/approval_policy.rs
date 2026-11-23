//! W1-3 approval policy, end to end: real SQLite + real `AcpHarness` driving
//! `om-fake-acp-agent` (Direct mode; the Bwrap auto-allow path is in `sandbox_bwrap.rs`).

use om_core::approval::{ApprovalDecision, ApprovalRecord, ApprovalStatus};
use om_core::content::{ContentBlock, Prompt};
use om_core::event::{EventPayload, SessionEvent, ToolKind};
use om_core::ids::{MemberId, SessionId, SpaceId};
use om_core::sandbox::{CommandSpec, SandboxMode};
use om_core::session::{SessionConfig, SessionStatus};
use om_server::acp::AcpHarness;
use om_server::approval::ApprovalPolicy;
use om_server::db::Db;
use om_server::orchestrator::{HarnessRegistry, Orchestrator, OrchestratorConfig, Subscription};
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    _dir: tempfile::TempDir,
    orch: Orchestrator,
    space: SpaceId,
}

async fn fixture(policy: ApprovalPolicy) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(AcpHarness::new(
        "fake",
        CommandSpec::new(env!("CARGO_BIN_EXE_om-fake-acp-agent")),
    )));
    let config = OrchestratorConfig {
        allow_direct_sandbox: true,
        ..OrchestratorConfig::default()
    };
    Fixture {
        _dir: dir,
        orch: Orchestrator::new(db, registry, config).with_approval_policy(policy),
        space,
    }
}

async fn ready(f: &Fixture) -> (SessionId, Subscription) {
    let s = f
        .orch
        .create_session(
            f.space.clone(),
            SessionConfig {
                harness_id: "fake".into(),
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
    until(&mut sub, |p| is_status(p, SessionStatus::Idle)).await;
    (s.id, sub)
}

fn is_status(p: &EventPayload, to: SessionStatus) -> bool {
    matches!(p, EventPayload::StatusChanged { to: t, .. } if *t == to)
}

async fn until(sub: &mut Subscription, pred: impl Fn(&EventPayload) -> bool) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), sub.next())
            .await
            .expect("timed out waiting for session event")
            .unwrap()
            .expect("subscription closed");
        let done = pred(&ev.payload);
        out.push(ev);
        if done {
            return out;
        }
    }
}

/// Runs one prompt to `Idle`, answering the first approval with `answer` if the
/// policy leaves it to members. Returns every event of the turn.
async fn turn(
    f: &Fixture,
    id: &SessionId,
    sub: &mut Subscription,
    text: &str,
    answer: Option<&str>,
) -> Vec<SessionEvent> {
    f.orch
        .submit_prompt(id, None, Prompt::text(text))
        .await
        .unwrap();
    let mut evs = until(sub, |p| {
        is_status(p, SessionStatus::Idle) || is_status(p, SessionStatus::WaitingApproval)
    })
    .await;
    if is_status(&evs.last().unwrap().payload, SessionStatus::WaitingApproval) {
        let approval = requested(&evs);
        let option = answer.expect("policy unexpectedly asked members");
        f.orch
            .resolve_approval(
                &approval.id,
                ApprovalDecision::Selected {
                    option_id: option.into(),
                },
                Some(MemberId::from("alice")),
            )
            .await
            .unwrap();
        evs.extend(until(sub, |p| is_status(p, SessionStatus::Idle)).await);
    }
    evs
}

fn requested(evs: &[SessionEvent]) -> ApprovalRecord {
    evs.iter()
        .find_map(|e| match &e.payload {
            EventPayload::ApprovalRequested { approval, .. } => Some(approval.clone()),
            _ => None,
        })
        .expect("approval requested")
}

fn resolved(evs: &[SessionEvent]) -> (ApprovalDecision, Option<MemberId>, Option<String>) {
    evs.iter()
        .find_map(|e| match &e.payload {
            EventPayload::ApprovalResolved {
                decision,
                resolved_by,
                auto_rule,
                ..
            } => Some((decision.clone(), resolved_by.clone(), auto_rule.clone())),
            _ => None,
        })
        .expect("approval resolved")
}

fn waited(evs: &[SessionEvent]) -> bool {
    evs.iter()
        .any(|e| is_status(&e.payload, SessionStatus::WaitingApproval))
}

fn reply(evs: &[SessionEvent]) -> String {
    evs.iter()
        .filter_map(|e| match &e.payload {
            EventPayload::AgentMessageChunk {
                content: ContentBlock::Text { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn selected(option: &str) -> ApprovalDecision {
    ApprovalDecision::Selected {
        option_id: option.into(),
    }
}

#[tokio::test]
async fn request_is_classified_from_the_earlier_tool_call() {
    let f = fixture(ApprovalPolicy::default()).await;
    let (id, mut sub) = ready(&f).await;
    // Direct mode: even with the default policy nothing is auto-allowed.
    let evs = turn(&f, &id, &mut sub, "read", Some("allow")).await;
    let a = requested(&evs);
    assert_eq!(a.tool_kind, Some(ToolKind::Read));
    assert_eq!(a.locations, vec!["README.md".to_string()]);
    let (decision, by, rule) = resolved(&evs);
    assert_eq!(decision, selected("allow"));
    assert_eq!(by, Some(MemberId::from("alice")));
    assert_eq!(rule, None);
    assert_eq!(reply(&evs), "ran it");

    let stored = f.orch.db().list_approvals(&id, false).await.unwrap();
    assert_eq!(stored[0].record.tool_kind, Some(ToolKind::Read));
    assert_eq!(stored[0].record.locations, vec!["README.md".to_string()]);
}

#[tokio::test]
async fn allow_always_is_remembered_per_command() {
    let f = fixture(ApprovalPolicy::default()).await;
    let (id, mut sub) = ready(&f).await;

    let first = turn(&f, &id, &mut sub, "tool always", Some("allow_always")).await;
    assert!(waited(&first));
    assert_eq!(requested(&first).tool_kind, Some(ToolKind::Execute));
    assert_eq!(reply(&first), "ran it");

    let second = turn(&f, &id, &mut sub, "tool always", None).await;
    assert!(
        !waited(&second),
        "remembered answer must not bother members"
    );
    let (decision, by, rule) = resolved(&second);
    assert_eq!(decision, selected("allow"), "auto answers are one-off");
    assert_eq!(by, None);
    assert_eq!(rule.as_deref(), Some("remembered:execute:rm -rf build"));
    assert_eq!(reply(&second), "ran it");

    let rules = f.orch.db().approval_rules(&id).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].rule_key, "execute:rm -rf build");
    assert!(rules[0].allow);
    assert_eq!(rules[0].created_by, Some(MemberId::from("alice")));

    // A different kind (read) is not covered by the execute rule.
    let other = turn(&f, &id, &mut sub, "read", Some("reject")).await;
    assert!(waited(&other));
    assert_eq!(reply(&other), "skipped");
}

#[tokio::test]
async fn reject_always_is_remembered_and_survives_resume() {
    let f = fixture(ApprovalPolicy::default()).await;
    let (id, mut sub) = ready(&f).await;

    let first = turn(&f, &id, &mut sub, "read always", Some("reject_always")).await;
    assert_eq!(reply(&first), "skipped");

    // Crash and resume: the rule is loaded from SQLite by the new actor.
    f.orch
        .submit_prompt(&id, None, Prompt::text("crash"))
        .await
        .unwrap();
    until(&mut sub, |p| is_status(p, SessionStatus::Failed)).await;
    f.orch.resume_session(&id).await.unwrap();
    until(&mut sub, |p| is_status(p, SessionStatus::Idle)).await;

    let second = turn(&f, &id, &mut sub, "read always", None).await;
    assert!(!waited(&second));
    let (decision, by, rule) = resolved(&second);
    assert_eq!(decision, selected("reject"));
    assert_eq!(by, None);
    assert_eq!(rule.as_deref(), Some("remembered:read"));
    assert_eq!(reply(&second), "skipped");
}

#[tokio::test]
async fn ask_always_policy_never_remembers() {
    let f = fixture(ApprovalPolicy::ask_always()).await;
    let (id, mut sub) = ready(&f).await;
    turn(&f, &id, &mut sub, "tool always", Some("allow_always")).await;
    let second = turn(&f, &id, &mut sub, "tool always", Some("reject")).await;
    assert!(waited(&second));
    assert!(f.orch.db().approval_rules(&id).await.unwrap().is_empty());
}

#[tokio::test]
async fn unanswered_request_times_out_as_a_rejection() {
    let policy = ApprovalPolicy {
        timeout: Some(Duration::from_millis(300)),
        ..ApprovalPolicy::default()
    };
    let f = fixture(policy).await;
    let (id, mut sub) = ready(&f).await;
    f.orch
        .submit_prompt(&id, None, Prompt::text("tool"))
        .await
        .unwrap();
    let mut evs = until(&mut sub, |p| is_status(p, SessionStatus::WaitingApproval)).await;
    let approval = requested(&evs);
    evs.extend(until(&mut sub, |p| is_status(p, SessionStatus::Idle)).await);
    let (decision, by, rule) = resolved(&evs);
    assert_eq!(decision, selected("reject"));
    assert_eq!(by, None);
    assert_eq!(rule.as_deref(), Some("timeout"));
    assert_eq!(reply(&evs), "skipped");

    // Too late for members now.
    let late = f
        .orch
        .resolve_approval(&approval.id, selected("allow"), Some(MemberId::from("bob")))
        .await;
    assert!(late.is_err());
    let stored = f.orch.db().get_approval(&approval.id).await.unwrap();
    assert_eq!(stored.record.status, ApprovalStatus::Resolved);
    assert_eq!(stored.record.auto_rule.as_deref(), Some("timeout"));
}

#[tokio::test]
async fn answered_request_does_not_time_out_later() {
    let policy = ApprovalPolicy {
        timeout: Some(Duration::from_millis(400)),
        ..ApprovalPolicy::default()
    };
    let f = fixture(policy).await;
    let (id, mut sub) = ready(&f).await;
    let evs = turn(&f, &id, &mut sub, "tool", Some("allow")).await;
    assert_eq!(reply(&evs), "ran it");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let stored = f.orch.db().list_approvals(&id, false).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].record.auto_rule, None);
    assert_eq!(stored[0].record.resolved_by, Some(MemberId::from("alice")));
}

#[tokio::test]
async fn every_step_is_audited() {
    let f = fixture(ApprovalPolicy::default()).await;
    let (id, mut sub) = ready(&f).await;
    let first = turn(&f, &id, &mut sub, "tool always", Some("allow_always")).await;
    let second = turn(&f, &id, &mut sub, "tool always", None).await;
    let a1 = requested(&first).id;
    let a2 = requested(&second).id;

    let audit = |target: String| {
        let db = f.orch.db().clone();
        async move {
            let mut rows = db.list_audit(Some(target), 50).await.unwrap();
            rows.reverse();
            rows
        }
    };
    let actions = |rows: &[om_server::db::AuditEntry]| {
        rows.iter().map(|r| r.action.clone()).collect::<Vec<_>>()
    };

    let r1 = audit(a1.to_string()).await;
    assert_eq!(actions(&r1), ["approval.requested", "approval.resolved"]);
    assert_eq!(r1[1].actor, Some(MemberId::from("alice")));
    assert_eq!(r1[1].detail["decision"]["option_id"], "allow_always");

    let r2 = audit(a2.to_string()).await;
    assert_eq!(
        actions(&r2),
        ["approval.requested", "approval.auto_resolved"]
    );
    assert_eq!(r2[1].actor, None);
    assert_eq!(r2[1].detail["rule"], "remembered:execute:rm -rf build");

    let rs = audit(id.to_string()).await;
    assert_eq!(actions(&rs), ["approval.rule_remembered"]);

    assert_eq!(
        f.orch.db().clear_approval_rules(&id, None).await.unwrap(),
        1
    );
    assert!(f.orch.db().approval_rules(&id).await.unwrap().is_empty());
}

#[tokio::test]
async fn revoking_rules_takes_effect_in_a_live_session() {
    let f = fixture(ApprovalPolicy::default()).await;
    let (id, mut sub) = ready(&f).await;
    turn(&f, &id, &mut sub, "tool always", Some("allow_always")).await;
    f.orch
        .db()
        .clear_approval_rules(&id, Some(MemberId::from("alice")))
        .await
        .unwrap();
    let again = turn(&f, &id, &mut sub, "tool always", Some("reject")).await;
    assert!(waited(&again), "cleared rule must not auto-answer");
    assert_eq!(reply(&again), "skipped");
}
