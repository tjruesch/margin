-- Workstream-action assignee column (#100).
--
-- Synthesized workstream actions can now carry an owner, in parity
-- with markdown-backed action items in notes. The synthesizer emits
-- an optional owner_label per action; the parse step resolves the
-- label against the team_members snapshot via the existing
-- OwnerResolver and writes the resolved id here.
--
-- ON DELETE SET NULL mirrors the actions(assignee_id) FK in
-- 007_action_owners.sql: removing a team member nulls the column but
-- keeps the action intact.

ALTER TABLE workstream_actions
  ADD COLUMN assignee_id TEXT REFERENCES team_members(id) ON DELETE SET NULL;

CREATE INDEX idx_ws_actions_assignee ON workstream_actions(assignee_id);

UPDATE meta SET value = '21' WHERE key = 'schema_version';
