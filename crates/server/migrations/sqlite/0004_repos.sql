-- v6: repo/branch/PR metadata per conversation, with per-user local paths.
-- A conversation can span multiple repos; each (repo, branch) is unique per
-- session. Per-user local checkout paths hang off the natural key so they
-- survive idempotent re-uploads.

CREATE TABLE IF NOT EXISTS session_repos (
    session_id  TEXT NOT NULL,
    repo        TEXT NOT NULL,
    branch      TEXT NOT NULL,
    remote_url  TEXT,
    pr_url      TEXT,
    PRIMARY KEY (session_id, repo, branch),
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_repos_session ON session_repos (session_id);

CREATE TABLE IF NOT EXISTS session_repo_paths (
    session_id  TEXT NOT NULL,
    repo        TEXT NOT NULL,
    branch      TEXT NOT NULL,
    user_email  TEXT NOT NULL,
    local_path  TEXT NOT NULL,
    PRIMARY KEY (session_id, repo, branch, user_email),
    FOREIGN KEY (session_id, repo, branch)
        REFERENCES session_repos (session_id, repo, branch) ON DELETE CASCADE
);
