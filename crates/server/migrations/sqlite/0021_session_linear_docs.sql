-- v21 (Linear refs): link Linear documents / issues / projects to a session.
--
-- The Linear analogue of `session_prs`, but a strict subset: the server holds
-- no Linear credentials, so there is no status cache and no CLI sync — we store
-- only the canonical URL plus parsed display fields and serve them back.
--
-- Each row is one Linear entity referenced by a session. The primary key is
-- (session_id, url) — the canonical `https://linear.app/{ws}/{kind}/{id}` URL
-- dedupes the same entity referenced with and without a trailing title slug.
-- Like `session_prs` (after migration 0016) there is intentionally NO foreign
-- key to sessions(id), so a ref can be linked to a LIVE session that has no
-- `sessions` row yet; `delete_session` clears these rows explicitly.
--
-- `kind` is one of 'document' | 'issue' | 'project'. `identifier` is the
-- human-readable handle: `TEAM-123` for an issue, the `{slug-id}` segment for a
-- document/project. Timestamps are RFC3339 TEXT (UTC), matching convention.
--
-- `sessions.linear_scanned_at` records when the one-time Linear backfill scan
-- over a session's stored messages last ran (NULL = never). It is deliberately
-- SEPARATE from `pr_scanned_at`: sessions already PR-scanned before this feature
-- shipped must still get a Linear backfill on their first linear-docs GET.

CREATE TABLE IF NOT EXISTS session_linear_docs (
    session_id  TEXT NOT NULL,
    url         TEXT NOT NULL,
    workspace   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    identifier  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (session_id, url)
);

CREATE INDEX IF NOT EXISTS idx_session_linear_docs_session_id
    ON session_linear_docs (session_id);

ALTER TABLE sessions ADD COLUMN linear_scanned_at TEXT;
