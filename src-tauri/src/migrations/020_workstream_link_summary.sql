-- Add an AI-generated summary column to workstream_links.
--
-- After a link is added via the paste-only composer, a background
-- task scrapes the URL via Firecrawl and asks Claude Haiku for a
-- 2-3 sentence summary. The chip renders the summary as a second
-- italic line; the AI-ask `read_workstream` output appends it after
-- the markdown link.
--
-- Nullable: NULL means "not yet summarized" or "summarization failed
-- silently" (no API key, network error, model error). The UI hides
-- the second line when NULL and there's no retry — the user can
-- remove + re-add to retry, or we'll add a manual regenerate later.

ALTER TABLE workstream_links ADD COLUMN summary TEXT;

UPDATE meta SET value = '20' WHERE key = 'schema_version';
