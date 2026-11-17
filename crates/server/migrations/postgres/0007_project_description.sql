-- v10: project description (Postgres dialect).
--
-- Kept in MEANING-sync with migrations/sqlite/0008_project_description.sql:
-- an optional free-text blurb on an explicit project. Nullable, so existing
-- rows and projects created without one carry NULL.
ALTER TABLE projects ADD COLUMN description TEXT;
