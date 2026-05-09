PRAGMA foreign_keys = ON;

CREATE TABLE connectors (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  display_name  TEXT NOT NULL,
  enabled       INTEGER NOT NULL DEFAULT 1,
  config_json   TEXT NOT NULL DEFAULT '{}',
  created_ms    INTEGER NOT NULL,
  updated_ms    INTEGER NOT NULL
);
CREATE INDEX idx_connectors_kind ON connectors(kind);
CREATE INDEX idx_connectors_enabled ON connectors(enabled);

CREATE TABLE sync_status (
  connector_id    TEXT PRIMARY KEY REFERENCES connectors(id) ON DELETE CASCADE,
  last_sync_ms    INTEGER,
  last_success_ms INTEGER,
  last_error      TEXT,
  cursor          TEXT,
  next_due_ms     INTEGER NOT NULL DEFAULT 0
);

UPDATE meta SET value = '8' WHERE key = 'schema_version';
