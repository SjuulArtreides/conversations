-- Communication becomes independent of transcript access. communication_only
-- keeps its stored meaning (no transcript access), so read checks are unchanged.
-- Back-fill preserves existing communication: read-write and communication-only
-- shares already communicated; read-only shares did not.
ALTER TABLE live_session_shares ADD COLUMN can_communicate BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE live_session_shares SET can_communicate = TRUE WHERE can_write OR communication_only;
