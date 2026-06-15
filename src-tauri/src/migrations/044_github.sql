-- GitHub contributions connector (#165).
--
-- One row per contribution the user authored, pulled from the GitHub
-- Search API by the `github` connector:
--   - kind = 'pr'     → a pull request (state 'merged' = a delivered
--                        feature; 'open' / 'closed' otherwise)
--   - kind = 'commit'  → a standalone commit (state 'committed') —
--                        treated as work-in-progress in the changelog
--
-- Accumulate-only, mirroring email (#69): the connector re-scans a
-- rolling 30-day window each poll and upserts; rows aging out of the
-- window are NOT deleted so the changelog grows over time. Idempotent
-- re-syncs key on (connector_id, external_id) — a PR flipping from open
-- to merged updates the same row.

PRAGMA foreign_keys = ON;

CREATE TABLE github_contributions (
  id                TEXT PRIMARY KEY,
  connector_id      TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
  external_id       TEXT NOT NULL,
  kind              TEXT NOT NULL,            -- 'pr' | 'commit'
  state             TEXT NOT NULL,            -- 'merged' | 'open' | 'closed' | 'committed'
  title             TEXT NOT NULL,
  body              TEXT,
  repo              TEXT NOT NULL,            -- 'owner/name'
  url               TEXT NOT NULL,            -- html_url on github.com
  author_login      TEXT NOT NULL,
  author_avatar_url TEXT,
  created_at_ms     INTEGER NOT NULL,         -- PR created_at / commit author date
  merged_at_ms      INTEGER,                  -- PR merge time; NULL for unmerged + commits
  modified_ms       INTEGER NOT NULL          -- PR updated_at / commit date; dirty-checks embeddings
);
CREATE INDEX idx_ghc_connector ON github_contributions(connector_id);
CREATE INDEX idx_ghc_kind ON github_contributions(kind);
CREATE INDEX idx_ghc_created ON github_contributions(created_at_ms DESC);
CREATE UNIQUE INDEX idx_ghc_extid ON github_contributions(connector_id, external_id);

UPDATE meta SET value = '44' WHERE key = 'schema_version';
