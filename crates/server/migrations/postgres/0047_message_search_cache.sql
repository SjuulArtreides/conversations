-- Keep expensive Unicode normalization out of lossy GIN heap rechecks.
-- A database trigger covers both current and older application writers atomically.
CREATE TABLE message_search_cache (
    session_id TEXT NOT NULL,
    uuid TEXT NOT NULL,
    lower_text TEXT NOT NULL,
    search_key TEXT NOT NULL,
    PRIMARY KEY (session_id, uuid),
    FOREIGN KEY (session_id, uuid) REFERENCES messages(session_id, uuid)
        ON UPDATE CASCADE ON DELETE CASCADE
);

CREATE FUNCTION maintain_message_search_cache() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE normalized TEXT := LOWER(NEW.text);
BEGIN
    INSERT INTO message_search_cache(session_id, uuid, lower_text, search_key)
    VALUES (NEW.session_id, NEW.uuid, normalized,
            TRANSLATE(normalized, ' :-/.', 'xqzvy'))
    ON CONFLICT (session_id, uuid) DO UPDATE
        SET lower_text = EXCLUDED.lower_text, search_key = EXCLUDED.search_key;
    RETURN NEW;
END $$;

CREATE TRIGGER messages_search_cache
AFTER INSERT OR UPDATE OF text ON messages
FOR EACH ROW EXECUTE FUNCTION maintain_message_search_cache();
