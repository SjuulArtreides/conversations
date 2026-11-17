-- v9 (live shares): per-user read / read+write shares for a live session
-- (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0009_live_session_shares.sql,
-- differing only in dialect/types: TIMESTAMPTZ for timestamps, BOOLEAN for the
-- can_write flag.
--
-- A live session is private to its author (`live_sessions.owner_email`) and may
-- be shared with other users at one of two levels:
--   * read       — `can_write = false`: may watch the live stream.
--   * read+write  — `can_write = true`: may watch AND reply.
-- The author always has full read+write and is the only one who may manage shares.

CREATE TABLE IF NOT EXISTS live_session_shares (
    live_id     TEXT NOT NULL REFERENCES live_sessions (id) ON DELETE CASCADE,
    email       TEXT NOT NULL,
    can_write   BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (live_id, email)
);

CREATE INDEX IF NOT EXISTS idx_live_session_shares_email ON live_session_shares (email);
