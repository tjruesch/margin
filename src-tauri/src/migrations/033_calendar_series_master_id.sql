-- Capture Microsoft Graph's `seriesMasterId` on each recurring
-- occurrence (#109). Pre-existing rows backfill to NULL (single
-- instance); the next sync pass populates the column for new rows.
-- Namespaced the same way as occurrence ids:
-- {connector_id}::{seriesMasterId}.
--
-- The column lets three consumers collapse occurrences at query time
-- instead of multi-counting:
--   1. Workstream synth prompt — one [E*] line per series, not per occurrence.
--   2. CO_ATTENDED edges — one shared-meeting credit per series.
--   3. Embeddings worker — embed only the earliest occurrence per series.

ALTER TABLE calendar_events ADD COLUMN series_master_id TEXT;

-- Partial index — only recurring events need it. The vast majority of
-- calendar rows are one-off meetings (series_master_id IS NULL); the
-- partial form keeps index size proportional to recurring volume.
CREATE INDEX idx_events_series ON calendar_events(series_master_id)
  WHERE series_master_id IS NOT NULL;

UPDATE meta SET value = '33' WHERE key = 'schema_version';
