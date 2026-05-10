-- User-curated external URLs on workstreams (#88).
--
-- Workstreams synthesize from emails / meetings / notes, but the user
-- often wants to attach the actual external artifact: the GitHub repo,
-- the Linear project, the Notion design doc. These are user-only — the
-- synthesizer never touches this table. The detail view renders them
-- as clickable chips (opened via tauri-plugin-opener) and read_workstream
-- folds them into the AI ask context.
--
-- `position` is reserved for a future drag-handle reorder UI; v1 keeps
-- insertion order via (position, created_ms). `kind` is a soft enum
-- ("github" | "linear" | "notion" | "figma" | "other" | NULL) — adding
-- a new kind is non-breaking, just a new icon mapping client-side.

CREATE TABLE workstream_links (
  id            TEXT PRIMARY KEY,
  workstream_id TEXT NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
  label         TEXT NOT NULL,
  url           TEXT NOT NULL,
  kind          TEXT,
  position      INTEGER NOT NULL DEFAULT 0,
  created_ms    INTEGER NOT NULL
);
CREATE INDEX idx_workstream_links_ws
  ON workstream_links(workstream_id, position, created_ms);

UPDATE meta SET value = '18' WHERE key = 'schema_version';
