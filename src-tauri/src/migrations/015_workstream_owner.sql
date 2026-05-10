ALTER TABLE workstreams ADD COLUMN owner_member_id TEXT REFERENCES team_members(id) ON DELETE SET NULL;
CREATE INDEX idx_workstreams_owner ON workstreams(owner_member_id);

UPDATE meta SET value = '15' WHERE key = 'schema_version';
