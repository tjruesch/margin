-- #49 — re-parse every note so the new owner-extraction step runs on
-- existing actions and populates assignee_id where matches exist.
-- The actions.assignee_id column already exists (added in 006).
UPDATE notes SET body_size = -1;

UPDATE meta SET value = '7' WHERE key = 'schema_version';
