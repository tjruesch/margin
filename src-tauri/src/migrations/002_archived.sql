ALTER TABLE notes ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_notes_archived ON notes(archived, modified_ms DESC);
UPDATE meta SET value = '2' WHERE key = 'schema_version';
