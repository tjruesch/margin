PRAGMA foreign_keys = ON;

-- Phase 1.4 of milestone #5 — flag that gates the one-time backfill
-- of existing reconciled notes' inline `## Action items` blocks into
-- reconcile-origin rows (#146). The boot sweep in
-- `actions_migration::run_if_pending` flips this to '1' once the
-- migration has run successfully. Manual re-runs via Settings ignore
-- the flag (the underlying migration is idempotent).
INSERT OR IGNORE INTO meta(key, value)
  VALUES ('actions_migration_v1_completed', '0');

UPDATE meta SET value = '40' WHERE key = 'schema_version';
