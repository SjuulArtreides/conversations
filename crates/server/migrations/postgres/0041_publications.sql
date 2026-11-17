-- Explicit publication ledger. Meaning-sync with
-- migrations/sqlite/0042_publications.sql; see there for the column contract.
-- The ledger is the tombstone and has NO foreign key to sessions.
CREATE TABLE publications (
    session_id      TEXT PRIMARY KEY NOT NULL,
    host_id         TEXT NOT NULL,
    host_public_key TEXT NOT NULL,
    owner_email     TEXT,
    max_rev         BIGINT NOT NULL CHECK (max_rev > 0),
    state           TEXT NOT NULL CHECK (state IN ('active', 'deleted')),
    next_seq        BIGINT NOT NULL DEFAULT 0 CHECK (next_seq >= 0),
    share_sha256    TEXT,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);

CREATE TABLE publication_chunks (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq        BIGINT NOT NULL CHECK (seq >= 0),
    sha256     TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

-- The links each published message carries, so updating a message in place
-- replaces exactly its links. Published content: removed with the session row.
CREATE TABLE publication_message_refs (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    uuid       TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('pr', 'linear')),
    url        TEXT NOT NULL,
    PRIMARY KEY (session_id, uuid, kind, url)
);
