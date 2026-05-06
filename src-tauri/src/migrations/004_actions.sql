CREATE TABLE actions (
  id          TEXT PRIMARY KEY,
  note_path   TEXT NOT NULL REFERENCES notes(note_path) ON DELETE CASCADE,
  line        INTEGER NOT NULL,
  text        TEXT NOT NULL,
  done        INTEGER NOT NULL DEFAULT 0,
  created_ms  INTEGER NOT NULL
);
CREATE INDEX idx_actions_note ON actions(note_path);
CREATE INDEX idx_actions_done ON actions(done);
UPDATE meta SET value = '4' WHERE key = 'schema_version';
