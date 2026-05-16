-- Prune legacy profile.md path (#117).
--
-- Before #107/#112, profile_md_path pointed at an on-disk markdown
-- file under ~/.margin/team/<id>/profile.md that held a free-form
-- per-member profile. After the DB-backed profile snapshots (#107)
-- and DB-backed notes (#112) shipped, both the column and the files
-- became orphans — nothing reads them. SQLite >= 3.35 supports plain
-- DROP COLUMN here (the column has no FK, no unique index, no CHECK).
ALTER TABLE team_members DROP COLUMN profile_md_path;

-- One-shot boot sweep deletes the orphan files on the user's disk.
-- Gated by this flag so it only runs once per install; the flag flips
-- to '1' after the first successful sweep (see team::purge_profile_md_if_pending).
INSERT OR IGNORE INTO meta(key, value) VALUES ('profile_md_purged', '0');

UPDATE meta SET value = '32' WHERE key = 'schema_version';
