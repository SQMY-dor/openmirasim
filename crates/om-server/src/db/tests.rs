use super::*;
use om_core::approval::{ApprovalOption, ApprovalOptionKind};
use om_core::content::ContentBlock;
use om_core::event::ToolKind;
use om_core::ids::TurnId;
use om_core::sandbox::SandboxMode;

fn config() -> SessionConfig {
    SessionConfig {
        harness_id: "mock".into(),
        cwd: "/tmp".into(),
        workspace_id: None,
        model: None,
        sandbox: SandboxMode::Direct,
        title: Some("t".into()),
    }
}

fn chunk(text: &str) -> EventPayload {
    EventPayload::AgentMessageChunk {
        turn_id: TurnId::from("turn"),
        content: ContentBlock::Text { text: text.into() },
    }
}

fn status(from: SessionStatus, to: SessionStatus) -> EventPayload {
    EventPayload::StatusChanged { from, to }
}

async fn setup() -> (tempfile::TempDir, Db, SpaceId) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    (dir, db, space)
}

fn pending_approval(session_id: &SessionId) -> StoredApproval {
    StoredApproval {
        harness_request_id: "rpc-7".into(),
        record: ApprovalRecord {
            id: ApprovalId::new(),
            session_id: session_id.clone(),
            tool_call_id: Some("call-1".into()),
            title: "rm -rf build".into(),
            tool_kind: Some(ToolKind::Execute),
            locations: vec!["build".into()],
            options: vec![
                ApprovalOption {
                    option_id: "allow".into(),
                    name: "Allow".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    option_id: "reject".into(),
                    name: "Reject".into(),
                    kind: ApprovalOptionKind::RejectOnce,
                },
            ],
            status: ApprovalStatus::Pending,
            decision: None,
            resolved_by: None,
            auto_rule: None,
            created_at: now_ms(),
            resolved_at: None,
        },
    }
}

#[tokio::test]
async fn wal_mode_and_migrations() {
    let (_dir, db, _) = setup().await;
    let (mode, version): (String, i64) = db
        .read(|c| {
            Ok((
                c.pragma_query_value(None, "journal_mode", |r| r.get(0))
                    .map_err(storage)?,
                c.pragma_query_value(None, "user_version", |r| r.get(0))
                    .map_err(storage)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    assert_eq!(version as usize, migrations::MIGRATIONS.len());
}

#[tokio::test]
async fn ensure_space_is_idempotent() {
    let (_dir, db, space) = setup().await;
    assert_eq!(db.ensure_space("default").await.unwrap(), space);
    assert_ne!(db.ensure_space("other").await.unwrap(), space);
}

#[tokio::test]
async fn create_session_writes_first_event() {
    let (_dir, db, space) = setup().await;
    let (summary, event) = db
        .create_session(space.clone(), config(), None)
        .await
        .unwrap();
    assert_eq!(summary.status, SessionStatus::Starting);
    assert_eq!(summary.head_seq, 1);
    assert_eq!(event.seq, 1);
    assert!(matches!(event.payload, EventPayload::SessionCreated { .. }));
    let listed = db.list_sessions(&space).await.unwrap();
    assert_eq!(listed, vec![summary]);
}

#[tokio::test]
async fn append_assigns_gapless_seq_and_tracks_status() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let events = db
        .append_events(
            &s.id,
            vec![
                EventPayload::HarnessReady {
                    capabilities: Default::default(),
                    native_session_id: Some("native-1".into()),
                },
                status(SessionStatus::Starting, SessionStatus::Idle),
                status(SessionStatus::Idle, SessionStatus::Running),
                chunk("a"),
                chunk("b"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![2, 3, 4, 5, 6]
    );
    let s = db.get_session(&s.id).await.unwrap();
    assert_eq!(s.head_seq, 6);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(s.native_session_id.as_deref(), Some("native-1"));
}

#[tokio::test]
async fn illegal_status_change_rolls_back_whole_batch() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let err = db
        .append_events(
            &s.id,
            vec![
                chunk("lost"),
                status(SessionStatus::Starting, SessionStatus::Running),
            ],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidTransition { .. }));
    let s = db.get_session(&s.id).await.unwrap();
    assert_eq!(s.head_seq, 1, "nothing from the failed batch is persisted");
    assert_eq!(s.status, SessionStatus::Starting);
}

#[tokio::test]
async fn stale_status_from_is_rejected() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let err = db
        .append_events(
            &s.id,
            vec![status(SessionStatus::Idle, SessionStatus::Running)],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidInput(_)));
}

#[tokio::test]
async fn list_events_pages_after_seq() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    db.append_events(&s.id, (0..10).map(|i| chunk(&i.to_string())).collect())
        .await
        .unwrap();
    let page = db.list_events(&s.id, 3, 4).await.unwrap();
    assert_eq!(page.head_seq, 11);
    assert!(page.has_more);
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![4, 5, 6, 7]
    );
    let tail = db.list_events(&s.id, 9, 100).await.unwrap();
    assert!(!tail.has_more);
    assert_eq!(tail.events.len(), 2);
    let none = db.list_events(&s.id, 11, 100).await.unwrap();
    assert!(none.events.is_empty());
}

