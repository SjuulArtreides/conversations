CREATE TABLE agent_mailboxes (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    owner_email TEXT,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    last_seen_at TEXT NOT NULL
);
CREATE INDEX agent_mailboxes_project ON agent_mailboxes(project_id, enabled);

CREATE TABLE agent_messages (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    sender_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    sender_owner_email TEXT,
    recipient_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    reply_to TEXT REFERENCES agent_messages(id) ON DELETE SET NULL,
    idempotency_key TEXT NOT NULL,
    created_at TEXT NOT NULL,
    received_at TEXT,
    acknowledged_at TEXT,
    UNIQUE(sender_session_id, idempotency_key)
);
CREATE INDEX agent_messages_inbox ON agent_messages(recipient_session_id, project_id, acknowledged_at, created_at, id);
