//! Persistent chat conversations for the full-page Chat surface
//! and the cmd+K palette. Schema in migration 035.
//!
//! The model itself is still driven by `ask.rs::ask_notes_start`,
//! which is stateless — this module just owns the conversation +
//! message rows so the transcript survives popover-close and app
//! restart, and so both surfaces render the same history.
//!
//! Design notes:
//! - **Single active conversation per user.** `archived_at_ms IS NULL`
//!   selects it; clear-chat stamps the timestamp and lazily inserts
//!   a fresh row. Multi-thread (a la ChatGPT) is one new IPC + a
//!   sidebar list away — schema already supports it.
//! - **Sources / tool_calls stored as JSON.** They're render-only
//!   metadata the model emitted; pivot tables would be over-engineered
//!   for shapes the UI alone consumes.
//! - **No FK on `conversation_id` from outside this module.** Cascade
//!   delete on the header drops messages cleanly via the schema's
//!   `ON DELETE CASCADE`.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::sync::Mutex;

use crate::events::current_unix_ms;

#[derive(Serialize, Clone, Debug)]
pub struct ChatConversation {
    pub id: String,
    pub title: Option<String>,
    pub created_ms: i64,
    pub last_message_ms: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChatMessage {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub text: String,
    pub created_ms: i64,
    pub turn_id: Option<String>,
    /// Decoded `sources_json`; `[]` when the column is NULL (always
    /// the case on user-role rows). The shape mirrors `ask.rs::AskSource`
    /// — the frontend types it as `ChatSource[]` and renders the same
    /// chip strip the palette already shows.
    pub sources: serde_json::Value,
    /// Decoded `tool_calls_json`; `[]` when NULL. Shape:
    /// `{name, target_kind, target_label, target_title, ok}`.
    pub tool_calls: serde_json::Value,
}

fn new_conversation_id() -> String {
    format!("conv_{}", uuid::Uuid::new_v4())
}

fn new_message_id() -> String {
    format!("msg_{}", uuid::Uuid::new_v4())
}

/// Insert a fresh conversation row. Returns the hydrated struct.
fn insert_conversation(conn: &Connection, now_ms: i64) -> rusqlite::Result<ChatConversation> {
    let id = new_conversation_id();
    conn.execute(
        "INSERT INTO chat_conversations(id, title, created_ms, last_message_ms) \
         VALUES (?1, NULL, ?2, ?2)",
        params![id, now_ms],
    )?;
    Ok(ChatConversation {
        id,
        title: None,
        created_ms: now_ms,
        last_message_ms: now_ms,
    })
}

/// Most-recent active conversation. Lazy-creates if none exists so the
/// frontend always has an id to write against.
pub fn get_active(conn: &Connection, now_ms: i64) -> rusqlite::Result<ChatConversation> {
    let row: Option<ChatConversation> = conn
        .query_row(
            "SELECT id, title, created_ms, last_message_ms \
               FROM chat_conversations \
              WHERE archived_at_ms IS NULL \
              ORDER BY last_message_ms DESC \
              LIMIT 1",
            [],
            |r| {
                Ok(ChatConversation {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    created_ms: r.get(2)?,
                    last_message_ms: r.get(3)?,
                })
            },
        )
        .optional()?;
    if let Some(c) = row {
        Ok(c)
    } else {
        insert_conversation(conn, now_ms)
    }
}

pub fn append_message(
    conn: &Connection,
    conversation_id: &str,
    role: &str,
    text: &str,
    sources: Option<&serde_json::Value>,
    tool_calls: Option<&serde_json::Value>,
    turn_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<ChatMessage> {
    if role != "user" && role != "assistant" {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "invalid role: {role}"
        )));
    }
    let id = new_message_id();
    let sources_json = sources.map(|v| v.to_string());
    let tool_calls_json = tool_calls.map(|v| v.to_string());
    conn.execute(
        "INSERT INTO chat_messages(id, conversation_id, role, text, \
                                   sources_json, tool_calls_json, turn_id, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, conversation_id, role, text, sources_json, tool_calls_json, turn_id, now_ms],
    )?;
    conn.execute(
        "UPDATE chat_conversations SET last_message_ms = ?2 WHERE id = ?1",
        params![conversation_id, now_ms],
    )?;
    Ok(ChatMessage {
        id,
        conversation_id: conversation_id.to_string(),
        role: role.to_string(),
        text: text.to_string(),
        created_ms: now_ms,
        turn_id: turn_id.map(|s| s.to_string()),
        sources: sources.cloned().unwrap_or_else(|| serde_json::json!([])),
        tool_calls: tool_calls.cloned().unwrap_or_else(|| serde_json::json!([])),
    })
}

