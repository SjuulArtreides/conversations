-- Older live sessions can predate eager creation of their session metadata row.
INSERT INTO sessions(id,agent,title,message_count,skipped_lines,uploaded_by,uploaded_at,owner_email,visibility)
 SELECT id,agent,COALESCE(title,'Live session'),0,0,COALESCE(owner_email,''),started_at,owner_email,'restricted'
 FROM live_sessions WHERE id NOT IN (SELECT id FROM sessions);
ALTER TABLE live_session_shares ADD COLUMN communication_only BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE live_session_shares DROP CONSTRAINT live_session_shares_live_id_fkey;
ALTER TABLE live_session_shares ADD FOREIGN KEY (live_id) REFERENCES sessions(id) ON DELETE CASCADE;
ALTER TABLE agent_messages ALTER COLUMN project_id DROP NOT NULL;
