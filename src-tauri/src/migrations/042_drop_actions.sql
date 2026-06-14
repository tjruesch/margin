DROP TABLE IF EXISTS action_deletions;
DROP TABLE IF EXISTS dismissed_action_sources;
DROP TABLE IF EXISTS actions;

UPDATE meta SET value = '42' WHERE key = 'schema_version';
