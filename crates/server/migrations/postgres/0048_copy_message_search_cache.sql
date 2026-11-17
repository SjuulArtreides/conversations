-- no-transaction
-- Each batch commits independently under SQLx's session advisory migration lock.
-- A failed start can repeat this migration: trigger-written rows always win.
DO $copy$
DECLARE
    last_session TEXT;
    last_uuid TEXT;
    next_session TEXT;
    next_uuid TEXT;
BEGIN
    LOOP
        WITH batch AS MATERIALIZED (
            SELECT m.session_id, m.uuid, LOWER(m.text) AS normalized
            FROM messages m
            WHERE (last_session IS NULL OR (m.session_id, m.uuid) > (last_session, last_uuid))
              AND NOT EXISTS (
                  SELECT 1 FROM message_search_cache c
                  WHERE c.session_id = m.session_id AND c.uuid = m.uuid
              )
            ORDER BY m.session_id, m.uuid
            LIMIT 256
            FOR SHARE OF m SKIP LOCKED
        ), copied AS (
            INSERT INTO message_search_cache(session_id, uuid, lower_text, search_key)
            SELECT session_id, uuid, normalized, TRANSLATE(normalized, ' :-/.', 'xqzvy')
            FROM batch
            ON CONFLICT (session_id, uuid) DO NOTHING
        )
        SELECT session_id, uuid INTO next_session, next_uuid
        FROM batch ORDER BY session_id DESC, uuid DESC LIMIT 1;

        IF FOUND THEN
            last_session := next_session;
            last_uuid := next_uuid;
            COMMIT;
        ELSE
            -- SKIP LOCKED is not a completeness check. Rescan skipped rows until
            -- every source row is covered; concurrent inserts already have a trigger.
            IF NOT EXISTS (
                SELECT 1 FROM messages m
                WHERE NOT EXISTS (
                    SELECT 1 FROM message_search_cache c
                    WHERE c.session_id = m.session_id AND c.uuid = m.uuid
                )
            ) THEN
                EXIT;
            END IF;
            last_session := NULL;
            last_uuid := NULL;
            COMMIT;
            PERFORM pg_sleep(0.05);
        END IF;
    END LOOP;
    ANALYZE message_search_cache;
END $copy$;