#[tokio::test]
async fn unknown_session_is_not_found() {
    let (_dir, db, _) = setup().await;
    let missing = SessionId::from("missing");
    assert!(matches!(
        db.get_session(&missing).await,
        Err(CoreError::NotFound(_))
    ));
    assert!(matches!(
        db.append_events(&missing, vec![chunk("x")]).await,
        Err(CoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("om.db");
    let id = {
        let db = Db::open(&path).unwrap();
        let space = db.ensure_space("default").await.unwrap();
        let (s, _) = db.create_session(space, config(), None).await.unwrap();
        db.append_events(&s.id, vec![chunk("persist me")])
            .await
            .unwrap();
        s.id
    };
    let db = Db::open(&path).unwrap();
    let page = db.list_events(&id, 0, 100).await.unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[1].payload, chunk("persist me"));
}

#[tokio::test]
async fn approval_resolution_is_first_writer_wins() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let approval = pending_approval(&s.id);
    let id = approval.record.id.clone();
    let ev = db.create_approval(approval, None).await.unwrap();
    assert!(matches!(ev.payload, EventPayload::ApprovalRequested { .. }));

    let mut tasks = Vec::new();
    for i in 0..8 {
        let db = db.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            db.resolve_approval(
                &id,
                ApprovalDecision::Selected {
                    option_id: "allow".into(),
                },
                Some(MemberId::from(format!("m{i}"))),
            )
            .await
        }));
    }
    let mut ok = 0;
    let mut conflicts = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(outcome) => {
                ok += 1;
                assert_eq!(outcome.approval.record.status, ApprovalStatus::Resolved);
                assert_eq!(outcome.approval.harness_request_id, "rpc-7");
            }
            Err(CoreError::ApprovalAlreadyResolved(_)) => conflicts += 1,
            Err(e) => panic!("unexpected error {e}"),
        }
    }
    assert_eq!((ok, conflicts), (1, 7));
    assert!(db.list_approvals(&s.id, true).await.unwrap().is_empty());
}

#[tokio::test]
async fn approval_rejects_unknown_option() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let approval = pending_approval(&s.id);
    let id = approval.record.id.clone();
    db.create_approval(approval, None).await.unwrap();
    let err = db
        .resolve_approval(
            &id,
            ApprovalDecision::Selected {
                option_id: "nope".into(),
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidInput(_)));
    assert_eq!(db.list_approvals(&s.id, true).await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancel_pending_approvals_emits_events() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    db.create_approval(pending_approval(&s.id), None)
        .await
        .unwrap();
    db.create_approval(pending_approval(&s.id), None)
        .await
        .unwrap();
    let outcomes = db.cancel_pending_approvals(&s.id).await.unwrap();
    assert_eq!(outcomes.len(), 2);
    for o in &outcomes {
        assert_eq!(o.approval.record.status, ApprovalStatus::Cancelled);
        assert!(matches!(
            o.event.payload,
            EventPayload::ApprovalResolved {
                decision: ApprovalDecision::Cancelled,
                ..
            }
        ));
    }
    assert_eq!(db.list_approvals(&s.id, false).await.unwrap().len(), 2);
}

