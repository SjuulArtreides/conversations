-- v20 (Linear refs): link Linear documents / issues / projects to a session
-- (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0021_session_linear_docs.sql,
-- differing only in dialect/types: TIMESTAMPTZ for timestamps.
--
-- The Linear analogue of `session_prs`, but a strict subset: the server holds
-- no Linear credentials, so there is no status cache and no CLI sync — we store
-- only the canonical URL plus parsed display fields and serve them back. The
-- primary key is (session_id, url); the canonical
-- `https://linear.app/{ws}/{kind}/{id}` URL dedupes the same entity referenced
-- with and without a trailing title slug. Like `session_prs` (after migration
-- 0015) there is intentionally NO foreign key to sessions(id) so a ref can be
-- linked to a LIVE session with no `sessions` row yet; `delete_session` clears
-- these rows explicitly.
--
-- `kind` is one of 'document' | 'issue' | 'project'. `identifier` is the
-- human-readable handle (`TEAM-123` for an issue, the `{slug-id}` segment for a
-- document/project).
--
-- `sessions.linear_scanned_at` records when the one-time Linear backfill scan
-- last ran (NULL = never). Deliberately SEPARATE from `pr_scanned_at` so
-- sessions already PR-scanned before this feature shipped still get a Linear
-- backfill on their first linear-docs GET.

CREATE TABLE IF NOT EXISTS session_linear_docs (
    session_id  TEXT NOT NULL,
    url         TEXT NOT NULL,
    workspace   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    identifier  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, url)
);

CREATE INDEX IF NOT EXISTS idx_session_linear_docs_session_id
    ON session_linear_docs (session_id);

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS linear_scanned_at TIMESTAMPTZ;
