-- v21: first-class source agent support.
-- Kept in MEANING-sync with migrations/sqlite/0022_agent_support.sql.

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS agent TEXT NOT NULL DEFAULT 'claude';
ALTER TABLE live_sessions ADD COLUMN IF NOT EXISTS agent TEXT NOT NULL DEFAULT 'claude';

CREATE TABLE IF NOT EXISTS user_settings (
    user_email    TEXT PRIMARY KEY NOT NULL REFERENCES users (email) ON DELETE CASCADE,
    default_agent TEXT NOT NULL DEFAULT 'claude'
);
