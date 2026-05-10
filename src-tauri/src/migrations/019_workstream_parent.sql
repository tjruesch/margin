-- Hierarchical workstreams (#89).
--
-- A flat 2-level hierarchy: a workstream may have a parent (the umbrella
-- effort) but parents themselves cannot have parents. The 2-level cap is
-- enforced in code (see `validate_proposed_parent` in persist.rs); the
-- schema only carries the FK so referential integrity holds.
--
-- ON DELETE SET NULL — deleting a parent leaves its children as
-- standalone workstreams rather than cascade-deleting them. Matches the
-- issue's "cascading archive out of scope" stance: each child should be
-- a separate decision when its umbrella goes away.
--
-- The new column is NULL on every existing row, so no backfill needed.

ALTER TABLE workstreams ADD COLUMN parent_workstream_id TEXT
  REFERENCES workstreams(id) ON DELETE SET NULL;
CREATE INDEX idx_workstreams_parent ON workstreams(parent_workstream_id);

UPDATE meta SET value = '19' WHERE key = 'schema_version';
