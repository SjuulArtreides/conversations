-- v13 (PR auto-detect): mark auto-detected PR links and stamp the one-time
-- backfill scan (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0014_pr_autodetect.sql, differing
-- only in dialect/types: BOOLEAN for the `detected` flag and TIMESTAMPTZ for
-- the scan stamp.
--
-- `session_prs.detected` distinguishes a PR that was auto-linked from
-- conversation text (true) from one a user linked manually (false, the
-- default). Auto linking inserts with `detected = true` via INSERT … ON
-- CONFLICT DO NOTHING, so a manual link (false) is never clobbered or
-- downgraded.
--
-- `sessions.pr_scanned_at` records when the one-time backfill scan over a
-- session's stored messages last ran (NULL = never scanned).

ALTER TABLE session_prs ADD COLUMN IF NOT EXISTS detected BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS pr_scanned_at TIMESTAMPTZ;