#[tokio::test]
async fn recovery_fails_live_sessions_and_cancels_approvals() {
    let (_dir, db, space) = setup().await;
    let (running, _) = db
        .create_session(space.clone(), config(), None)
        .await
        .unwrap();
    db.append_events(
        &running.id,
        vec![
            status(SessionStatus::Starting, SessionStatus::Idle),
            status(SessionStatus::Idle, SessionStatus::Running),
        ],
    )
    .await
    .unwrap();
    db.create_approval(pending_approval(&running.id), None)
        .await
        .unwrap();
    let (closed, _) = db.create_session(space, config(), None).await.unwrap();
    db.append_events(
        &closed.id,
        vec![status(SessionStatus::Starting, SessionStatus::Closed)],
    )
    .await
    .unwrap();

    let recovered = db.recover_after_restart().await.unwrap();
    assert_eq!(recovered, vec![running.id.clone()]);
    let s = db.get_session(&running.id).await.unwrap();
    assert_eq!(s.status, SessionStatus::Failed);
    assert!(
        db.list_approvals(&running.id, true)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.get_session(&closed.id).await.unwrap().status,
        SessionStatus::Closed
    );
    // Idempotent: a second pass finds nothing live.
    assert!(db.recover_after_restart().await.unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_appends_stay_gapless() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..20 {
        let db = db.clone();
        let id = s.id.clone();
        tasks.push(tokio::spawn(async move {
            db.append_events(&id, vec![chunk(&i.to_string()), chunk("x")])
                .await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let page = db.list_events(&s.id, 0, 1000).await.unwrap();
    let seqs: Vec<u64> = page.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=41).collect::<Vec<_>>());
}

#[tokio::test]
async fn policy_resolution_records_rule_metadata_and_audit() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let a = pending_approval(&s.id);
    let id = a.record.id.clone();
    db.create_approval(a, None).await.unwrap();

    let stored = db.get_approval(&id).await.unwrap();
    assert_eq!(stored.record.tool_kind, Some(ToolKind::Execute));
    assert_eq!(stored.record.locations, vec!["build".to_string()]);

    let outcome = db
        .resolve_approval_by_policy(
            &id,
            ApprovalDecision::Selected {
                option_id: "reject".into(),
            },
            "timeout".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.approval.record.auto_rule.as_deref(),
        Some("timeout")
    );
    assert_eq!(outcome.approval.record.resolved_by, None);
    assert!(matches!(
        &outcome.event.payload,
        EventPayload::ApprovalResolved { auto_rule: Some(r), resolved_by: None, .. } if r == "timeout"
    ));
    // First writer wins against members too.
    assert!(matches!(
        db.resolve_approval(
            &id,
            ApprovalDecision::Selected {
                option_id: "allow".into()
            },
            Some(MemberId::from("alice"))
        )
        .await,
        Err(CoreError::ApprovalAlreadyResolved(_))
    ));

    let audit = db.list_audit(&s.space_id, Some(id.to_string()), 10).await.unwrap();
    let actions: Vec<_> = audit.iter().map(|a| a.action.as_str()).collect();
    assert_eq!(actions, ["approval.auto_resolved", "approval.requested"]);
}

#[tokio::test]
async fn approval_rules_upsert_and_clear() {
    let (_dir, db, space) = setup().await;
    let (s, _) = db.create_session(space, config(), None).await.unwrap();
    let rule = |allow: bool| ApprovalRule {
        rule_key: "execute:cargo test".into(),
        allow,
        option_kind: if allow {
            ApprovalOptionKind::AllowAlways
        } else {
            ApprovalOptionKind::RejectAlways
        },
        created_by: Some(MemberId::from("alice")),
        created_at: 1,
    };
    db.remember_approval_rule(&s.id, rule(true)).await.unwrap();
    db.remember_approval_rule(&s.id, rule(false)).await.unwrap();
    let rules = db.approval_rules(&s.id).await.unwrap();
    assert_eq!(
        rules,
        vec![rule(false)],
        "latest answer replaces the earlier one"
    );
    assert_eq!(db.clear_approval_rules(&s.id, None).await.unwrap(), 1);
    assert!(db.approval_rules(&s.id).await.unwrap().is_empty());
}
