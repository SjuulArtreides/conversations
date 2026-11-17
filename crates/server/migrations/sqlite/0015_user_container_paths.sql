-- v15 (container paths): per-user "container" repo paths.
--
-- A user can mark one or more absolute filesystem paths as monorepo "containers".
-- A repo link whose stored per-user `local_path` equals one of these paths (after
-- trailing-slash normalization) is treated as a container: excluded from
-- PR-context resolution and flagged for the UI to hide/de-emphasize. The repo
-- link itself is never deleted — this is a display + resolution filter only.
--
-- Paths are stored normalized (trimmed, single trailing `/` removed, blanks
-- dropped, deduped) by the store layer. Matching the SQLite convention, FK
-- enforcement relies on `PRAGMA foreign_keys = ON` (set by the SqliteRepo migrate
-- path); see users in 0002_sharing_users.sql.

CREATE TABLE IF NOT EXISTS user_container_paths (
    user_email TEXT NOT NULL,
    path       TEXT NOT NULL,
    PRIMARY KEY (user_email, path),
    FOREIGN KEY (user_email) REFERENCES users (email) ON DELETE CASCADE
);
