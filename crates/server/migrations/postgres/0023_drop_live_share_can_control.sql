-- v24 (fold keyboard control into write): the separate `can_control` grant is
-- removed. Write access now allows keyboard/clipboard input as well, so there is
-- a single interactive grant (`can_write`) instead of write + a stronger control
-- escalation. (Postgres dialect.)
--
-- Data-preserving: coerce any former controller to a writer so none lose
-- interactive access, THEN drop the column.
--
-- MEANING-sync with migrations/sqlite/0024_drop_live_share_can_control.sql.

UPDATE live_session_shares SET can_write = true WHERE can_control = true;

ALTER TABLE live_session_shares DROP COLUMN IF EXISTS can_control;
