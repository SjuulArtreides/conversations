-- Per-user opt-in for Claude permission bypass; disabled by default.
ALTER TABLE user_settings ADD COLUMN claude_skip_permissions INTEGER NOT NULL DEFAULT 0;
