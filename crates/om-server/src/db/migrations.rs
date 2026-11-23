//! Embedded schema migrations, applied in order and tracked with `PRAGMA user_version`.

/// Each entry is one migration; index + 1 is the resulting `user_version`.
/// Never edit a released migration: append a new one instead.
pub(super) const MIGRATIONS: &[&str] = &[
    // v1: Phase 0 core schema.
    r#"
    CREATE TABLE spaces (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL,
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE members (
        id            TEXT PRIMARY KEY,
        space_id      TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        display_name  TEXT NOT NULL,
        role          TEXT NOT NULL,
        created_at    INTEGER NOT NULL
    );
    CREATE INDEX members_space ON members(space_id);

    CREATE TABLE sessions (
        id                 TEXT PRIMARY KEY,
        space_id           TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        config             TEXT NOT NULL,
        status             TEXT NOT NULL,
        head_seq           INTEGER NOT NULL DEFAULT 0,
        native_session_id  TEXT,
        created_at         INTEGER NOT NULL,
        updated_at         INTEGER NOT NULL
    );
    CREATE INDEX sessions_space ON sessions(space_id, created_at DESC);

    CREATE TABLE events (
        session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        seq         INTEGER NOT NULL,
        ts          INTEGER NOT NULL,
        type        TEXT NOT NULL,
        payload     TEXT NOT NULL,
        PRIMARY KEY (session_id, seq)
    ) WITHOUT ROWID;

    CREATE TABLE approvals (
        id                  TEXT PRIMARY KEY,
        session_id          TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        harness_request_id  TEXT NOT NULL,
        tool_call_id        TEXT,
        title               TEXT NOT NULL,
        options             TEXT NOT NULL,
        status              TEXT NOT NULL,
        decision            TEXT,
        resolved_by         TEXT,
        created_at          INTEGER NOT NULL,
        resolved_at         INTEGER
    );
    CREATE INDEX approvals_session ON approvals(session_id, status);

    CREATE TABLE audit_logs (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        ts      INTEGER NOT NULL,
        actor   TEXT,
        action  TEXT NOT NULL,
        target  TEXT,
        detail  TEXT
    );
    "#,
    // v2 (W1-2): registered repositories and per-session worktrees / scratch dirs.
    r#"
    CREATE TABLE workspaces (
        id              TEXT PRIMARY KEY,
        space_id        TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        name            TEXT NOT NULL,
        repo_path       TEXT NOT NULL,
        git_common_dir  TEXT NOT NULL,
        default_branch  TEXT NOT NULL,
        created_at      INTEGER NOT NULL,
        UNIQUE (space_id, name)
    );

    CREATE TABLE worktrees (
        session_id    TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
        workspace_id  TEXT REFERENCES workspaces(id) ON DELETE SET NULL,
        path          TEXT NOT NULL UNIQUE,
        branch        TEXT,
        base_commit   TEXT,
        git_dir       TEXT,
        home_path     TEXT,
        created_at    INTEGER NOT NULL,
        removed_at    INTEGER
    );
    CREATE INDEX worktrees_workspace ON worktrees(workspace_id);
    "#,
    // v3 (W1-3): approval policy metadata, remembered "always" rules and audit index.
    r#"
    ALTER TABLE approvals ADD COLUMN tool_kind TEXT;
    ALTER TABLE approvals ADD COLUMN locations TEXT;
    ALTER TABLE approvals ADD COLUMN auto_rule TEXT;

    CREATE TABLE approval_rules (
        session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        rule_key    TEXT NOT NULL,
        allow       INTEGER NOT NULL,
        option_kind TEXT NOT NULL,
        created_by  TEXT,
        created_at  INTEGER NOT NULL,
        PRIMARY KEY (session_id, rule_key)
    ) WITHOUT ROWID;

    CREATE INDEX audit_logs_target ON audit_logs(target, ts);
    "#,
    // v4 (W2-1): member disabling and hashed device tokens.
    r#"
    ALTER TABLE members ADD COLUMN disabled_at INTEGER;

    CREATE TABLE device_tokens (
        id            TEXT PRIMARY KEY,
        member_id     TEXT NOT NULL REFERENCES members(id) ON DELETE CASCADE,
        token_hash    TEXT NOT NULL UNIQUE,
        label         TEXT NOT NULL,
        created_at    INTEGER NOT NULL,
        expires_at    INTEGER,
        last_used_at  INTEGER,
        revoked_at    INTEGER
    );
    CREATE INDEX device_tokens_member ON device_tokens(member_id);

    CREATE INDEX audit_logs_actor ON audit_logs(actor, ts);
    "#,
    // v5 (W4-1): schedules and their runs.
    r#"
    CREATE TABLE schedules (
        id           TEXT PRIMARY KEY,
        space_id     TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        name         TEXT NOT NULL,
        spec         TEXT NOT NULL,
        target       TEXT NOT NULL,
        created_by   TEXT NOT NULL REFERENCES members(id) ON DELETE CASCADE,
        enabled      INTEGER NOT NULL,
        missed       TEXT NOT NULL,
        overlap      TEXT NOT NULL,
        next_run_at  INTEGER,
        last_run_at  INTEGER,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        UNIQUE (space_id, name)
    );
    CREATE INDEX schedules_due ON schedules(next_run_at) WHERE enabled = 1;

    CREATE TABLE schedule_runs (
        id             TEXT PRIMARY KEY,
        schedule_id    TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
        scheduled_for  INTEGER NOT NULL,
        fired_at       INTEGER NOT NULL,
        outcome        TEXT NOT NULL,
        session_id     TEXT,
        manual         INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX schedule_runs_schedule ON schedule_runs(schedule_id, fired_at DESC);
    "#,
    // v6 (W4-1): goals and session-to-session dispatches.
    r#"
    CREATE TABLE goals (
        id            TEXT PRIMARY KEY,
        space_id      TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        objective     TEXT NOT NULL,
        done_marker   TEXT NOT NULL,
        max_turns     INTEGER NOT NULL,
        turns_used    INTEGER NOT NULL DEFAULT 0,
        deadline_at   INTEGER,
        created_by    TEXT NOT NULL REFERENCES members(id) ON DELETE CASCADE,
        status        TEXT NOT NULL,
        ended_reason  TEXT,
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL
    );
    CREATE INDEX goals_session ON goals(session_id, created_at DESC);
    CREATE UNIQUE INDEX goals_one_active ON goals(session_id) WHERE status = 'active';

    CREATE TABLE dispatches (
        id               TEXT PRIMARY KEY,
        space_id         TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
        from_session     TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        to_session       TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        member_id        TEXT NOT NULL,
        depth            INTEGER NOT NULL,
        created_session  INTEGER NOT NULL,
        submit           TEXT NOT NULL,
        created_at       INTEGER NOT NULL
    );
    CREATE INDEX dispatches_from ON dispatches(from_session, created_at DESC);
    CREATE INDEX dispatches_to ON dispatches(to_session, created_at DESC);
    "#,
];
