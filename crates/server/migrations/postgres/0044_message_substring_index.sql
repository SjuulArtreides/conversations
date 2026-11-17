-- no-transaction
-- Keep this a single statement: concurrent index creation cannot run in a transaction.
-- Do not use IF NOT EXISTS: an interrupted build may leave an invalid index.
-- If this migration fails with an existing index, inspect it, then run
-- DROP INDEX CONCURRENTLY idx_messages_text_trgm; and restart the migration.
CREATE INDEX CONCURRENTLY idx_messages_text_trgm ON messages USING GIN (LOWER(text) gin_trgm_ops);
