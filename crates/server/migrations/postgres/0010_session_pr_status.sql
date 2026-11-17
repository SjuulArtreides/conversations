-- v10 (session PR status cache): cache the GitHub PR status the per-user CLI
-- pushes up (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0011_session_pr_status.sql,
-- differing only in dialect/types: BOOLEAN for the flags, TIMESTAMPTZ for the
-- fetched-at timestamp.
--
-- The server holds NO GitHub creds and never fetches status itself; it just
-- stores the last snapshot the CLI pushed and serves it back. All columns are
-- nullable: a freshly-linked PR has no cached status until the CLI pushes one.

ALTER TABLE session_prs ADD COLUMN IF NOT EXISTS status_state TEXT;
ALTER TABLE session_prs ADD COLUMN IF NOT EXISTS status_merged BOOLEAN;
ALTER TABLE session_prs ADD COLUMN IF NOT EXISTS status_draft BOOLEAN;
ALTER TABLE session_prs ADD COLUMN IF NOT EXISTS status_title TEXT;
ALTER TABLE session_prs ADD COLUMN IF NOT EXISTS status_fetched_at TIMESTAMPTZ;
