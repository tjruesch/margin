PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE notes (
  note_path    TEXT PRIMARY KEY,
  bundle_id    TEXT NOT NULL,
  title        TEXT NOT NULL,
  modified_ms  INTEGER NOT NULL,
  duration_ms  INTEGER,
  preview      TEXT NOT NULL DEFAULT '',
  body_size    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_notes_modified ON notes(modified_ms DESC);

CREATE TABLE tags (
  note_path TEXT NOT NULL REFERENCES notes(note_path) ON DELETE CASCADE,
  tag       TEXT NOT NULL,
  PRIMARY KEY (note_path, tag)
);
CREATE INDEX idx_tags_tag ON tags(tag);

CREATE VIRTUAL TABLE notes_fts USING fts5(
  note_path UNINDEXED,
  title,
  body,
  tokenize = 'porter unicode61'
);

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
INSERT INTO meta(key, value) VALUES ('schema_version', '1');
