-- Standalone todo list (#166).
--
-- A fresh, self-contained to-do feature — NOT the note-derived "actions"
-- system that was removed in #162. Items are created directly (on the
-- Todos page or via the command palette, by text or voice), carry an
-- optional due date, and fire an OS notification when due.
--
-- `notified_ms` is stamped when the due reminder fires so the ticker
-- never double-notifies. `source` records where the item came from
-- ('page' | 'palette' | 'voice') for light analytics / debugging.

PRAGMA foreign_keys = ON;

CREATE TABLE todos (
  id           TEXT PRIMARY KEY,
  text         TEXT NOT NULL,
  done         INTEGER NOT NULL DEFAULT 0,
  due_ms       INTEGER,
  created_ms   INTEGER NOT NULL,
  modified_ms  INTEGER NOT NULL,
  completed_ms INTEGER,
  notified_ms  INTEGER,
  source       TEXT NOT NULL DEFAULT 'page'
);
CREATE INDEX idx_todos_done ON todos(done);
CREATE INDEX idx_todos_due ON todos(due_ms);

UPDATE meta SET value = '46' WHERE key = 'schema_version';
