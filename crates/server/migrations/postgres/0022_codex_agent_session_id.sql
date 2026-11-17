-- v22: the agent's native session id, for resuming sessions started live.
--
-- The live `id` we register is not always a valid resume target. Claude is
-- launched with `--session-id <id>`, so `id` == Claude's session id and
-- `claude --resume <id>` works. Codex mints its OWN session id (it cannot be
-- told one at launch), so the registered `id` is a conversations-generated uuid
-- that `codex resume <id>` rejects. The CLI tailer reads Codex's real id from
-- the rollout (`session_meta.payload.id`) and pushes it here; `reopen` then
-- resumes by it. NULL for Claude / legacy / uploaded rows (resume falls back to
-- the row id, which is already correct there).

ALTER TABLE sessions ADD COLUMN agent_session_id TEXT;
ALTER TABLE live_sessions ADD COLUMN agent_session_id TEXT;
