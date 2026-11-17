-- v10 (session PRs): link GitHub pull requests to a session.
--
-- Each row represents a PR linked to a session. The primary key is
-- (session_id, owner, repo, number) so the same PR can only be linked once
-- per session but many PRs from many repos can be linked to one session.
--
-- Booleans are INTEGER 0/1 and timestamps are RFC3339 TEXT (UTC), matching the
-- SQLite convention elsewhere (see 0009_live_session_shares.sql).
-- FK enforcement relies on `PRAGMA foreign_keys = ON`, set by the SqliteRepo
-- migrate path.

CREATE TABLE IF NOT EXISTS session_prs (
    session_id  TEXT    NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    url         TEXT    NOT NULL,
    owner       TEXT    NOT NULL,
    repo        TEXT    NOT NULL,
    number      INTEGER NOT NULL,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (session_id, owner, repo, number)
);

CREATE INDEX IF NOT EXISTS idx_session_prs_session_id ON session_prs (session_id);
