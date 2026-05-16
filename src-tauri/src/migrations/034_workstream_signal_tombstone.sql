PRAGMA foreign_keys = ON;

ALTER TABLE workstream_signals ADD COLUMN manual_detached_ms INTEGER;

UPDATE meta SET value = '34' WHERE key = 'schema_version';
