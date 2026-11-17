CREATE TABLE agent_handoffs (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    source_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    agent TEXT NOT NULL,
    brief TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('requested', 'starting', 'started', 'failed')),
    error TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(source_session_id, idempotency_key)
);
