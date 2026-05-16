PRAGMA foreign_keys = ON;

ALTER TABLE prompt_dumps ADD COLUMN cache_creation_tokens INTEGER;
ALTER TABLE prompt_dumps ADD COLUMN cache_read_tokens INTEGER;

UPDATE meta SET value = '38' WHERE key = 'schema_version';
