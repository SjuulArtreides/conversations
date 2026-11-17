-- v8: explicit, user-named projects grouping sessions, plus saved chat threads
-- (session- or project-scoped) with their messages (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0006_projects_chats.sql, differing
-- only in dialect/types: TIMESTAMPTZ for timestamps, BOOLEAN for flags.

CREATE TABLE IF NOT EXISTS projects (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    owner_email TEXT,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);

-- A session belongs to at most one project. Deleting the project unassigns its
-- sessions (they survive) via ON DELETE SET NULL.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS project_id TEXT
    REFERENCES projects (id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_sessions_project_id ON sessions (project_id);

-- A chat thread attaches to EXACTLY ONE parent: a session OR a project.
-- The CHECK enforces the polymorphic-parent invariant. Deleting either parent
-- cascades the thread (and its messages) away.
CREATE TABLE IF NOT EXISTS chats (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT REFERENCES sessions (id) ON DELETE CASCADE,
    project_id  TEXT REFERENCES projects (id) ON DELETE CASCADE,
    owner_email TEXT,
    title       TEXT,
    shared      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    CHECK ((session_id IS NULL) <> (project_id IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_chats_session_id ON chats (session_id);
CREATE INDEX IF NOT EXISTS idx_chats_project_id ON chats (project_id);

CREATE TABLE IF NOT EXISTS chat_messages (
    id         TEXT PRIMARY KEY NOT NULL,
    chat_id    TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chat_messages_chat_id ON chat_messages (chat_id);
