ALTER TABLE workstreams ADD COLUMN user_notes TEXT;

UPDATE meta SET value = '13' WHERE key = 'schema_version';
