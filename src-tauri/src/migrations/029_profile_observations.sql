PRAGMA foreign_keys = OFF;

-- AI-suggested profile observations (#52). The reconcile pass proposes
-- 0..N short notes per attendee per meeting; each lands here as
-- `pending`. The user accepts/rejects from the Team detail page. Only
-- `accepted` rows are read by the #107 worker prompt (wired in a later
-- issue) and by the reconcile-prompt's attendee-context builder.
--
-- `source_note_id` references notes(id) (post-#112 schema; the column
-- was renamed from note_path in migration 026). Cascading delete drops
-- observations when their source meeting note is removed.
CREATE TABLE profile_observations (
  id              TEXT PRIMARY KEY,
  member_id       TEXT NOT NULL REFERENCES team_members(id) ON DELETE CASCADE,
  source_note_id  TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  body            TEXT NOT NULL,
  status          TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'accepted', 'rejected')),
  created_ms      INTEGER NOT NULL,
  reviewed_ms     INTEGER
);
CREATE INDEX idx_obs_member_status ON profile_observations(member_id, status, created_ms DESC);
CREATE INDEX idx_obs_pending       ON profile_observations(status, created_ms DESC)
  WHERE status = 'pending';

PRAGMA foreign_keys = ON;

UPDATE meta SET value = '29' WHERE key = 'schema_version';
