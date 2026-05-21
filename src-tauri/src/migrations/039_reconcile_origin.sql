PRAGMA foreign_keys = ON;

-- Phase 1.1 of milestone #5 — declare 'reconcile' as a known action
-- origin_kind. No structural change: the existing `actions` schema
-- already carries `origin_kind TEXT NOT NULL` (mig 025) and
-- `origin_note_id TEXT REFERENCES notes(id) ON DELETE CASCADE`
-- (mig 026), with no CHECK constraint on origin_kind.
--
-- A reconcile-origin row, once #144 lands, has shape:
--   origin_kind        = 'reconcile'
--   origin_note_id     = <meeting note bundle id>   -- so the sidebar can query
--   origin_line        = NULL                        -- no source line anymore
--   origin_synth_kind  = NULL
--   origin_synth_id    = NULL
--   workstream_id      = NULL  (or set by user)
--
-- Bumping schema_version is the only DB change here. This pins the
-- version below which 'reconcile' rows do not exist, which the
-- one-time backfill in #146 keys off.

UPDATE meta SET value = '39' WHERE key = 'schema_version';
