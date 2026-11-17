ALTER TABLE session_pr_comments ADD COLUMN revision BIGINT NOT NULL DEFAULT 0;

CREATE TABLE session_pr_comment_revisions (
    session_id TEXT PRIMARY KEY REFERENCES sessions (id) ON DELETE CASCADE,
    revision BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX idx_session_pr_comments_revision
    ON session_pr_comments (session_id, revision);

CREATE INDEX idx_session_pr_comments_preview
    ON session_pr_comments (session_id, owner, repo, number, created_at DESC);
