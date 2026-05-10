PRAGMA foreign_keys = ON;

CREATE TABLE workstreams (
  id                 TEXT PRIMARY KEY,
  title              TEXT NOT NULL,
  summary            TEXT NOT NULL,
  status             TEXT NOT NULL DEFAULT 'active',
  last_activity_ms   INTEGER NOT NULL,
  created_ms         INTEGER NOT NULL,
  updated_ms         INTEGER NOT NULL
);
CREATE INDEX idx_workstreams_status ON workstreams(status, last_activity_ms DESC);

CREATE TABLE workstream_emails (
  workstream_id  TEXT NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
  message_id     TEXT NOT NULL REFERENCES email_messages(id) ON DELETE CASCADE,
  relevance      REAL,
  PRIMARY KEY (workstream_id, message_id)
);
CREATE INDEX idx_ws_emails_msg ON workstream_emails(message_id);

CREATE TABLE workstream_events (
  workstream_id  TEXT NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
  event_id       TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
  PRIMARY KEY (workstream_id, event_id)
);
CREATE INDEX idx_ws_events_ev ON workstream_events(event_id);

CREATE TABLE workstream_notes (
  workstream_id  TEXT NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
  note_path      TEXT NOT NULL,
  PRIMARY KEY (workstream_id, note_path)
);
CREATE INDEX idx_ws_notes_path ON workstream_notes(note_path);

CREATE TABLE workstream_actions (
  id              TEXT PRIMARY KEY,
  workstream_id   TEXT NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
  text            TEXT NOT NULL,
  due_ms          INTEGER,
  source_kind     TEXT NOT NULL,
  source_id       TEXT NOT NULL,
  done            INTEGER NOT NULL DEFAULT 0,
  created_ms      INTEGER NOT NULL
);
CREATE INDEX idx_ws_actions_ws ON workstream_actions(workstream_id);
CREATE INDEX idx_ws_actions_done ON workstream_actions(done, created_ms DESC);

INSERT INTO meta(key, value) VALUES ('last_clustered_ms', '0');

UPDATE meta SET value = '12' WHERE key = 'schema_version';
