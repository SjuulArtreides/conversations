-- v24 (fold keyboard control into write): the separate `can_control` grant is
-- removed. Write access now allows keyboard/clipboard input as well, so there is
-- a single interactive grant (`can_write`) instead of write + a stronger control
-- escalation.
--
-- Data-preserving: any sharee who held control could already write, but coerce
-- defensively so no former controller loses interactive access, THEN drop the
-- column. SQLite (>= 3.35) supports ALTER TABLE ... DROP COLUMN; the bundled
-- libsqlite3-sys is 3.46. `can_control` has no index/trigger referencing it.
--
-- MEANING-sync with migrations/postgres/0023_drop_live_share_can_control.sql.

UPDATE live_session_shares SET can_write = 1 WHERE can_control = 1;

ALTER TABLE live_session_shares DROP COLUMN can_control;
