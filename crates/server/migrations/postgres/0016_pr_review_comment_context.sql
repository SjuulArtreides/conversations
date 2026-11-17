-- v16 (inline review-comment context, Postgres dialect): kept in MEANING-sync
-- with migrations/sqlite/0017_pr_review_comment_context.sql, differing only in
-- types — BIGINT for the line number and a real BOOLEAN for `resolved`.
--
-- Inline PR review comments (kind `review_comment`) now carry the file/line
-- they hang on, the diff snippet shown above them, and their thread's
-- resolution state, fetched by the per-user CLI via the GitHub GraphQL
-- `reviewThreads` API and pushed up. All four columns are NULLable: populated
-- only for `review_comment` rows, NULL for issue / review kinds and for rows
-- predating this migration.

ALTER TABLE session_pr_comments
    ADD COLUMN path      TEXT,
    ADD COLUMN line      BIGINT,
    ADD COLUMN diff_hunk TEXT,
    ADD COLUMN resolved  BOOLEAN;
