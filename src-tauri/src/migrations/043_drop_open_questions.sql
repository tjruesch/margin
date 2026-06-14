DROP TABLE IF EXISTS note_open_questions;
DELETE FROM meta WHERE key = 'questions_backfill_done';

UPDATE meta SET value = '43' WHERE key = 'schema_version';
