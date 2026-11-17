-- Initial schema for conversations-server (Postgres dialect).
--
-- Kept in MEANING-sync with the SQLite migrations under migrations/sqlite/
-- (0001_init.sql + 0002_sharing_users.sql), differing only in dialect/types:
-- TIMESTAMPTZ for timestamps, JSONB for raw/usage blobs, BOOLEAN for flags.

CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY NOT NULL,
    title           TEXT NOT NULL,
    project_path    TEXT,
    cwd             TEXT,
    git_branch      TEXT,
    claude_version  TEXT,
    started_at      TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ,
    message_count   BIGINT NOT NULL DEFAULT 0,
    skipped_lines   BIGINT NOT NULL DEFAULT 0,
    uploaded_by     TEXT NOT NULL,
    uploaded_at     TIMESTAMPTZ NOT NULL,
    source_host     TEXT,
    owner_email     TEXT,
    visibility      TEXT NOT NULL DEFAULT 'organization'
);

CREATE INDEX IF NOT EXISTS idx_sessions_started_at ON sessions (started_at);

CREATE TABLE IF NOT EXISTS messages (
    uuid          TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    parent_uuid   TEXT,
    role          TEXT NOT NULL,
    kind          TEXT NOT NULL,
    model         TEXT,
    is_sidechain  BOOLEAN NOT NULL DEFAULT FALSE,
    timestamp     TIMESTAMPTZ,
    text          TEXT NOT NULL,
    raw           JSONB NOT NULL,
    usage         JSONB,
    ord           BIGINT NOT NULL,
    PRIMARY KEY (session_id, uuid),
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_messages_session_ord ON messages (session_id, ord);

-- Per-session share list (only meaningful when visibility = 'restricted').
CREATE TABLE IF NOT EXISTS session_shares (
    session_id  TEXT NOT NULL,
    email       TEXT NOT NULL,
    PRIMARY KEY (session_id, email),
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_shares_email ON session_shares (email);

-- Discoverable users directory.
CREATE TABLE IF NOT EXISTS users (
    email         TEXT PRIMARY KEY NOT NULL,
    display_name  TEXT,
    created_at    TIMESTAMPTZ NOT NULL,
    last_seen_at  TIMESTAMPTZ NOT NULL
);
