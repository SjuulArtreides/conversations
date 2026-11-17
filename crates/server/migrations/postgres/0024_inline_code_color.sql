-- v24: per-user inline-code colour for Slack-friendly terminal copy.
-- Kept in MEANING-sync with migrations/sqlite/0025_inline_code_color.sql.
-- Stores the viewer's CodeColorConfig as a JSON blob ({"palette":[],"rgb":[]}),
-- or NULL when unset (the client then uses its baked-in default). Opaque to the
-- server — the frontend owns the shape.

ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS inline_code_colors TEXT;
