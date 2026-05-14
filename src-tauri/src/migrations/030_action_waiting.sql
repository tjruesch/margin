PRAGMA foreign_keys = OFF;

-- Two new columns on `actions` (#107/#120 follow-up).
--
-- `subject_member_id` — for synth-extracted actions tied to a
-- counterparty (an email from Heike, a Teams DM from Davis), this
-- points at the *other* person in the conversation. NULL for
-- note-extracted actions and any case where there's no single
-- counterparty. Used by the Team-detail Profile pane to filter
-- waiting items per-member without joining through source rows.
--
-- `manual_override` — set to 1 the moment the user touches a synth
-- action (toggles done, edits text, reassigns). Once set, the worker
-- never auto-modifies the row again. Prevents the "user unchecks,
-- worker re-checks, repeat" feedback loop after the LLM judges
-- something resolved.
ALTER TABLE actions ADD COLUMN subject_member_id TEXT
  REFERENCES team_members(id) ON DELETE SET NULL;
ALTER TABLE actions ADD COLUMN manual_override INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_actions_subject ON actions(subject_member_id);

-- When the user explicitly dismisses a waiting action, we record the
-- source so the worker doesn't recreate it on the next recompute.
-- Composite PK is the natural unique key. CASCADE on member delete
-- so dismissed rows don't outlive their counterparty.
CREATE TABLE dismissed_action_sources (
  origin_synth_kind TEXT NOT NULL,
  origin_synth_id   TEXT NOT NULL,
  assignee_id       TEXT REFERENCES team_members(id) ON DELETE CASCADE,
  dismissed_ms      INTEGER NOT NULL,
  PRIMARY KEY (origin_synth_kind, origin_synth_id, assignee_id)
);

PRAGMA foreign_keys = ON;

UPDATE meta SET value = '30' WHERE key = 'schema_version';
