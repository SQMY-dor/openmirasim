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
];