pub fn list_messages(
    conn: &Connection,
    conversation_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<ChatMessage>> {
    // Strategy: pull the newest `limit` rows DESC so we cap the most
    // recent slice, then reverse for chronological render order. ASC
    // with no LIMIT would force a full scan when conversations grow.
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, role, text, sources_json, \
                tool_calls_json, turn_id, created_ms \
           FROM chat_messages \
          WHERE conversation_id = ?1 \
          ORDER BY created_ms DESC \
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![conversation_id, limit as i64], |r| {
        let sources_json: Option<String> = r.get(4)?;
        let tool_calls_json: Option<String> = r.get(5)?;
        Ok(ChatMessage {
            id: r.get(0)?,
            conversation_id: r.get(1)?,
            role: r.get(2)?,
            text: r.get(3)?,
            sources: sources_json
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| serde_json::json!([])),
            tool_calls: tool_calls_json
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| serde_json::json!([])),
            turn_id: r.get(6)?,
            created_ms: r.get(7)?,
        })
    })?;
    let mut out: Vec<ChatMessage> = rows.filter_map(Result::ok).collect();
    out.reverse();
    Ok(out)
}

/// Archive the current active conversation (if any) and lazily insert
/// a fresh one. One transaction so a partial state can't leave the
/// schema with two active rows.
pub fn clear_active(conn: &mut Connection, now_ms: i64) -> rusqlite::Result<ChatConversation> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE chat_conversations \
            SET archived_at_ms = ?1 \
          WHERE archived_at_ms IS NULL",
        params![now_ms],
    )?;
    let fresh = {
        let id = new_conversation_id();
        tx.execute(
            "INSERT INTO chat_conversations(id, title, created_ms, last_message_ms) \
             VALUES (?1, NULL, ?2, ?2)",
            params![id, now_ms],
        )?;
        ChatConversation {
            id,
            title: None,
            created_ms: now_ms,
            last_message_ms: now_ms,
        }
    };
    tx.commit()?;
    Ok(fresh)
}

// ---------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------

