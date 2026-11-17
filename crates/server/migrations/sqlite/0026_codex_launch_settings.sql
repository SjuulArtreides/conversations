-- v26: per-user Codex launch overrides for live sessions.
-- NULL means "do not pass an override; let the local Codex CLI config decide".
-- Values are validated by the API before storage.

ALTER TABLE user_settings ADD COLUMN codex_sandbox_mode TEXT;
ALTER TABLE user_settings ADD COLUMN codex_approval_policy TEXT;
