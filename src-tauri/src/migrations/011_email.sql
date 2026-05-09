PRAGMA foreign_keys = ON;

CREATE TABLE email_messages (
  id              TEXT PRIMARY KEY,
  connector_id    TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
  external_id     TEXT NOT NULL,
  thread_id       TEXT NOT NULL,
  subject         TEXT NOT NULL,
  from_email      TEXT NOT NULL,
  from_name       TEXT,
  sent_at_ms      INTEGER NOT NULL,
  body_preview    TEXT,
  body_html       TEXT,
  has_attachments INTEGER NOT NULL DEFAULT 0,
  is_read         INTEGER NOT NULL DEFAULT 0,
  raw_etag        TEXT,
  modified_ms     INTEGER NOT NULL
);
CREATE INDEX idx_email_thread ON email_messages(thread_id);
CREATE INDEX idx_email_sent ON email_messages(sent_at_ms);
CREATE INDEX idx_email_connector ON email_messages(connector_id);
CREATE UNIQUE INDEX idx_email_extid ON email_messages(connector_id, external_id);

CREATE TABLE email_recipients (
  message_id     TEXT NOT NULL REFERENCES email_messages(id) ON DELETE CASCADE,
  email          TEXT NOT NULL,
  display_name   TEXT,
  recipient_type TEXT NOT NULL,
  team_member_id TEXT REFERENCES team_members(id) ON DELETE SET NULL,
  PRIMARY KEY (message_id, email, recipient_type)
);
CREATE INDEX idx_email_recip_team ON email_recipients(team_member_id);

UPDATE meta SET value = '11' WHERE key = 'schema_version';
