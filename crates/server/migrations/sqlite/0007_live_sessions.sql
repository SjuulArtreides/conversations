-- v9: live sessions — a session being streamed to the web UI as it happens, plus
-- the durable per-session message log that is the replay + graduation source of
-- truth (the in-memory broadcast hub is only the fast path).
--
-- Timestamps are RFC3339 TEXT (UTC), matching the SQLite convention elsewhere.
-- FK enforcement relies on `PRAGMA foreign_keys = ON`, set by the SqliteRepo
-- migrate path.
--
-- status ∈ live | ended | graduated | abandoned ; mode ∈ view | interactive.
-- On graduation a `core::Session` is assembled from `live_messages` and upserted
-- into `sessions`; `graduated_session_id` then points at that row (SET NULL if it
-- is later deleted).

CREATE TABLE IF NOT EXISTS live_sessions (
    id                   TEXT PRIMARY KEY NOT NULL,
    session_id           TEXT NOT NULL,
    title                TEXT,
    project_path         TEXT,
    cwd                  TEXT,
    git_branch           TEXT,
    owner_email          TEXT,
    source_host          TEXT,
    status               TEXT NOT NULL DEFAULT 'live',
    mode                 TEXT NOT NULL DEFAULT 'view',
    started_at           TEXT NOT NULL,
    last_event_at        TEXT NOT NULL,
    ended_at             TEXT,
    graduated_session_id TEXT REFERENCES sessions (id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_live_sessions_status ON live_sessions (status);

CREATE TABLE IF NOT EXISTS live_messages (
    id           TEXT PRIMARY KEY NOT NULL,
    live_id      TEXT NOT NULL REFERENCES live_sessions (id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    message_uuid TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    UNIQUE (live_id, message_uuid)
);

CREATE INDEX IF NOT EXISTS idx_live_messages_live_seq ON live_messages (live_id, seq);
