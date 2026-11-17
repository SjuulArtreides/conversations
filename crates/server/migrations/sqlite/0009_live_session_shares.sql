-- v9 (live shares): per-user read / read+write shares for a live session.
--
-- Increment 3 replaces the global LIVE_REPLY_ENABLED flag with real per-session
-- authorization. A live session is private to its author (`live_sessions.owner_email`)
-- and may be shared with other users at one of two levels:
--   * read       — `can_write = 0`: may watch the live stream.
--   * read+write  — `can_write = 1`: may watch AND reply.
-- The author always has full read+write and is the only one who may manage shares.
--
-- Booleans are INTEGER 0/1 and timestamps are RFC3339 TEXT (UTC), matching the
-- SQLite convention elsewhere (see session_shares in 0002_sharing_users.sql).
-- FK enforcement relies on `PRAGMA foreign_keys = ON`, set by the SqliteRepo
-- migrate path.

CREATE TABLE IF NOT EXISTS live_session_shares (
    live_id     TEXT NOT NULL REFERENCES live_sessions (id) ON DELETE CASCADE,
    email       TEXT NOT NULL,
    can_write   INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (live_id, email)
);

CREATE INDEX IF NOT EXISTS idx_live_session_shares_email ON live_session_shares (email);
