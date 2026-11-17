-- v12 (session PR comments cache): cache the GitHub PR comments the per-user
-- CLI fetches (via `gh`) and pushes up (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0013_session_pr_comments.sql,
-- differing only in dialect/types: BIGINT for the PR number and TIMESTAMPTZ for
-- the server-stamped `ingested_at` (gh's `created_at` stays TEXT — it is the
-- verbatim RFC3339 string the CLI pushes).
--
-- The server holds NO GitHub creds and never fetches comments itself; it just
-- stores the last snapshot the CLI pushed and serves them back. One row per
-- comment; the primary key lets the CLI re-push idempotently.

CREATE TABLE IF NOT EXISTS session_pr_comments (
    session_id  TEXT        NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    owner       TEXT        NOT NULL,
    repo        TEXT        NOT NULL,
    number      BIGINT      NOT NULL,
    comment_id  TEXT        NOT NULL,
    kind        TEXT        NOT NULL,
    author      TEXT        NOT NULL,
    body        TEXT        NOT NULL,
    html_url    TEXT        NOT NULL,
    created_at  TEXT        NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, owner, repo, number, comment_id)
);

CREATE INDEX IF NOT EXISTS idx_session_pr_comments_session_id
    ON session_pr_comments (session_id);
