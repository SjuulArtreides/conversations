-- no-transaction
-- Only retire the old access path after the replacement was built successfully.
-- IF EXISTS makes interruption after the drop but before ledger insertion safe.
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_text_trgm;
