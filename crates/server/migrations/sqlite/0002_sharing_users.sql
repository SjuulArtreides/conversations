-- v2: sharing model + users directory (SQLite dialect).
-- Timestamps are stored as TEXT in RFC3339 (UTC) form, matching 0001.

-- Sharing columns on sessions.
ALTER TABLE sessions ADD COLUMN owner_email TEXT;
ALTER TABLE sessions ADD COLUMN visibility TEXT NOT NULL DEFAULT 'organization';

-- Per-session share list (only meaningful when visibility = 'restricted').
CREATE TABLE IF NOT EXISTS session_shares (
    session_id  TEXT NOT NULL,
    email       TEXT NOT NULL,
    PRIMARY KEY (session_id, email),
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_shares_email ON session_shares (email);

-- Discoverable users directory.
CREATE TABLE IF NOT EXISTS users (
    email         TEXT PRIMARY KEY NOT NULL,
    display_name  TEXT,
    created_at    TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL
);
