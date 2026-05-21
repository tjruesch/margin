PRAGMA foreign_keys = ON;

-- Phase 2.1 of milestone #5 — universal deletion log (#147).
--
-- Replaces the synth-only `dismissed_action_sources` model with a
-- per-row snapshot of every action that disappears, regardless of
-- origin. Read by the reconcile prompt (#148), the profile worker
-- (#149) and the workstream synthesizer (#150) so all three
-- producers learn from the user's rejections.
--
-- Snapshot fields are kept verbatim rather than as FKs: by the time
-- a reader cares about a deletion the underlying `actions` row is
-- gone. Subject + assignee ids may dangle if a team member is later
-- deleted; that's acceptable — the text + origin metadata is the
-- learning signal, not the FK joinability.
CREATE TABLE action_deletions (
  id                      INTEGER PRIMARY KEY AUTOINCREMENT,
  deleted_ms              INTEGER NOT NULL,
  origin_kind             TEXT NOT NULL,
  origin_synth_kind       TEXT,
  origin_synth_id         TEXT,
  origin_note_id          TEXT,
  subject_member_id       TEXT,
  assignee_id             TEXT,
  text                    TEXT NOT NULL,
  -- For reconcile-origin rows tied to a recurring meeting, the master
  -- id of the series. NULL for one-offs and non-reconcile origins.
  -- Lets #148 suppress per-series rather than per-occurrence.
  source_series_master_id TEXT,
  -- Distinguishes user intent from worker sweeps so #148/#149/#150
  -- can gate on `cause IN ('user_delete','user_dismiss')` and ignore
  -- auto_resolved omissions (which are weak signal, not rejection).
  cause                   TEXT NOT NULL DEFAULT 'user_delete'
);

CREATE INDEX idx_action_deletions_subject
  ON action_deletions(subject_member_id, deleted_ms DESC)
  WHERE subject_member_id IS NOT NULL;
CREATE INDEX idx_action_deletions_series
  ON action_deletions(source_series_master_id, deleted_ms DESC)
  WHERE source_series_master_id IS NOT NULL;
CREATE INDEX idx_action_deletions_recency
  ON action_deletions(deleted_ms DESC);

UPDATE meta SET value = '41' WHERE key = 'schema_version';
