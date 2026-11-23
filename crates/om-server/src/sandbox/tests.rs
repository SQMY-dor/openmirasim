use super::*;

fn sb() -> BwrapSandbox {
    BwrapSandbox::new(BwrapConfig {
        bwrap_path: "/usr/bin/bwrap".into(),
        system_paths: vec!["/usr".into()],
        env_passthrough: vec![],
        ..BwrapConfig::default()
    })
}

fn spec(dir: &Path) -> SandboxSpec {
    SandboxSpec {
        writable_paths: vec![dir.to_path_buf()],
        ..SandboxSpec::default()
    }
}

/// Position of `window` inside `args`.
fn find(args: &[String], window: &[&str]) -> Option<usize> {
    args.windows(window.len())
        .position(|w| w.iter().zip(window).all(|(a, b)| a == b))
}

#[test]
fn wraps_into_bwrap_with_isolation_flags() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_string_lossy().into_owned();
    let cmd = CommandSpec::new("/usr/bin/env")
        .arg("-0")
        .cwd(dir.path())
        .env("OM_TOKEN_FOR_AGENT", "x");
    let out = sb().wrap(&spec(dir.path()), cmd).unwrap();
    assert_eq!(out.program, "/usr/bin/bwrap");
    assert!(out.clear_env);
    assert_eq!(out.cwd, None);
    let a = &out.args;
    for flag in [
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
    ] {
        assert!(a.iter().any(|x| x == flag), "missing {flag}");
    }
    assert!(!a.iter().any(|x| x == "--share-net"));
    assert!(find(a, &["--bind", &d, &d]).is_some());
    assert!(find(a, &["--chdir", &d]).is_some());
    assert!(find(a, &["--ro-bind-try", "/usr", "/usr"]).is_some());
    // The agent command follows the final `--`, untouched.
    let sep = a.iter().rposition(|x| x == "--").unwrap();
    assert_eq!(&a[sep + 1..], ["/usr/bin/env", "-0"]);
    // Env: explicit only, plus HOME/TMPDIR.
    assert_eq!(
        out.env.get("OM_TOKEN_FOR_AGENT").map(String::as_str),
        Some("x")
    );
    assert_eq!(
        out.env.get("HOME").map(String::as_str),
        Some(SANDBOX_TMP_HOME)
    );
    assert_eq!(out.env.len(), 3);
}

#[test]
fn network_and_home_and_readonly_paths() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let h = home.path().to_string_lossy().into_owned();
    let s = SandboxSpec {
        writable_paths: vec![dir.path().into()],
        readonly_paths: vec!["/opt/toolchain".into(), "/opt/toolchain".into()],
        home: Some(home.path().into()),
        network: true,
        ..SandboxSpec::default()
    };
    let out = sb().wrap(&s, CommandSpec::new("/usr/bin/true")).unwrap();
    assert!(out.args.iter().any(|x| x == "--share-net"));
    assert!(find(&out.args, &["--bind", &h, &h]).is_some());
    assert_eq!(out.env["HOME"], h);
    let n = out.args.iter().filter(|x| *x == "/opt/toolchain").count();
    assert_eq!(n, 2, "read-only path bound exactly once (src + dest)");
}

#[test]
fn program_outside_system_paths_gets_its_dir_bound() {
    let bin = tempfile::tempdir().unwrap();
    let agent = bin.path().join("agent");
    std::fs::write(&agent, "").unwrap();
    let b = bin.path().to_string_lossy().into_owned();
    let out = sb()
        .wrap(
            &SandboxSpec::default(),
            CommandSpec::new(agent.to_string_lossy()),
        )
        .unwrap();
    assert!(find(&out.args, &["--ro-bind", &b, &b]).is_some());

    // Bare names resolve through the PATH given to the agent.
    let out = sb()
        .wrap(
            &SandboxSpec::default(),
            CommandSpec::new("agent").env("PATH", b.clone()),
        )
        .unwrap();
    assert!(out.args.last().unwrap().ends_with("/agent"));
}

