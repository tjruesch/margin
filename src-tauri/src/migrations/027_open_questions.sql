-- Open questions: derived projection from notes (#113).
--
-- One row per `- [?]` line parsed from a note's body_md. Mirrors the
-- actions table's content-hashed id scheme:
--   id = "<bundle_id>:q:<fnv1a32(text)>"
-- so the same line-collision semantics apply (two `- [?]` lines with
-- identical text collapse to one row — documented v1 trade-off, same
-- as actions).
--
-- The `q:` infix distinguishes question ids from action ids
-- ("<bundle>:<hash>") so the same bundle can carry both without
-- colliding in events.ref_id payloads.

PRAGMA foreign_keys = OFF;

CREATE TABLE note_open_questions (
  id              TEXT PRIMARY KEY,
  origin_note_id  TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  origin_line     INTEGER NOT NULL,
  text            TEXT NOT NULL,
  resolved        INTEGER NOT NULL DEFAULT 0,
  resolved_ms     INTEGER,
  resolved_note   TEXT,
  asked_of_id     TEXT REFERENCES team_members(id) ON DELETE SET NULL,
  created_ms      INTEGER NOT NULL
);
CREATE INDEX idx_open_q_note     ON note_open_questions(origin_note_id);
CREATE INDEX idx_open_q_resolved ON note_open_questions(resolved, created_ms DESC);
CREATE INDEX idx_open_q_asked    ON note_open_questions(asked_of_id)
  WHERE asked_of_id IS NOT NULL;

-- Boot trigger: on first launch after upgrade, walk every note and
-- re-run the (newly extended) body parser so existing `- [?]` lines
-- get picked up into the table. Idempotency-gated by this flag;
-- notes::questions_backfill_if_pending flips it to '1' once done.
INSERT OR IGNORE INTO meta(key, value) VALUES ('questions_backfill_done', '0');

PRAGMA foreign_keys = ON;

UPDATE meta SET value = '27' WHERE key = 'schema_version';
