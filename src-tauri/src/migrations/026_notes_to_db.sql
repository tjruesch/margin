-- Move note storage from filesystem to SQLite (#112).
--
-- Notes were `~/.margin/notes/<bundle>/note.md` files on disk; the DB
-- was a cache kept in sync via a watcher + reconcile loop. This
-- migration moves the canonical content into a `notes.body_md` column
-- and rewrites every FK / soft-reference that pointed at note paths.
--
-- Identity choice: `notes.id := bundle_id`. Bundle ids are already
-- UUID-shaped (or the magic 'inbox'), action ids are content-hashed
-- over `<bundle>:<text>` so they stay byte-identical, and
-- `events.ref_id` payloads for note rows survive without rewrite.
--
-- The disk → DB body backfill runs in Rust at boot, gated by the
-- `notes_body_backfill_done` meta flag set below. After backfill the
-- legacy notes folder is renamed to `notes-archive-pre-v26/` so the
-- watcher (also dropped in #112) has no path to rebind to.

PRAGMA foreign_keys = OFF;

-- 1. Rebuild `notes` with `id` as the primary key. body_md/summary_md/
--    kind/created_ms are new; everything else carries over.
CREATE TABLE notes_new (
  id           TEXT PRIMARY KEY,
  bundle_id    TEXT NOT NULL,
  title        TEXT NOT NULL,
  body_md      TEXT NOT NULL DEFAULT '',
  summary_md   TEXT,
  kind         TEXT NOT NULL DEFAULT 'note',
  modified_ms  INTEGER NOT NULL,
  duration_ms  INTEGER,
  preview      TEXT NOT NULL DEFAULT '',
  body_size    INTEGER NOT NULL DEFAULT 0,
  archived     INTEGER NOT NULL DEFAULT 0,
  favorite     INTEGER NOT NULL DEFAULT 0,
  created_ms   INTEGER NOT NULL DEFAULT 0
);
INSERT INTO notes_new
  (id, bundle_id, title, body_md, summary_md, kind, modified_ms,
   duration_ms, preview, body_size, archived, favorite, created_ms)
SELECT bundle_id, bundle_id, title, '', NULL, 'note', modified_ms,
       duration_ms, preview, body_size, archived, favorite, modified_ms
FROM notes;

-- 2. Hard-FK children: rebuild every table that referenced
--    notes(note_path). The OLD notes table is still around as `notes`
--    so we can translate paths → bundle_ids via JOIN.

CREATE TABLE tags_new (
  note_id TEXT NOT NULL REFERENCES notes_new(id) ON DELETE CASCADE,
  tag     TEXT NOT NULL,
  PRIMARY KEY (note_id, tag)
);
INSERT INTO tags_new (note_id, tag)
SELECT n.bundle_id, t.tag
FROM tags t JOIN notes n ON n.note_path = t.note_path;

CREATE TABLE meeting_attendees_new (
  note_id       TEXT NOT NULL REFERENCES notes_new(id) ON DELETE CASCADE,
  member_id     TEXT NOT NULL REFERENCES team_members(id) ON DELETE CASCADE,
  speaker_index INTEGER,
  PRIMARY KEY (note_id, member_id)
);
INSERT INTO meeting_attendees_new (note_id, member_id, speaker_index)
SELECT n.bundle_id, ma.member_id, ma.speaker_index
FROM meeting_attendees ma
JOIN notes n ON n.note_path = ma.note_path;

-- actions: rename origin_note_path → origin_note_id and map paths to
-- ids. The other columns survive unchanged. Synth-origin rows
-- (origin_note_path IS NULL) carry through unchanged.
CREATE TABLE actions_new (
  id                 TEXT PRIMARY KEY,
  origin_note_id     TEXT REFERENCES notes_new(id) ON DELETE CASCADE,
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
INSERT INTO actions_new
  (id, origin_note_id, origin_line, text, done, created_ms, due_ms,
   reminder_sent_ms, assignee_id, origin_kind, origin_synth_kind,
   origin_synth_id, workstream_id)
SELECT a.id,
       CASE
         WHEN a.origin_note_path IS NULL THEN NULL
         ELSE (SELECT bundle_id FROM notes WHERE note_path = a.origin_note_path)
       END,
       a.origin_line, a.text, a.done, a.created_ms, a.due_ms,
       a.reminder_sent_ms, a.assignee_id, a.origin_kind,
       a.origin_synth_kind, a.origin_synth_id, a.workstream_id
FROM actions a;

-- 3. Soft references: rewrite ref_id / item_id / src_id / tgt_id where
--    they held a note_path. The OLD notes table is still `notes` —
--    use it as the translation table. COALESCE protects against
--    rows whose path doesn't resolve (leaves them as-is for the
--    legacy log).

UPDATE workstream_signals
   SET item_id = COALESCE(
     (SELECT bundle_id FROM notes WHERE note_path = workstream_signals.item_id),
     item_id)
 WHERE kind = 'note';

UPDATE embeddings
   SET ref_id = COALESCE(
     (SELECT bundle_id FROM notes WHERE note_path = embeddings.ref_id),
     ref_id)
 WHERE ref_kind = 'note';

UPDATE events
   SET ref_id = COALESCE(
     (SELECT bundle_id FROM notes WHERE note_path = events.ref_id),
     ref_id)
 WHERE ref_kind = 'note';

UPDATE edges
   SET src_id = COALESCE(
     (SELECT bundle_id FROM notes WHERE note_path = edges.src_id),
     src_id)
 WHERE src_kind = 'note';

UPDATE edges
   SET tgt_id = COALESCE(
     (SELECT bundle_id FROM notes WHERE note_path = edges.tgt_id),
     tgt_id)
 WHERE tgt_kind = 'note';

-- 4. calendar_events.linked_note_path → linked_note_id (soft TEXT,
--    no FK — the link is user-curated). Add the new column, backfill,
--    drop the old (SQLite 3.35+ supports DROP COLUMN).
ALTER TABLE calendar_events ADD COLUMN linked_note_id TEXT;
UPDATE calendar_events
   SET linked_note_id = (
     SELECT bundle_id FROM notes WHERE note_path = calendar_events.linked_note_path
   )
 WHERE linked_note_path IS NOT NULL;
DROP INDEX IF EXISTS idx_events_linked_note;
ALTER TABLE calendar_events DROP COLUMN linked_note_path;

-- 5. Swap tables and drop legacy.
DROP TABLE tags;              ALTER TABLE tags_new              RENAME TO tags;
DROP TABLE meeting_attendees; ALTER TABLE meeting_attendees_new  RENAME TO meeting_attendees;
DROP TABLE actions;           ALTER TABLE actions_new           RENAME TO actions;
DROP TABLE notes_fts;
DROP TABLE notes;             ALTER TABLE notes_new             RENAME TO notes;

-- 6. Rebuild FTS keyed on note_id. Body starts empty — the Rust
--    backfill (notes::body_backfill_if_pending) repopulates it
--    on first boot post-migration.
CREATE VIRTUAL TABLE notes_fts USING fts5(
  note_id UNINDEXED,
  title,
  body,
  tokenize = 'porter unicode61'
);
INSERT INTO notes_fts(note_id, title, body)
SELECT id, title, '' FROM notes;

-- 7. Indexes for the new schema. The old indexes died with the
--    table drops above; recreate them with their canonical names
--    against the renamed tables.
CREATE INDEX idx_tags_tag                ON tags(tag);
CREATE INDEX idx_meeting_attendees_member ON meeting_attendees(member_id);

CREATE INDEX idx_notes_modified   ON notes(modified_ms DESC);
CREATE INDEX idx_notes_archived   ON notes(archived, modified_ms DESC);
CREATE INDEX idx_notes_favorite   ON notes(favorite, modified_ms DESC);

CREATE INDEX idx_actions_note     ON actions(origin_note_id) WHERE origin_note_id IS NOT NULL;
CREATE INDEX idx_actions_ws       ON actions(workstream_id)  WHERE workstream_id IS NOT NULL;
CREATE INDEX idx_actions_done     ON actions(done, due_ms);
CREATE INDEX idx_actions_due      ON actions(due_ms) WHERE due_ms IS NOT NULL;
CREATE INDEX idx_actions_assignee ON actions(assignee_id) WHERE assignee_id IS NOT NULL;
CREATE INDEX idx_actions_synth    ON actions(origin_synth_kind, origin_synth_id)
  WHERE origin_synth_kind IS NOT NULL;

CREATE INDEX idx_events_calendar_linked_note ON calendar_events(linked_note_id)
  WHERE linked_note_id IS NOT NULL;

-- 8. Backfill marker. notes::body_backfill_if_pending flips this to
--    '1' after walking disk and populating body_md + the FTS rows.
INSERT OR IGNORE INTO meta(key, value) VALUES ('notes_body_backfill_done', '0');

PRAGMA foreign_keys = ON;

UPDATE meta SET value = '26' WHERE key = 'schema_version';
