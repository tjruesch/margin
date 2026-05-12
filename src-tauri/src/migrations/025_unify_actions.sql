-- Unify action items into one table (#111).
--
-- Collapses `workstream_actions` into `actions`. Adds origin_kind +
-- nullable origin_note_path/origin_line/origin_synth_kind/origin_synth_id
-- + workstream_id columns. Backfills synth rows from workstream_actions
-- using the existing wsa_<sha256>-style ids (no collision with the
-- note-side <bundle>:<hash> ids).
--
-- See issue #111 for the dedup-on-next-synth-pass policy. This migration
-- is data-preserving only: a note row and a synth row that paraphrase
-- the same todo both survive intact here; runtime dedup happens the
-- next time the synthesizer runs.

PRAGMA foreign_keys = OFF;

-- 1. Rebuild `actions` with the unified schema. Original columns kept:
--    id, text, done, created_ms, due_ms, reminder_sent_ms, assignee_id.
--    `note_path` and `line` are renamed to `origin_note_path`/`origin_line`
--    AND relaxed to NULLABLE so synth rows can omit them.
CREATE TABLE actions_new (
  id                 TEXT PRIMARY KEY,
  origin_note_path   TEXT REFERENCES notes(note_path) ON DELETE CASCADE,
  origin_line        INTEGER,
  text               TEXT NOT NULL,
  done               INTEGER NOT NULL DEFAULT 0,
  created_ms         INTEGER NOT NULL,
  due_ms             INTEGER,
  reminder_sent_ms   INTEGER,
  assignee_id        TEXT REFERENCES team_members(id) ON DELETE SET NULL,
  origin_kind        TEXT NOT NULL,
  origin_synth_kind  TEXT,
  origin_synth_id    TEXT,
  workstream_id      TEXT REFERENCES workstreams(id) ON DELETE SET NULL
);

-- 2. Carry existing note-backed rows over with origin_kind='note'.
INSERT INTO actions_new
  (id, origin_note_path, origin_line, text, done, created_ms, due_ms,
   reminder_sent_ms, assignee_id, origin_kind)
SELECT
   id, note_path, line, text, done, created_ms, due_ms,
   reminder_sent_ms, assignee_id, 'note'
FROM actions;

-- 3. Carry existing synth-backed rows over with origin_kind='synth'.
--    INSERT OR IGNORE guards against an unlikely id collision; note ids
--    are "<bundle>:<8hex>" and synth ids are "wsa_<64hex>" so the
--    schemes never overlap, but the OR IGNORE makes the migration
--    self-healing if a row sneaks in twice.
INSERT OR IGNORE INTO actions_new
  (id, text, done, due_ms, assignee_id, created_ms,
   origin_kind, origin_synth_kind, origin_synth_id, workstream_id)
SELECT
   id, text, done, due_ms, assignee_id, created_ms,
   'synth', source_kind, source_id, workstream_id
FROM workstream_actions;

-- 4. Swap in the new table and drop the old workstream_actions table.
DROP TABLE actions;
ALTER TABLE actions_new RENAME TO actions;
DROP TABLE workstream_actions;

-- 5. Indexes.
CREATE INDEX idx_actions_note     ON actions(origin_note_path) WHERE origin_note_path IS NOT NULL;
CREATE INDEX idx_actions_ws       ON actions(workstream_id)    WHERE workstream_id IS NOT NULL;
CREATE INDEX idx_actions_done     ON actions(done, due_ms);
CREATE INDEX idx_actions_due      ON actions(due_ms) WHERE due_ms IS NOT NULL;
CREATE INDEX idx_actions_assignee ON actions(assignee_id) WHERE assignee_id IS NOT NULL;
CREATE INDEX idx_actions_synth    ON actions(origin_synth_kind, origin_synth_id)
  WHERE origin_synth_kind IS NOT NULL;

PRAGMA foreign_keys = ON;

UPDATE meta SET value = '25' WHERE key = 'schema_version';
