-- Initial schema for conversations-server.
-- Timestamps are stored as TEXT in RFC3339 (UTC) form. NULL when absent.

CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY NOT NULL,
    title           TEXT NOT NULL,
    project_path    TEXT,
    cwd             TEXT,
    git_branch      TEXT,
    claude_version  TEXT,
    started_at      TEXT,
    ended_at        TEXT,
    message_count   INTEGER NOT NULL DEFAULT 0,
    skipped_lines   INTEGER NOT NULL DEFAULT 0,
    uploaded_by     TEXT NOT NULL,
    uploaded_at     TEXT NOT NULL,
    source_host     TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_started_at ON sessions (started_at);

CREATE TABLE IF NOT EXISTS messages (
    uuid          TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    parent_uuid   TEXT,
    role          TEXT NOT NULL,
    kind          TEXT NOT NULL,
    model         TEXT,
    is_sidechain  INTEGER NOT NULL DEFAULT 0,
    timestamp     TEXT,
    text          TEXT NOT NULL,
    raw           TEXT NOT NULL,
    usage         TEXT,
    ord           INTEGER NOT NULL,
    PRIMARY KEY (session_id, uuid),
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_messages_session_ord ON messages (session_id, ord);
