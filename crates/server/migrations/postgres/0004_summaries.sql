-- v7: one LLM-generated summary per session (Postgres dialect).
-- 1:1 with sessions (session_id PK). `invariants` is a JSON array stored as
-- text. `generated_at` is TIMESTAMPTZ, matching the Postgres timestamp
-- convention used elsewhere in this schema.

CREATE TABLE IF NOT EXISTS session_summaries (
    session_id   TEXT PRIMARY KEY,
    intent       TEXT NOT NULL,
    invariants   TEXT NOT NULL,   -- JSON array stored as text
    summary      TEXT NOT NULL,
    model        TEXT NOT NULL,
    generated_by TEXT,
    generated_at TIMESTAMPTZ NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);
