CREATE TABLE team_members (
  id              TEXT PRIMARY KEY,
  display_name    TEXT NOT NULL,
  role            TEXT NOT NULL DEFAULT '',
  aliases         TEXT NOT NULL DEFAULT '[]',
  profile_md_path TEXT NOT NULL,
  is_self         INTEGER NOT NULL DEFAULT 0,
  created_ms      INTEGER NOT NULL,
  updated_ms      INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_team_self ON team_members(is_self) WHERE is_self = 1;

CREATE TABLE meeting_attendees (
  note_path     TEXT NOT NULL REFERENCES notes(note_path) ON DELETE CASCADE,
  member_id     TEXT NOT NULL REFERENCES team_members(id) ON DELETE CASCADE,
  speaker_index INTEGER,
  PRIMARY KEY (note_path, member_id)
);
CREATE INDEX idx_meeting_attendees_member ON meeting_attendees(member_id);

ALTER TABLE actions ADD COLUMN assignee_id TEXT REFERENCES team_members(id) ON DELETE SET NULL;
CREATE INDEX idx_actions_assignee ON actions(assignee_id) WHERE assignee_id IS NOT NULL;

UPDATE meta SET value = '6' WHERE key = 'schema_version';
