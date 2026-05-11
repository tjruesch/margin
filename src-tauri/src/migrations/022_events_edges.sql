-- Foundation tables: events + edges (#102).
--
-- `events` is an append-only activity stream. Every entity write
-- (existing or new kinds) drops a row here. Drives "what did I do
-- today" views, profile derivation, the deterministic edge synthesizer
-- (#103), and the embeddings pipeline (#104).
--
-- `edges` is a generic graph layer. One row per typed relationship
-- between any two entities. Subsumes the read-side role of
-- workstream_signals (the pivot stays; this table is a superset
-- projection plus relationships between non-workstream entities).
--
-- This migration is pure-add — no existing read paths change. The
-- inline backfill at the end populates both tables from current data
-- so the new surfaces are useful from day one.

PRAGMA foreign_keys = ON;

CREATE TABLE events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_ms       INTEGER NOT NULL,
  kind        TEXT NOT NULL,
  actor_id    TEXT REFERENCES team_members(id) ON DELETE SET NULL,
  ref_kind    TEXT,
  ref_id      TEXT,
  payload     TEXT,
  created_ms  INTEGER NOT NULL
);
CREATE INDEX idx_events_ts ON events(ts_ms DESC);
CREATE INDEX idx_events_ref ON events(ref_kind, ref_id);
CREATE INDEX idx_events_actor ON events(actor_id, ts_ms DESC);
CREATE INDEX idx_events_kind ON events(kind, ts_ms DESC);

CREATE TABLE edges (
  src_kind       TEXT NOT NULL,
  src_id         TEXT NOT NULL,
  tgt_kind       TEXT NOT NULL,
  tgt_id         TEXT NOT NULL,
  edge_kind      TEXT NOT NULL,
  confidence     REAL NOT NULL DEFAULT 1.0,
  evidence       TEXT NOT NULL DEFAULT '[]',
  first_seen_ms  INTEGER NOT NULL,
  last_seen_ms   INTEGER NOT NULL,
  PRIMARY KEY (src_kind, src_id, tgt_kind, tgt_id, edge_kind)
);
CREATE INDEX idx_edges_src ON edges(src_kind, src_id);
CREATE INDEX idx_edges_tgt ON edges(tgt_kind, tgt_id);
CREATE INDEX idx_edges_kind ON edges(edge_kind, last_seen_ms DESC);

-- ============================================================
-- events backfill
-- ============================================================

-- email_sent / email_received. Resolve actor via team_member_aliases
-- (kind='email'). The 'sent' branch is when the matched member is_self;
-- external senders that don't resolve to any teammate get actor_id NULL.
INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
SELECT
  e.sent_at_ms,
  CASE WHEN EXISTS (
    SELECT 1 FROM team_member_aliases a
    JOIN team_members m ON m.id = a.member_id
    WHERE a.kind = 'email'
      AND lower(a.value) = lower(e.from_email)
      AND m.is_self = 1
  ) THEN 'email_sent' ELSE 'email_received' END,
  (SELECT a.member_id FROM team_member_aliases a
    WHERE a.kind = 'email' AND lower(a.value) = lower(e.from_email)
    LIMIT 1),
  'email', e.id,
  json_object('thread_id', e.thread_id, 'subject', e.subject),
  e.sent_at_ms
FROM email_messages e;

-- meeting. Always actor=self (calendar events are on the user's
-- calendar; per-attendee edges go in the edges table below).
INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
SELECT
  c.start_ms,
  'meeting',
  (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1),
  'event', c.id,
  json_object('title', c.title, 'all_day', c.all_day),
  c.start_ms
FROM calendar_events c;

-- note_modified. notes has no created_ms column; modified_ms is the
-- only timestamp available, so we emit a single 'note_modified' event
-- per existing note. Live note inserts going forward will emit
-- 'note_created' separately.
INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
SELECT
  n.modified_ms,
  'note_modified',
  (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1),
  'note', n.note_path,
  json_object('title', n.title, 'bundle_id', n.bundle_id),
  n.modified_ms
FROM notes n;

-- action_created (note-backed actions). The actions table has no
-- done_ms column, so 'action_completed' events have no backfill
-- source — they get emitted by future live writes only.
INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
SELECT
  a.created_ms,
  'action_created',
  COALESCE(a.assignee_id, (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1)),
  'action', a.id,
  json_object('text', a.text, 'note_path', a.note_path),
  a.created_ms
FROM actions a;

-- action_created (workstream-backed actions).
INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
SELECT
  wa.created_ms,
  'action_created',
  COALESCE(wa.assignee_id, (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1)),
  'action', wa.id,
  json_object('text', wa.text, 'workstream_id', wa.workstream_id),
  wa.created_ms
FROM workstream_actions wa;

-- ============================================================
-- edges backfill
-- ============================================================

-- INCLUDES: workstream → email/event/note (from workstream_signals).
INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
SELECT 'workstream', s.workstream_id, s.kind, s.item_id, 'INCLUDES',
       s.added_ms, s.added_ms
FROM workstream_signals s
;

-- ATTENDED: person → event (from calendar_attendees rows whose email
-- has been resolved to a team_member). Self gets ATTENDED edges
-- alongside teammates so the graph treats them uniformly.
INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
SELECT 'person', ca.team_member_id, 'event', ca.event_id, 'ATTENDED',
       ce.start_ms, ce.start_ms
FROM calendar_attendees ca
JOIN calendar_events ce ON ce.id = ca.event_id
WHERE ca.team_member_id IS NOT NULL
;

-- OWNS: person → action (note-backed actions with a resolved assignee).
INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
SELECT 'person', a.assignee_id, 'action', a.id, 'OWNS',
       a.created_ms, a.created_ms
FROM actions a
WHERE a.assignee_id IS NOT NULL
;

-- OWNS: person → action (workstream-backed actions with a resolved assignee).
INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
SELECT 'person', wa.assignee_id, 'action', wa.id, 'OWNS',
       wa.created_ms, wa.created_ms
FROM workstream_actions wa
WHERE wa.assignee_id IS NOT NULL
;

UPDATE meta SET value = '22' WHERE key = 'schema_version';
