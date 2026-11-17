-- Explicit publication ledger. One row per managed session id, retained after
-- unshare as the tombstone, so it deliberately has NO foreign key to sessions:
-- deleting the published content never cascades into it.
--
-- host_id/host_public_key are the signing publisher; owner_email is the central
-- account bound in the signed operations. max_rev is the highest accepted share
-- revision (share or unshare); state is the outcome at max_rev. next_seq is the
-- next chunk sequence for an active revision and is retained in the tombstone.
-- share_sha256 is the digest of the accepted share payload at max_rev (exact
-- retries are idempotent; different bytes at the same revision are rejected).
CREATE TABLE publications (
    session_id      TEXT PRIMARY KEY NOT NULL,
    host_id         TEXT NOT NULL,
    host_public_key TEXT NOT NULL,
    owner_email     TEXT,
    max_rev         INTEGER NOT NULL CHECK (max_rev > 0),
    state           TEXT NOT NULL CHECK (state IN ('active', 'deleted')),
    next_seq        INTEGER NOT NULL DEFAULT 0 CHECK (next_seq >= 0),
    share_sha256    TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Accepted chunk digests of the active revision. Published content: removed
-- with the session row.
CREATE TABLE publication_chunks (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL CHECK (seq >= 0),
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
