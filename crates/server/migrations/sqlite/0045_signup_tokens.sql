CREATE TABLE signup_tokens (
    email TEXT PRIMARY KEY NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    sent_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
