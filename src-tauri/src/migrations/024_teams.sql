-- Microsoft Teams messages connector (#105).
--
-- Mirrors the email_messages + email_recipients shape from #011. One
-- row per chat message, plus a chat_members pivot for the people in
-- each chat (used for external_participants derivation + person→event
-- ATTENDED-style edges via the chat membership).
--
-- v1 covers oneOnOne and group chats only — channel messages are
-- excluded at sync time to keep the corpus / embedding quota bounded.
-- The schema does NOT enforce that constraint via CHECK; future opt-in
-- channel sync would just start writing rows with chat_kind='channel'.

PRAGMA foreign_keys = ON;

CREATE TABLE teams_messages (
  id              TEXT PRIMARY KEY,
  connector_id    TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
  external_id     TEXT NOT NULL,
  chat_id         TEXT NOT NULL,
  chat_kind       TEXT NOT NULL,
  chat_topic      TEXT,
  sent_at_ms      INTEGER NOT NULL,
  from_aad_id     TEXT,
  from_email      TEXT,
  from_name       TEXT,
  body_html       TEXT,
  body_preview    TEXT,
  reply_to_id     TEXT,
  modified_ms     INTEGER NOT NULL,
  raw_etag        TEXT
);
CREATE INDEX idx_teams_messages_chat ON teams_messages(chat_id, sent_at_ms DESC);
CREATE INDEX idx_teams_messages_sent ON teams_messages(sent_at_ms DESC);
CREATE INDEX idx_teams_messages_from ON teams_messages(from_email);
CREATE INDEX idx_teams_messages_connector ON teams_messages(connector_id);

CREATE TABLE teams_chat_members (
  chat_id        TEXT NOT NULL,
  aad_id         TEXT NOT NULL,
  email          TEXT,
  display_name   TEXT,
  team_member_id TEXT REFERENCES team_members(id) ON DELETE SET NULL,
  is_self        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (chat_id, aad_id)
);
CREATE INDEX idx_teams_chat_members_team ON teams_chat_members(team_member_id);
CREATE INDEX idx_teams_chat_members_email ON teams_chat_members(email);

UPDATE meta SET value = '24' WHERE key = 'schema_version';
