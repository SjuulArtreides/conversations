-- no-transaction
-- Keep punctuation adjacency that pg_trgm otherwise discards. The query retains
-- the original LIKE predicate because this one-character mapping has collisions.
-- On an interrupted build, inspect the index, DROP INDEX CONCURRENTLY
-- idx_messages_search_key_trgm, then restart; do not skip an invalid index.
CREATE INDEX CONCURRENTLY idx_messages_search_key_trgm ON messages USING GIN (TRANSLATE(LOWER(text), ' :-/.', 'xqzvy') gin_trgm_ops);
