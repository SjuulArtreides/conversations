-- v11 (session PR status cache): cache the GitHub PR status the per-user CLI
-- pushes up. The server holds NO GitHub creds and never fetches status itself;
-- it just stores the last snapshot the CLI pushed and serves it back.
--
-- All columns are nullable: a freshly-linked PR has no cached status until the
-- CLI pushes one (the API surfaces `status: null` until then).
--
-- Booleans are INTEGER 0/1 and timestamps are RFC3339 TEXT (UTC), matching the
-- SQLite convention elsewhere (see 0010_session_prs.sql).

ALTER TABLE session_prs ADD COLUMN status_state TEXT;
ALTER TABLE session_prs ADD COLUMN status_merged INTEGER;
ALTER TABLE session_prs ADD COLUMN status_draft INTEGER;
ALTER TABLE session_prs ADD COLUMN status_title TEXT;
ALTER TABLE session_prs ADD COLUMN status_fetched_at TEXT;
