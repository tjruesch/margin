-- Versioned per-person profile snapshots (#107).
--
-- The latest row per `person_id` (by `computed_ms`) is "the current
-- profile"; older rows are kept for history. `dirty_since_ms` is a
-- reserved column for the worker to stamp a "computing now" marker;
-- it is NULL in the steady state. Dirtiness for tick-time selection
-- is derived from the `events` table, not from this column — keeps
-- the event emission path cheap and avoids needing to thread a
-- write through every emit() call site.
--
-- `source_hash` is a sha256 over the prompt inputs (team_member
-- row + edges fingerprint + event window fingerprint + retrieval
-- seeds). When the inputs are unchanged the worker can skip the
-- Anthropic call entirely — "structural cache hit".

PRAGMA foreign_keys = OFF;

CREATE TABLE profile_snapshots (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  person_id       TEXT NOT NULL REFERENCES team_members(id) ON DELETE CASCADE,
  computed_ms     INTEGER NOT NULL,
  body_json       TEXT NOT NULL,
  dirty_since_ms  INTEGER,
  source_hash     TEXT NOT NULL
);
CREATE INDEX idx_profile_person_time ON profile_snapshots(person_id, computed_ms DESC);
CREATE INDEX idx_profile_dirty       ON profile_snapshots(dirty_since_ms)
  WHERE dirty_since_ms IS NOT NULL;

PRAGMA foreign_keys = ON;

UPDATE meta SET value = '28' WHERE key = 'schema_version';
