-- v16 (relax session_prs FK): drop the session_prs -> sessions(id) foreign key
-- so a PR can be linked to a LIVE session, which has no `sessions` row yet (one
-- is created only on graduation). A live session's id IS its eventual
-- sessions.id (id == session_id == graduated sessions.id), so a row linked under
-- the live id is already valid after graduation — no merge needed.
--
-- SQLite cannot drop a column constraint in place, so rebuild the table. The
-- ON DELETE CASCADE the FK provided is replaced by an explicit
-- `DELETE FROM session_prs` in SqliteRepo::delete_session.
--
-- Preserves every column added through 0014 (status cache + `detected`), the
-- composite PK, and the session_id index. Meaning-sync with
-- migrations/postgres/0015_session_prs_drop_session_fk.sql (which only needs a
-- DROP CONSTRAINT).

CREATE TABLE session_prs_new (
    session_id        TEXT    NOT NULL,
    url               TEXT    NOT NULL,
    owner             TEXT    NOT NULL,
    repo              TEXT    NOT NULL,
    number            INTEGER NOT NULL,
    created_at        TEXT    NOT NULL,
    status_state      TEXT,
    status_merged     INTEGER,
    status_draft      INTEGER,
    status_title      TEXT,
    status_fetched_at TEXT,
    detected          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, owner, repo, number)
);

INSERT INTO session_prs_new
    (session_id, url, owner, repo, number, created_at,
     status_state, status_merged, status_draft, status_title, status_fetched_at, detected)
SELECT
    session_id, url, owner, repo, number, created_at,
    status_state, status_merged, status_draft, status_title, status_fetched_at, detected
FROM session_prs;

DROP TABLE session_prs;

ALTER TABLE session_prs_new RENAME TO session_prs;

CREATE INDEX IF NOT EXISTS idx_session_prs_session_id ON session_prs (session_id);
