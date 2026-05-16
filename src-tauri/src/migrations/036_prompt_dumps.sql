PRAGMA foreign_keys = ON;

CREATE TABLE prompt_dumps (
  turn_id          TEXT PRIMARY KEY,
  prompt           TEXT NOT NULL,
  system_prompt    TEXT NOT NULL,
  tool_names_json  TEXT NOT NULL,
  sources_json     TEXT NOT NULL,
  dispatches_json  TEXT NOT NULL DEFAULT '[]',
  latency_ms       INTEGER NOT NULL,
  created_ms       INTEGER NOT NULL
);
CREATE INDEX idx_prompt_dumps_created ON prompt_dumps(created_ms DESC);

UPDATE meta SET value = '36' WHERE key = 'schema_version';
