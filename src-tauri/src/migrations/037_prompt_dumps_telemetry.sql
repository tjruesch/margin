PRAGMA foreign_keys = ON;

ALTER TABLE prompt_dumps ADD COLUMN query TEXT NOT NULL DEFAULT '';
ALTER TABLE prompt_dumps ADD COLUMN tokens_in INTEGER;
ALTER TABLE prompt_dumps ADD COLUMN tokens_out INTEGER;

UPDATE meta SET value = '37' WHERE key = 'schema_version';
