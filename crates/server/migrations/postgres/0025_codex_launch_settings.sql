-- v25: per-user Codex launch overrides for live sessions.
-- Kept in MEANING-sync with migrations/sqlite/0026_codex_launch_settings.sql.
-- NULL means "do not pass an override; let the local Codex CLI config decide".
-- Values are validated by the API before storage.

ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS codex_sandbox_mode TEXT;
ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS codex_approval_policy TEXT;
