-- v26: per-user OpenCode automatic permission approval for live sessions.
-- Kept in MEANING-sync with migrations/sqlite/0027_opencode_auto.sql.
-- Disabled by default; enabled sessions launch OpenCode with `--auto`.

ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS opencode_auto BOOLEAN NOT NULL DEFAULT FALSE;
