-- v13 (session PR comments cache): cache the GitHub PR comments the per-user
-- CLI fetches (via `gh`) and pushes up. The server holds NO GitHub creds and
-- never fetches comments itself; it just stores the last snapshot the CLI
-- pushed and serves them back.
--
-- One row per comment. The primary key (session_id, owner, repo, number,
-- comment_id) lets the CLI re-push the same comment idempotently (an upsert
-- refreshes the mutable fields and re-stamps ingested_at).
--
-- `kind` is a plain string: issue | review | review_comment. `created_at` is
-- the RFC3339 timestamp gh reports for the comment; `ingested_at` is the
-- server's own stamp at upsert time. Timestamps are RFC3339 TEXT (UTC),
-- matching the SQLite convention elsewhere (see 0011_session_pr_status.sql).

CREATE TABLE IF NOT EXISTS session_pr_comments (
    session_id  TEXT    NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    owner       TEXT    NOT NULL,
    repo        TEXT    NOT NULL,
    number      INTEGER NOT NULL,
    comment_id  TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    author      TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    html_url    TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,
    ingested_at TEXT    NOT NULL,
    PRIMARY KEY (session_id, owner, repo, number, comment_id)
);

CREATE INDEX IF NOT EXISTS idx_session_pr_comments_session_id
    ON session_pr_comments (session_id);
