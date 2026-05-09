PRAGMA foreign_keys = ON;

CREATE TABLE calendar_events (
  id              TEXT PRIMARY KEY,
  connector_id    TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
  external_id     TEXT NOT NULL,
  title           TEXT NOT NULL,
  start_ms        INTEGER NOT NULL,
  end_ms          INTEGER NOT NULL,
  all_day         INTEGER NOT NULL DEFAULT 0,
  location        TEXT,
  description     TEXT,
  source_calendar TEXT,
  status          TEXT,
  raw_etag        TEXT,
  modified_ms     INTEGER NOT NULL
);
CREATE INDEX idx_events_start ON calendar_events(start_ms);
CREATE INDEX idx_events_connector ON calendar_events(connector_id);
CREATE UNIQUE INDEX idx_events_extid ON calendar_events(connector_id, external_id);

CREATE TABLE calendar_attendees (
  event_id        TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
  email           TEXT NOT NULL,
  display_name    TEXT,
  response_status TEXT,
  is_self         INTEGER NOT NULL DEFAULT 0,
  is_organizer    INTEGER NOT NULL DEFAULT 0,
  team_member_id  TEXT REFERENCES team_members(id) ON DELETE SET NULL,
  PRIMARY KEY (event_id, email)
);
CREATE INDEX idx_attendees_team_member ON calendar_attendees(team_member_id);

UPDATE meta SET value = '9' WHERE key = 'schema_version';
