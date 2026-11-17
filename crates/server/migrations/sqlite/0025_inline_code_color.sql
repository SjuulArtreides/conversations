-- v25: per-user inline-code colour for Slack-friendly terminal copy.
-- Stores the viewer's CodeColorConfig as a JSON blob ({"palette":[],"rgb":[]}),
-- or NULL when unset (the client then uses its baked-in default). The server
-- treats this as an opaque preference string — the frontend owns the shape.

ALTER TABLE user_settings ADD COLUMN inline_code_colors TEXT;
