ALTER TABLE workstreams ADD COLUMN archived_at_ms INTEGER;
ALTER TABLE workstreams ADD COLUMN reopened_at_ms INTEGER;
CREATE INDEX idx_workstreams_reopened ON workstreams(reopened_at_ms) WHERE reopened_at_ms IS NOT NULL;

UPDATE meta SET value = '14' WHERE key = 'schema_version';
