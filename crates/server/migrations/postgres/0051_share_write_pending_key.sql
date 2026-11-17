-- Pending key trust for a shared reader (plans/simpler-sharing/spec.md EX-02):
-- the publishing host named this person as intended to type into the session
-- through it once their key is trusted there. Until then they read the
-- published copy like any other audience member and cannot write.
ALTER TABLE session_shares ADD COLUMN write_pending_key BOOLEAN NOT NULL DEFAULT FALSE;
