-- v14 (container paths): per-user "container" repo paths (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0015_user_container_paths.sql.
-- Both columns are TEXT in both backends, so this differs from the SQLite
-- migration only in the file number.
--
-- A user can mark one or more absolute filesystem paths as monorepo "containers".
-- A repo link whose stored per-user `local_path` equals one of these paths (after
-- trailing-slash normalization) is treated as a container: excluded from
-- PR-context resolution and flagged for the UI to hide/de-emphasize. The repo
-- link itself is never deleted — this is a display + resolution filter only.

CREATE TABLE IF NOT EXISTS user_container_paths (
    user_email TEXT NOT NULL,
    path       TEXT NOT NULL,
    PRIMARY KEY (user_email, path),
    FOREIGN KEY (user_email) REFERENCES users (email) ON DELETE CASCADE
);
