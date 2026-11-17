-- v5: email+password auth (parallel to Google OIDC). No email verification.
-- A NULL password_hash means the user has no local password (e.g. a Google-only
-- account, or a row created by sharing before that person ever signs in).
ALTER TABLE users ADD COLUMN IF NOT EXISTS password_hash TEXT;
