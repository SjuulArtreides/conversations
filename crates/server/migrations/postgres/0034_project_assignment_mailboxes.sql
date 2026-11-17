-- Policy: assigning a session to a project enables its messaging participation.
-- Backfill a mailbox for every session already assigned to a project that does
-- not have one yet. Existing mailbox rows (including explicit opt-outs) are
-- left untouched.
INSERT INTO agent_mailboxes (session_id, project_id, owner_email, enabled, last_seen_at)
SELECT id, project_id, owner_email, 1, to_char(timezone('UTC', now()), 'YYYY-MM-DD"T"HH24:MI:SS.MSZ')
FROM sessions
WHERE project_id IS NOT NULL
ON CONFLICT(session_id) DO NOTHING;
