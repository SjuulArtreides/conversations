-- v28: per-user `adk code` launcher settings for live OpenCode sessions.
-- Kept in MEANING-sync with migrations/sqlite/0029_adk_launcher.sql.
--
-- `adk_reopen` re-opens an OpenCode session through `adk code` instead of a bare
-- `opencode`, so a re-opened session keeps the monorepo harness it was started
-- with. `adk_statusline` lets an `adk code` child draw its tmux status strip;
-- off by default because that strip is what the web mirror would show instead of
-- the OpenCode TUI. Both disabled by default.

ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS adk_reopen BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS adk_statusline BOOLEAN NOT NULL DEFAULT FALSE;
