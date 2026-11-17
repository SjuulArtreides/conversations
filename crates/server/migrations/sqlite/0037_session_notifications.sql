CREATE TABLE session_notifications (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('share', 'attention')),
    sender_email TEXT NOT NULL,
    recipient_email TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL,
    read_at TEXT,
    resolved_at TEXT
);
CREATE INDEX session_notifications_recipient ON session_notifications(recipient_email, created_at);
