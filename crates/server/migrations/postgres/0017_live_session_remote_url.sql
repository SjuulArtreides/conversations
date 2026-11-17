-- v17 (live session repo context): record the launch repo's origin remote on
-- the live_sessions row. A live session has no session_repos rows (that table's
-- FK still requires a sessions row, which an ungraduated live session lacks), so
-- the remote URL is carried here. The server parses it (github::parse_repo_remote)
-- to resolve owner/repo for PR-shorthand auto-linking during the live session.
ALTER TABLE live_sessions ADD COLUMN remote_url TEXT;
