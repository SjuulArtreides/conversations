-- v15 (relax session_prs FK): drop the session_prs -> sessions(id) foreign key
-- so a PR can be linked to a LIVE session (Postgres dialect).
--
-- Meaning-sync with migrations/sqlite/0016_session_prs_drop_session_fk.sql. A
-- live session has no `sessions` row until graduation, but its id IS the
-- eventual sessions.id, so a row linked under the live id is already valid once
-- the session graduates — no merge needed. The ON DELETE CASCADE the FK
-- provided is replaced by an explicit `DELETE FROM session_prs` in
-- PostgresRepo::delete_session.
--
-- Postgres can drop the constraint in place (no table rebuild). The default
-- constraint name for `session_id ... REFERENCES sessions (id)` is
-- `session_prs_session_id_fkey`; IF EXISTS keeps this idempotent.

ALTER TABLE session_prs DROP CONSTRAINT IF EXISTS session_prs_session_id_fkey;
