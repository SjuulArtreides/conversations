-- Admin-issued, one-time password reset links.
-- The raw secret is never stored; secret_hash is bcrypt.
-- Sessions record the version current at login. A password reset increments it
-- atomically with the password change, invalidating every older session.
ALTER TABLE users ADD COLUMN auth_version INTEGER NOT NULL DEFAULT 0;

CREATE TABLE password_reset_tokens (
    token_id     TEXT PRIMARY KEY NOT NULL,
    user_email   TEXT NOT NULL,
    secret_hash  TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    used_at      TEXT,
    FOREIGN KEY (user_email) REFERENCES users (email) ON DELETE CASCADE
);

CREATE INDEX idx_password_reset_tokens_user
    ON password_reset_tokens (user_email);

-- Persisted admin grants. Bootstrap admins remain in INITIAL_ADMIN_EMAILS and
-- are intentionally not represented here, so they cannot be revoked in-app.
CREATE TABLE user_admins (
    user_email  TEXT PRIMARY KEY NOT NULL,
    granted_by  TEXT NOT NULL,
    granted_at  TEXT NOT NULL,
    FOREIGN KEY (user_email) REFERENCES users (email) ON DELETE CASCADE
);
