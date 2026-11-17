-- Older live sessions can predate eager creation of their session metadata row.
INSERT INTO sessions(id,agent,title,message_count,skipped_lines,uploaded_by,uploaded_at,owner_email,visibility)
 SELECT id,agent,COALESCE(title,'Live session'),0,0,COALESCE(owner_email,''),started_at,owner_email,'restricted'
 FROM live_sessions WHERE id NOT IN (SELECT id FROM sessions);
CREATE TABLE communication_session_shares (
 live_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
 email TEXT NOT NULL, can_write INTEGER NOT NULL DEFAULT 0,
 communication_only INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL,
 PRIMARY KEY(live_id,email));
INSERT INTO communication_session_shares(live_id,email,can_write,created_at)
 SELECT live_id,email,can_write,created_at FROM live_session_shares;
DROP TABLE live_session_shares;
ALTER TABLE communication_session_shares RENAME TO live_session_shares;
CREATE INDEX idx_live_session_shares_email ON live_session_shares(email);
CREATE TABLE communication_agent_messages (
 id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
 sender_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
 sender_owner_email TEXT, recipient_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
 text TEXT NOT NULL, reply_to TEXT REFERENCES communication_agent_messages(id) ON DELETE SET NULL,
 idempotency_key TEXT NOT NULL, created_at TEXT NOT NULL, received_at TEXT, acknowledged_at TEXT,
 UNIQUE(sender_session_id,idempotency_key));
INSERT INTO communication_agent_messages SELECT * FROM agent_messages;
DROP TABLE agent_messages;
ALTER TABLE communication_agent_messages RENAME TO agent_messages;
CREATE INDEX agent_messages_inbox ON agent_messages(recipient_session_id,project_id,acknowledged_at,created_at,id);
