-- v10: project description — an optional free-text blurb on an explicit project.
--
-- Nullable: existing rows and projects created without one carry NULL.
ALTER TABLE projects ADD COLUMN description TEXT;
