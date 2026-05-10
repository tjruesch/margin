-- Typed team-member aliases (#87).
--
-- The flat `team_members.aliases` JSON column conflates emails, names, and
-- (incoming) GitHub/Slack handles. The resolver assumed every alias was an
-- email-or-name, so a GitHub username like `heike-mueller` would either
-- leak into email matching or fail to resolve at all.
--
-- This migration introduces a typed pivot keyed by (member_id, kind, value)
-- and backfills existing rows by sniffing for `@` (email) or falling back
-- to name. The original JSON column is dropped — single source of truth.

CREATE TABLE team_member_aliases (
  member_id TEXT NOT NULL REFERENCES team_members(id) ON DELETE CASCADE,
  kind      TEXT NOT NULL,
  value     TEXT NOT NULL,
  PRIMARY KEY (member_id, kind, value)
);
CREATE INDEX idx_alias_kind_value ON team_member_aliases(kind, value);

-- Existing aliases live as a JSON array on `team_members.aliases`. Sniff
-- each entry: anything containing `@` is presumed an email, everything
-- else is a name. Empty strings are filtered.
INSERT INTO team_member_aliases(member_id, kind, value)
SELECT
  tm.id,
  CASE WHEN je.value LIKE '%@%' THEN 'email' ELSE 'name' END,
  je.value
FROM team_members tm, json_each(tm.aliases) je
WHERE je.value <> '';

ALTER TABLE team_members DROP COLUMN aliases;

UPDATE meta SET value = '17' WHERE key = 'schema_version';
