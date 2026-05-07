ALTER TABLE actions ADD COLUMN due_ms INTEGER;
ALTER TABLE actions ADD COLUMN reminder_sent_ms INTEGER;
CREATE INDEX idx_actions_due ON actions(due_ms) WHERE due_ms IS NOT NULL;
UPDATE meta SET value = '5' WHERE key = 'schema_version';
UPDATE notes SET body_size = -1;
