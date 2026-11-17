-- v17 (inline review-comment context): inline PR review comments (kind
-- `review_comment`) now carry the file/line they hang on, the diff snippet
-- GitHub shows above them, and their thread's resolution state — fetched by the
-- per-user CLI via the GitHub GraphQL `reviewThreads` API and pushed up. This
-- lets the UI surface the OPEN (unresolved) review comments with code context,
-- like GitHub does.
--
-- All four columns are NULLable: they are populated only for `review_comment`
-- rows, and remain NULL for issue / review kinds and for rows that predate this
-- migration (an old CLI that never pushed them). `resolved` is stored as the
-- usual SQLite boolean-as-INTEGER (0/1, NULL ⇒ unknown).
--
-- SQLite allows only one ADD COLUMN per ALTER TABLE, hence four statements.

ALTER TABLE session_pr_comments ADD COLUMN path TEXT;
ALTER TABLE session_pr_comments ADD COLUMN line INTEGER;
ALTER TABLE session_pr_comments ADD COLUMN diff_hunk TEXT;
ALTER TABLE session_pr_comments ADD COLUMN resolved INTEGER;
