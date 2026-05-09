ALTER TABLE calendar_events ADD COLUMN linked_note_path TEXT;
CREATE INDEX idx_events_linked_note ON calendar_events(linked_note_path);

UPDATE meta SET value = '10' WHERE key = 'schema_version';
