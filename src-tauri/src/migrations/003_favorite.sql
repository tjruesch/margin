ALTER TABLE notes ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_notes_favorite ON notes(favorite, modified_ms DESC);
UPDATE meta SET value = '3' WHERE key = 'schema_version';
