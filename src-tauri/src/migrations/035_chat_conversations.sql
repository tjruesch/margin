PRAGMA foreign_keys = ON;

CREATE TABLE chat_conversations (
  id              TEXT PRIMARY KEY,
  title           TEXT,
  created_ms      INTEGER NOT NULL,
  last_message_ms INTEGER NOT NULL,
  archived_at_ms  INTEGER
);
CREATE INDEX idx_chat_conversations_active
  ON chat_conversations(archived_at_ms, last_message_ms DESC);

CREATE TABLE chat_messages (
  id              TEXT PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES chat_conversations(id) ON DELETE CASCADE,
  role            TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
  text            TEXT NOT NULL,
  sources_json    TEXT,
  tool_calls_json TEXT,
  turn_id         TEXT,
  created_ms      INTEGER NOT NULL
);
CREATE INDEX idx_chat_messages_conv_time
  ON chat_messages(conversation_id, created_ms ASC);

UPDATE meta SET value = '35' WHERE key = 'schema_version';
