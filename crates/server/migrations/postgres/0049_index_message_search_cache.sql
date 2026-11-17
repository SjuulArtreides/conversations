-- no-transaction
-- Build after the copy, without blocking message writes. Deliberately no IF NOT
-- EXISTS: an interrupted invalid index must fail visibly, never appear complete.
CREATE INDEX CONCURRENTLY idx_message_search_cache_trgm
ON message_search_cache USING GIN (search_key gin_trgm_ops);
