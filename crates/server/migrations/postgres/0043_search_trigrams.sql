-- PostgreSQL-only access path: preserve LOWER(text) LIKE substring semantics.
-- SQLite's schema and search behavior do not change.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
