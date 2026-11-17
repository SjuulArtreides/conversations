-- Admin-issued, one-time password reset links.
-- The raw secret is never stored; secret_hash is bcrypt.
-- Sessions record the version current at login. A password reset increments it
-- atomically with the password change, invalidating every older session.
ALTER TABLE users ADD COLUMN auth_version BIGINT NOT NULL DEFAULT 0;

CREATE TABLE password_reset_tokens (
    token_id     TEXT PRIMARY KEY NOT NULL,
    user_email   TEXT NOT NULL REFERENCES users (email) ON DELETE CASCADE,
    secret_hash  TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL,
    used_at      TIMESTAMPTZ
);

CREATE INDEX idx_password_reset_tokens_user
    ON password_reset_tokens (user_email);

-- Persisted admin grants. Bootstrap admins remain in INITIAL_ADMIN_EMAILS and
-- are intentionally not represented here, so they cannot be revoked in-app.
CREATE TABLE user_admins (
    user_email  TEXT PRIMARY KEY NOT NULL REFERENCES users (email) ON DELETE CASCADE,
    granted_by  TEXT NOT NULL,
    granted_at  TIMESTAMPTZ NOT NULL
);
