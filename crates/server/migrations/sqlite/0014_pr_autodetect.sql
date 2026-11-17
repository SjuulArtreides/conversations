-- v14 (PR auto-detect): mark auto-detected PR links and stamp the one-time
-- backfill scan.
--
-- `session_prs.detected` distinguishes a PR that was auto-linked from
-- conversation text (1) from one a user linked manually (0, the default). Auto
-- linking inserts with `detected = 1` via INSERT … ON CONFLICT DO NOTHING, so a
-- manual link (detected = 0) is never clobbered or downgraded.
--
-- `sessions.pr_scanned_at` records when the one-time backfill scan over a
-- session's stored messages last ran (NULL = never scanned). The PR-list GET
-- scans when it is NULL, then stamps it, so backfill runs at most once per
-- session. RFC3339 TEXT (UTC), matching the SQLite convention elsewhere.

ALTER TABLE session_prs ADD COLUMN detected INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN pr_scanned_at TEXT;
