-- v22: first-class source agent support.

ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude';
ALTER TABLE live_sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude';

CREATE TABLE IF NOT EXISTS user_settings (
    user_email    TEXT PRIMARY KEY NOT NULL REFERENCES users (email) ON DELETE CASCADE,
    default_agent TEXT NOT NULL DEFAULT 'claude'
);
