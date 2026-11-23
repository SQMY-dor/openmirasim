use super::*;

fn opts() -> Vec<ApprovalOption> {
    [
        ("a1", ApprovalOptionKind::AllowOnce),
        ("aa", ApprovalOptionKind::AllowAlways),
        ("r1", ApprovalOptionKind::RejectOnce),
        ("ra", ApprovalOptionKind::RejectAlways),
    ]
    .into_iter()
    .map(|(id, kind)| ApprovalOption {
        option_id: id.into(),
        name: id.into(),
        kind,
    })
    .collect()
}

fn req<'a>(
    sandbox: SandboxMode,
    kind: Option<ToolKind>,
    locations: &'a [String],
    title: &'a str,
    options: &'a [ApprovalOption],
) -> PolicyRequest<'a> {
    PolicyRequest {
        sandbox,
        cwd: "/data/worktrees/s1",
        kind,
        locations,
        title,
        options,
    }
}

fn auto(rule: &str, option: &str) -> Verdict {
    Verdict::Auto {
        option_id: option.into(),
        rule: rule.into(),
    }
}

#[test]
fn within_checks_are_lexical_and_strict() {
    let cwd = "/data/worktrees/s1";
    let ok = |l: &[&str]| all_within(cwd, &l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert!(ok(&["src/main.rs"]));
    assert!(ok(&["/data/worktrees/s1/a/../b.txt", "./c"]));
    assert!(!ok(&[]), "unknown scope is not within");
    assert!(!ok(&["../s2/x"]));
    assert!(
        !ok(&["/data/worktrees/s1x/y"]),
        "prefix of a sibling is not within"
    );
    assert!(!ok(&["/etc/passwd"]));
    assert!(!ok(&["src/ok.rs", "/tmp/x"]), "every path must be within");
    assert!(!ok(&[".git/config"]));
    assert!(!ok(&["sub/.git/hooks/pre-commit"]));
    assert!(!ok(&["/../../etc"]));
    assert!(!all_within("relative/cwd", &["a".into()]));
    assert!(!ok(&[""]), "an empty path is not a scope");
    assert!(!ok(&["src/a.rs", "  "]));
}

#[test]
fn sandboxed_reads_and_edits_in_worktree_auto_allow() {
    let p = ApprovalPolicy::default();
    let o = opts();
    let none = HashMap::new();
    let inside = vec!["src/lib.rs".to_string()];
    for (kind, rule) in [
        (ToolKind::Read, "read_in_worktree"),
        (ToolKind::Search, "read_in_worktree"),
        (ToolKind::Edit, "edit_in_worktree"),
    ] {
        assert_eq!(
            p.evaluate(
                &req(SandboxMode::Bwrap, Some(kind), &inside, "t", &o),
                &none
            ),
            auto(rule, "a1")
        );
    }
    assert_eq!(
        p.evaluate(
            &req(SandboxMode::Bwrap, Some(ToolKind::Think), &[], "t", &o),
            &none
        ),
        auto("think", "a1")
    );
}

#[test]
fn risky_kinds_outside_paths_and_direct_mode_ask() {
    let p = ApprovalPolicy::default();
    let o = opts();
    let none = HashMap::new();
    let inside = vec!["src/lib.rs".to_string()];
    let outside = vec!["/home/alice/.ssh/id_ed25519".to_string()];
    for kind in [
        ToolKind::Execute,
        ToolKind::Fetch,
        ToolKind::Delete,
        ToolKind::Move,
        ToolKind::SwitchMode,
        ToolKind::Other,
    ] {
        assert_eq!(
            p.evaluate(
                &req(SandboxMode::Bwrap, Some(kind), &inside, "t", &o),
                &none
            ),
            Verdict::Ask,
            "{kind:?}"
        );
    }
    assert_eq!(
        p.evaluate(
            &req(SandboxMode::Bwrap, Some(ToolKind::Read), &outside, "t", &o),
            &none
        ),
        Verdict::Ask
    );
    assert_eq!(
        p.evaluate(
            &req(SandboxMode::Bwrap, Some(ToolKind::Edit), &[], "t", &o),
            &none
        ),
        Verdict::Ask,
        "edit with unknown paths"
    );
    assert_eq!(
        p.evaluate(&req(SandboxMode::Bwrap, None, &inside, "t", &o), &none),
        Verdict::Ask,
        "no kind"
    );
    assert_eq!(
        p.evaluate(
            &req(SandboxMode::Direct, Some(ToolKind::Read), &inside, "t", &o),
            &none
        ),
        Verdict::Ask,
        "no OS boundary, no auto-allow"
    );
    assert_eq!(
        ApprovalPolicy::ask_always().evaluate(
            &req(SandboxMode::Bwrap, Some(ToolKind::Read), &inside, "t", &o),
            &none
        ),
        Verdict::Ask
    );
}

#[test]
fn auto_allow_needs_an_allow_once_option() {
    let p = ApprovalPolicy::default();
    let only_always: Vec<_> = opts()
        .into_iter()
        .filter(|o| o.kind != ApprovalOptionKind::AllowOnce)
        .collect();
    let inside = vec!["a".to_string()];
    assert_eq!(
        p.evaluate(
            &req(
                SandboxMode::Bwrap,
                Some(ToolKind::Read),
                &inside,
                "t",
                &only_always
            ),
            &HashMap::new()
        ),
        Verdict::Ask,
        "never escalate a one-off auto answer into a permanent agent-side grant"
    );
}

#[test]
fn rule_keys_scope_commands_by_title() {
    assert_eq!(rule_key(Some(ToolKind::Read), "x").as_deref(), Some("read"));
    assert_eq!(
        rule_key(Some(ToolKind::Execute), " cargo test ").as_deref(),
        Some("execute:cargo test")
    );
    assert_eq!(rule_key(None, "x"), None);
    assert_eq!(rule_key(Some(ToolKind::Execute), "  "), None);
    assert_eq!(rule_key(Some(ToolKind::Fetch), PLACEHOLDER_TITLE), None);
}

#[test]
fn remembered_file_allows_stay_in_the_sandboxed_worktree() {
    let p = ApprovalPolicy::ask_always();
    let p = ApprovalPolicy {
        remember_always: true,
        ..p
    };
    let o = opts();
    let mut mem = HashMap::new();
    mem.insert("edit".to_string(), Remembered { allow: true });
    mem.insert("delete:rm a".to_string(), Remembered { allow: true });
    let inside = vec!["src/a.rs".to_string()];
    let git = vec![".git/config".to_string()];
    let outside = vec!["/home/x/.bashrc".to_string()];
    assert_eq!(
        p.evaluate(&req(SandboxMode::Bwrap, Some(ToolKind::Edit), &inside, "t", &o), &mem),
        auto("remembered:edit", "a1")
    );
    for locs in [&git, &outside, &vec![]] {
        assert_eq!(
            p.evaluate(&req(SandboxMode::Bwrap, Some(ToolKind::Edit), locs, "t", &o), &mem),
            Verdict::Ask,
            "{locs:?}"
        );
    }
    assert_eq!(
        p.evaluate(&req(SandboxMode::Direct, Some(ToolKind::Edit), &inside, "t", &o), &mem),
        Verdict::Ask,
        "no remembered file allow outside the sandbox"
    );
    assert_eq!(
        p.evaluate(&req(SandboxMode::Bwrap, Some(ToolKind::Delete), &git, "rm a", &o), &mem),
        Verdict::Ask
    );
}

#[test]
fn remembered_answers_apply_and_reject_wins_everywhere() {
    let p = ApprovalPolicy::default();
    let o = opts();
    let mut mem = HashMap::new();
    mem.insert("execute:cargo test".to_string(), Remembered { allow: true });
    mem.insert("read".to_string(), Remembered { allow: false });
    let inside = vec!["a".to_string()];

    assert_eq!(
        p.evaluate(
            &req(
                SandboxMode::Bwrap,
                Some(ToolKind::Execute),
                &[],
                "cargo test",
                &o
            ),
            &mem
        ),
        auto("remembered:execute:cargo test", "a1")
    );
    assert_eq!(
        p.evaluate(
            &req(
                SandboxMode::Bwrap,
                Some(ToolKind::Execute),
                &[],
                "rm -rf /",
                &o
            ),
            &mem
        ),
        Verdict::Ask
    );
    // A remembered reject beats the in-worktree auto-allow, in any sandbox mode.
    for mode in [SandboxMode::Bwrap, SandboxMode::Direct] {
        assert_eq!(
            p.evaluate(&req(mode, Some(ToolKind::Read), &inside, "t", &o), &mem),
            auto("remembered:read", "r1")
        );
    }
    let mut off = p.clone();
    off.remember_always = false;
    assert_eq!(
        off.evaluate(
            &req(
                SandboxMode::Bwrap,
                Some(ToolKind::Execute),
                &[],
                "cargo test",
                &o
            ),
            &mem
        ),
        Verdict::Ask
    );
}

#[test]
fn remembers_only_always_options() {
    assert_eq!(
        remembers(ApprovalOptionKind::AllowAlways),
        Some(Remembered { allow: true })
    );
    assert_eq!(
        remembers(ApprovalOptionKind::RejectAlways),
        Some(Remembered { allow: false })
    );
    assert_eq!(remembers(ApprovalOptionKind::AllowOnce), None);
    assert_eq!(
        ApprovalPolicy::timeout_option(&opts()).as_deref(),
        Some("r1")
    );
}
