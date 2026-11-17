-- A user-set title override, stored in the DB only. The on-disk JSONL
-- transcript (and therefore the parsed `title`) is never modified; reads
-- return COALESCE(title_override, title) so the override is transparent and
-- survives re-uploads (upsert_session never touches this column).
ALTER TABLE sessions ADD COLUMN title_override TEXT;
