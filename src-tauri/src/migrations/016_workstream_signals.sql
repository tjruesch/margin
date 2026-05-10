PRAGMA foreign_keys = ON;

CREATE TABLE workstream_signals (
  workstream_id  TEXT NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
  kind           TEXT NOT NULL,
  item_id        TEXT NOT NULL,
  added_ms       INTEGER NOT NULL,
  PRIMARY KEY (workstream_id, kind, item_id)
);
CREATE INDEX idx_signals_workstream_kind ON workstream_signals(workstream_id, kind);
CREATE INDEX idx_signals_kind_item ON workstream_signals(kind, item_id);

INSERT OR IGNORE INTO workstream_signals(workstream_id, kind, item_id, added_ms)
SELECT we.workstream_id, 'email', we.message_id, COALESCE(w.updated_ms, 0)
FROM workstream_emails we
LEFT JOIN workstreams w ON w.id = we.workstream_id;

INSERT OR IGNORE INTO workstream_signals(workstream_id, kind, item_id, added_ms)
SELECT wev.workstream_id, 'event', wev.event_id, COALESCE(w.updated_ms, 0)
FROM workstream_events wev
LEFT JOIN workstreams w ON w.id = wev.workstream_id;

INSERT OR IGNORE INTO workstream_signals(workstream_id, kind, item_id, added_ms)
SELECT wn.workstream_id, 'note', wn.note_path, COALESCE(w.updated_ms, 0)
FROM workstream_notes wn
LEFT JOIN workstreams w ON w.id = wn.workstream_id;

DROP TABLE workstream_emails;
DROP TABLE workstream_events;
DROP TABLE workstream_notes;

UPDATE meta SET value = '16' WHERE key = 'schema_version';