#[test]
fn rejects_unsafe_or_inconsistent_specs() {
    let dir = tempfile::tempdir().unwrap();
    let s = sb();
    let rel = SandboxSpec {
        writable_paths: vec!["relative/dir".into()],
        ..SandboxSpec::default()
    };
    assert!(matches!(
        s.wrap(&rel, CommandSpec::new("/usr/bin/true")),
        Err(CoreError::InvalidInput(_))
    ));
    let missing = SandboxSpec {
        writable_paths: vec!["/definitely/not/here".into()],
        ..SandboxSpec::default()
    };
    assert!(matches!(
        s.wrap(&missing, CommandSpec::new("/usr/bin/true")),
        Err(CoreError::Sandbox(_))
    ));
    // cwd must be inside something exposed.
    let err = s
        .wrap(
            &spec(dir.path()),
            CommandSpec::new("/usr/bin/true").cwd("/etc"),
        )
        .unwrap_err();
    assert!(matches!(err, CoreError::Sandbox(m) if m.contains("cwd")));
    assert!(
        s.wrap(&spec(dir.path()), CommandSpec::new("./agent"))
            .is_err()
    );
    assert!(
        s.wrap(
            &spec(dir.path()),
            CommandSpec::new("no-such-agent-xyz").env("PATH", "/nonexistent")
        )
        .is_err()
    );
}

#[test]
fn server_environment_is_not_inherited() {
    // SAFETY: test-only; no other test in this module reads this variable.
    unsafe { std::env::set_var("OM_TEST_SECRET_API_KEY", "leak") };
    let s = BwrapSandbox::new(BwrapConfig {
        env_passthrough: vec!["OM_TEST_PASSTHROUGH".into()],
        ..BwrapConfig::default()
    });
    let out = s
        .wrap(&SandboxSpec::default(), CommandSpec::new("/usr/bin/true"))
        .unwrap();
    assert!(!out.env.contains_key("OM_TEST_SECRET_API_KEY"));
}

#[test]
fn cgroup_limits_use_systemd_run_scope() {
    let dir = tempfile::tempdir().unwrap();
    let limited = SandboxSpec {
        limits: ResourceLimits {
            memory_max_bytes: Some(512 << 20),
            cpu_quota_percent: Some(150),
            pids_max: None,
        },
        ..spec(dir.path())
    };
    // Disabled cgroups refuse limits instead of silently ignoring them.
    assert!(
        sb().wrap(&limited, CommandSpec::new("/usr/bin/true"))
            .is_err()
    );

    let s = BwrapSandbox::new(BwrapConfig {
        cgroups: CgroupMode::SystemdUserScope,
        default_limits: ResourceLimits {
            pids_max: Some(256),
            ..ResourceLimits::default()
        },
        bwrap_path: "/usr/bin/bwrap".into(),
        ..BwrapConfig::default()
    });
    let out = s.wrap(&limited, CommandSpec::new("/usr/bin/true")).unwrap();
    assert_eq!(out.program, "systemd-run");
    let a = &out.args;
    assert_eq!(&a[..4], ["--user", "--scope", "--quiet", "--collect"]);
    assert!(find(a, &["-p", "MemoryMax=536870912"]).is_some());
    assert!(find(a, &["-p", "CPUQuota=150%"]).is_some());
    assert!(
        find(a, &["-p", "TasksMax=256"]).is_some(),
        "default limit applied"
    );
    assert!(find(a, &["--", "/usr/bin/bwrap", "--die-with-parent"]).is_some());

    // Without any limits, no systemd-run prefix.
    let s2 = BwrapSandbox::new(BwrapConfig {
        cgroups: CgroupMode::SystemdSystemScope,
        ..BwrapConfig::default()
    });
    let out = s2
        .wrap(&spec(dir.path()), CommandSpec::new("/usr/bin/true"))
        .unwrap();
    assert_eq!(out.program, "bwrap");
}

#[test]
fn merged_usr_symlinks_are_recreated() {
    let s = BwrapSandbox::default();
    let out = s
        .wrap(&SandboxSpec::default(), CommandSpec::new("/usr/bin/true"))
        .unwrap();
    if std::fs::symlink_metadata("/bin")
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let target = std::fs::read_link("/bin")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(find(&out.args, &["--symlink", &target, "/bin"]).is_some());
    }
    assert!(!out.args.iter().any(|x| x == "/etc/shadow"));
    assert!(
        !out.args
            .iter()
            .any(|x| x == "/root" || x == "/home" || x == "/var")
    );
}
