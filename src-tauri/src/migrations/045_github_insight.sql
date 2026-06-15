-- AI changelog insight per contribution (#165 follow-up).
--
-- Generated lazily when the user opens a PR's detail view: a plain
-- summary of what was implemented, plus an optional high-bar
-- "worth writing about" highlight (a technical detail / learning /
-- story that could anchor a blog or LinkedIn post). `ai_highlight` is
-- JSON {"angle","content"} when present, NULL when nothing cleared the
-- bar. `ai_generated_ms` NULL means "not generated yet" (vs generated
-- with no highlight).

PRAGMA foreign_keys = ON;

ALTER TABLE github_contributions ADD COLUMN ai_summary TEXT;
ALTER TABLE github_contributions ADD COLUMN ai_highlight TEXT;
ALTER TABLE github_contributions ADD COLUMN ai_generated_ms INTEGER;

UPDATE meta SET value = '45' WHERE key = 'schema_version';
