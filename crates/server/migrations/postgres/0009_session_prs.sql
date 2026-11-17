-- v9 (session PRs): link GitHub pull requests to a session
-- (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0010_session_prs.sql,
-- differing only in dialect/types: TIMESTAMPTZ for timestamps, BIGINT for
-- the PR number.
--
-- Each row represents a PR linked to a session. The primary key is
-- (session_id, owner, repo, number) so the same PR can only be linked once
-- per session but many PRs from many repos can be linked to one session.

CREATE TABLE IF NOT EXISTS session_prs (
    session_id  TEXT    NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    url         TEXT    NOT NULL,
    owner       TEXT    NOT NULL,
    repo        TEXT    NOT NULL,
    number      BIGINT  NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, owner, repo, number)
);

CREATE INDEX IF NOT EXISTS idx_session_prs_session_id ON session_prs (session_id);