#[tauri::command]
pub fn get_active_conversation(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<ChatConversation, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    get_active(&c, current_unix_ms()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_chat_messages(
    conversation_id: String,
    limit: Option<usize>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<ChatMessage>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    list_messages(&c, &conversation_id, limit.unwrap_or(200)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn append_chat_message(
    conversation_id: String,
    role: String,
    text: String,
    sources: Option<serde_json::Value>,
    tool_calls: Option<serde_json::Value>,
    turn_id: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<ChatMessage, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    append_message(
        &c,
        &conversation_id,
        &role,
        &text,
        sources.as_ref(),
        tool_calls.as_ref(),
        turn_id.as_deref(),
        current_unix_ms(),
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clear_active_conversation(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<ChatConversation, String> {
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    clear_active(&mut c, current_unix_ms()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Seed meta + chat_conversations / chat_messages directly —
        // migration 035 expects `meta` to exist (it's created in 001).
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); \
             INSERT INTO meta(key, value) VALUES ('schema_version', '34');",
        )
        .unwrap();
        conn.execute_batch(include_str!("migrations/035_chat_conversations.sql"))
            .unwrap();
        conn
    }

    #[test]
    fn get_active_creates_when_none_exists() {
        let conn = open_test_db();
        let c = get_active(&conn, 1_000).unwrap();
        assert!(c.id.starts_with("conv_"));
        assert_eq!(c.created_ms, 1_000);
        assert_eq!(c.last_message_ms, 1_000);
        // Second call returns the same row, not a fresh insert.
        let c2 = get_active(&conn, 9_999).unwrap();
        assert_eq!(c2.id, c.id);
    }

    #[test]
    fn get_active_returns_most_recent_active() {
        let conn = open_test_db();
        let a = get_active(&conn, 1_000).unwrap();
        // Direct INSERT another active row at a later timestamp.
        conn.execute(
            "INSERT INTO chat_conversations(id, title, created_ms, last_message_ms) \
             VALUES (?1, NULL, ?2, ?2)",
            params!["conv_newer", 2_000_i64],
        )
        .unwrap();
        let active = get_active(&conn, 9_999).unwrap();
        assert_eq!(active.id, "conv_newer");
        assert_ne!(active.id, a.id);
    }

    #[test]
    fn get_active_skips_archived() {
        let conn = open_test_db();
        let c = get_active(&conn, 1_000).unwrap();
        conn.execute(
            "UPDATE chat_conversations SET archived_at_ms = ?1 WHERE id = ?2",
            params![5_000_i64, c.id],
        )
        .unwrap();
        // No active rows → lazily creates a new one.
        let next = get_active(&conn, 6_000).unwrap();
        assert_ne!(next.id, c.id);
        assert_eq!(next.created_ms, 6_000);
    }

    #[test]
    fn append_user_then_assistant_round_trips_with_sources() {
        let conn = open_test_db();
        let conv = get_active(&conn, 1_000).unwrap();
        let user = append_message(
            &conn,
            &conv.id,
            "user",
            "What did I work on?",
            None,
            None,
            None,
            2_000,
        )
        .unwrap();
        let sources = serde_json::json!([{
            "kind": "note", "label": "3", "title": "Standup", "modified_ms": 100
        }]);
        let asst = append_message(
            &conn,
            &conv.id,
            "assistant",
            "You worked on the auth refactor.",
            Some(&sources),
            None,
            Some("turn_abc"),
            3_000,
        )
        .unwrap();
        let listed = list_messages(&conn, &conv.id, 10).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, user.id);
        assert_eq!(listed[0].role, "user");
        assert_eq!(listed[1].id, asst.id);
        assert_eq!(listed[1].role, "assistant");
        assert_eq!(listed[1].turn_id.as_deref(), Some("turn_abc"));
        assert_eq!(
            listed[1].sources[0]["title"].as_str(),
            Some("Standup")
        );
        // last_message_ms tracks the most recent append.
        let bumped: i64 = conn
            .query_row(
                "SELECT last_message_ms FROM chat_conversations WHERE id = ?1",
                params![conv.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bumped, 3_000);
    }

    #[test]
    fn append_rejects_invalid_role() {
        let conn = open_test_db();
        let conv = get_active(&conn, 1_000).unwrap();
        let err =
            append_message(&conn, &conv.id, "system", "x", None, None, None, 2_000).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("invalid role"), "got: {msg}");
    }

    #[test]
    fn clear_archives_current_and_creates_fresh() {
        let mut conn = open_test_db();
        let original = get_active(&conn, 1_000).unwrap();
        append_message(
            &conn,
            &original.id,
            "user",
            "first",
            None,
            None,
            None,
            2_000,
        )
        .unwrap();
        let fresh = clear_active(&mut conn, 5_000).unwrap();
        assert_ne!(fresh.id, original.id);
        // Original tombstoned.
        let arch: i64 = conn
            .query_row(
                "SELECT archived_at_ms FROM chat_conversations WHERE id = ?1",
                params![original.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(arch, 5_000);
        // get_active returns the fresh row.
        let active = get_active(&conn, 9_999).unwrap();
        assert_eq!(active.id, fresh.id);
        // Old messages still queryable on the archived id — useful for
        // a future "browse archived conversations" surface.
        let old = list_messages(&conn, &original.id, 10).unwrap();
        assert_eq!(old.len(), 1);
        // Fresh conversation has no messages.
        let new_msgs = list_messages(&conn, &fresh.id, 10).unwrap();
        assert!(new_msgs.is_empty());
    }

    #[test]
    fn list_chat_messages_orders_by_created_ms_asc_with_limit() {
        let conn = open_test_db();
        let conv = get_active(&conn, 1_000).unwrap();
        for (i, ts) in [10, 20, 30, 40, 50].iter().enumerate() {
            append_message(
                &conn,
                &conv.id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("msg {i}"),
                None,
                None,
                None,
                *ts,
            )
            .unwrap();
        }
        let all = list_messages(&conn, &conv.id, 100).unwrap();
        let ts: Vec<i64> = all.iter().map(|m| m.created_ms).collect();
        assert_eq!(ts, vec![10, 20, 30, 40, 50]);
        // Limit retains the newest slice and still renders chronologically.
        let last3 = list_messages(&conn, &conv.id, 3).unwrap();
        let ts3: Vec<i64> = last3.iter().map(|m| m.created_ms).collect();
        assert_eq!(ts3, vec![30, 40, 50]);
    }

    #[test]
    fn cascade_delete_on_conversation_drop_removes_messages() {
        let conn = open_test_db();
        // Foreign keys must be ON for ON DELETE CASCADE to fire under
        // sqlite. Migration's `PRAGMA foreign_keys = ON;` is per-conn,
        // so set it again on the test connection.
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let conv = get_active(&conn, 1_000).unwrap();
        append_message(&conn, &conv.id, "user", "x", None, None, None, 2_000).unwrap();
        conn.execute(
            "DELETE FROM chat_conversations WHERE id = ?1",
            params![conv.id],
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_messages WHERE conversation_id = ?1",
                params![conv.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
