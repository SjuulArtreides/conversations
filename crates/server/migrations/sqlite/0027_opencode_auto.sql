-- v27: per-user OpenCode automatic permission approval for live sessions.
-- Disabled by default; enabled sessions launch OpenCode with `--auto`.

ALTER TABLE user_settings ADD COLUMN opencode_auto INTEGER NOT NULL DEFAULT 0;
