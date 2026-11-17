-- Published discovery candidates. This table confers no host authorization.
CREATE TABLE browser_keys (
 user_email TEXT NOT NULL REFERENCES users(email) ON DELETE CASCADE,
 kid TEXT NOT NULL,
 public_key TEXT NOT NULL,
 label TEXT NOT NULL,
 created_at TEXT NOT NULL,
 PRIMARY KEY(user_email, kid)
);
CREATE TABLE browser_key_challenges (
 challenge TEXT PRIMARY KEY,
 user_email TEXT NOT NULL REFERENCES users(email) ON DELETE CASCADE,
 payload TEXT NOT NULL,
 expires_at BIGINT NOT NULL
);
CREATE INDEX browser_key_challenge_expiry ON browser_key_challenges(expires_at);
