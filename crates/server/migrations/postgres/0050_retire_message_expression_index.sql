-- no-transaction
-- SQLx reaches this migration only after the replacement index is valid.
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_search_key_trgm;
