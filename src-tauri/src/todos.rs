//! Standalone todo list (#166).
//!
//! A fresh, self-contained to-do feature — deliberately NOT the
//! note-derived "actions" system removed in #162. Items are created
//! directly (on the Todos page or via the command palette, by text or
//! voice), carry an optional due date, and a background ticker
//! (`reminders.rs`) fires an OS notification when one comes due.
//!
//! All write commands emit a `todos-changed` event so the page, the
//! palette, and the notification bell stay in sync without polling.

use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
pub struct Todo {
    pub id: String,
    pub text: String,
    pub done: bool,
    pub due_ms: Option<i64>,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub completed_ms: Option<i64>,
}

const SELECT_COLS: &str =
    "id, text, done, due_ms, created_ms, modified_ms, completed_ms";

fn row_to_todo(r: &rusqlite::Row<'_>) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: r.get(0)?,
        text: r.get(1)?,
        done: r.get::<_, i64>(2)? != 0,
        due_ms: r.get(3)?,
        created_ms: r.get(4)?,
        modified_ms: r.get(5)?,
        completed_ms: r.get(6)?,
    })
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- DB layer (also reused by the reminder ticker) ---------------------

/// List todos for `scope`: "active" (incomplete), "completed", or "all".
/// Active items sort by due date (soonest first, undated last), then
/// newest-created; completed items sort by completion time, newest first.
pub fn list(conn: &Connection, scope: &str) -> rusqlite::Result<Vec<Todo>> {
    let sql = match scope {
        "completed" => format!(
            "SELECT {SELECT_COLS} FROM todos WHERE done = 1 \
             ORDER BY completed_ms DESC LIMIT 500"
        ),
        "all" => format!(
            "SELECT {SELECT_COLS} FROM todos \
             ORDER BY done ASC, (due_ms IS NULL) ASC, due_ms ASC, created_ms DESC"
        ),
        // default "active"
        _ => format!(
            "SELECT {SELECT_COLS} FROM todos WHERE done = 0 \
             ORDER BY (due_ms IS NULL) ASC, due_ms ASC, created_ms DESC"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_todo)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn insert(
    conn: &Connection,
    text: &str,
    due_ms: Option<i64>,
    source: &str,
) -> rusqlite::Result<Todo> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    conn.execute(
        "INSERT INTO todos(id, text, done, due_ms, created_ms, modified_ms, completed_ms, notified_ms, source) \
         VALUES (?1, ?2, 0, ?3, ?4, ?4, NULL, NULL, ?5)",
        params![id, text, due_ms, now, source],
    )?;
    Ok(Todo {
        id,
        text: text.to_string(),
        done: false,
        due_ms,
        created_ms: now,
        modified_ms: now,
        completed_ms: None,
    })
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<Todo>> {
    let sql = format!("SELECT {SELECT_COLS} FROM todos WHERE id = ?1");
    conn.query_row(&sql, params![id], row_to_todo).optional()
}

/// Update the editable fields. Moving the due date clears `notified_ms`
/// so a rescheduled item can notify again at its new time.
pub fn update(
    conn: &Connection,
    id: &str,
    text: &str,
    due_ms: Option<i64>,
) -> rusqlite::Result<Option<Todo>> {
    conn.execute(
        "UPDATE todos SET text = ?2, due_ms = ?3, modified_ms = ?4, \
            notified_ms = CASE WHEN COALESCE(due_ms, -1) IS ?3 THEN notified_ms ELSE NULL END \
         WHERE id = ?1",
        params![id, text, due_ms, now_ms()],
    )?;
    get(conn, id)
}

pub fn set_done(conn: &Connection, id: &str, done: bool) -> rusqlite::Result<Option<Todo>> {
    let now = now_ms();
    conn.execute(
        "UPDATE todos SET done = ?2, completed_ms = ?3, modified_ms = ?4 WHERE id = ?1",
        params![id, done as i64, if done { Some(now) } else { None }, now],
    )?;
    get(conn, id)
}

pub fn delete(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM todos WHERE id = ?1", params![id])?;
    Ok(())
}

/// Incomplete, dated todos that are due now and haven't been notified.
/// Drives the reminder ticker.
pub fn due_unnotified(conn: &Connection, now: i64) -> rusqlite::Result<Vec<Todo>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM todos \
         WHERE done = 0 AND due_ms IS NOT NULL AND due_ms <= ?1 AND notified_ms IS NULL \
         ORDER BY due_ms ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now], row_to_todo)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn mark_notified(conn: &Connection, id: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE todos SET notified_ms = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Count of incomplete todos that are overdue (past due as of `now`).
/// Used for the sidebar / bell badge.
pub fn overdue_count(conn: &Connection, now: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM todos WHERE done = 0 AND due_ms IS NOT NULL AND due_ms <= ?1",
        params![now],
        |r| r.get(0),
    )
}

// ----- Tauri commands -----------------------------------------------------

#[tauri::command]
pub fn list_todos(
    scope: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<Todo>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    list(&c, scope.as_deref().unwrap_or("active")).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_todo(
    text: String,
    due_ms: Option<i64>,
    source: Option<String>,
    app: AppHandle,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Todo, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("Todo text is empty.".to_string());
    }
    let todo = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        insert(&c, &text, due_ms, source.as_deref().unwrap_or("page")).map_err(|e| e.to_string())?
    };
    let _ = app.emit("todos-changed", ());
    Ok(todo)
}

#[tauri::command]
pub fn update_todo(
    id: String,
    text: String,
    due_ms: Option<i64>,
    app: AppHandle,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<Todo>, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("Todo text is empty.".to_string());
    }
    let todo = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        update(&c, &id, &text, due_ms).map_err(|e| e.to_string())?
    };
    let _ = app.emit("todos-changed", ());
    Ok(todo)
}

#[tauri::command]
pub fn set_todo_done(
    id: String,
    done: bool,
    app: AppHandle,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<Todo>, String> {
    let todo = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        set_done(&c, &id, done).map_err(|e| e.to_string())?
    };
    let _ = app.emit("todos-changed", ());
    Ok(todo)
}

#[tauri::command]
pub fn delete_todo(
    id: String,
    app: AppHandle,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        delete(&c, &id).map_err(|e| e.to_string())?;
    }
    let _ = app.emit("todos-changed", ());
    Ok(())
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_list_and_complete() {
        let conn = open_db();
        let a = insert(&conn, "buy milk", None, "page").unwrap();
        insert(&conn, "ship release", Some(1_000), "palette").unwrap();

        let active = list(&conn, "active").unwrap();
        assert_eq!(active.len(), 2);
        // Dated item sorts before the undated one.
        assert_eq!(active[0].text, "ship release");
        assert_eq!(active[1].text, "buy milk");

        set_done(&conn, &a.id, true).unwrap();
        assert_eq!(list(&conn, "active").unwrap().len(), 1);
        let done = list(&conn, "completed").unwrap();
        assert_eq!(done.len(), 1);
        assert!(done[0].completed_ms.is_some());
    }

    #[test]
    fn due_notification_lifecycle() {
        let conn = open_db();
        let t = insert(&conn, "call dentist", Some(500), "page").unwrap();
        // Not yet due at t=400.
        assert!(due_unnotified(&conn, 400).unwrap().is_empty());
        // Due at t=600.
        let due = due_unnotified(&conn, 600).unwrap();
        assert_eq!(due.len(), 1);
        mark_notified(&conn, &t.id, 600).unwrap();
        // Won't fire again.
        assert!(due_unnotified(&conn, 999).unwrap().is_empty());
    }

    #[test]
    fn rescheduling_due_clears_notified() {
        let conn = open_db();
        let t = insert(&conn, "x", Some(500), "page").unwrap();
        mark_notified(&conn, &t.id, 600).unwrap();
        assert!(due_unnotified(&conn, 999).unwrap().is_empty());
        // Move the due date out — notified_ms should clear so it can
        // fire again at the new time.
        update(&conn, &t.id, "x", Some(5_000)).unwrap();
        assert!(due_unnotified(&conn, 6_000).unwrap().len() == 1);
    }

    #[test]
    fn editing_text_keeps_notified_flag() {
        let conn = open_db();
        let t = insert(&conn, "x", Some(500), "page").unwrap();
        mark_notified(&conn, &t.id, 600).unwrap();
        // Same due date, just a text edit — must NOT re-arm the reminder.
        update(&conn, &t.id, "x edited", Some(500)).unwrap();
        assert!(due_unnotified(&conn, 999).unwrap().is_empty());
    }

    #[test]
    fn overdue_count_counts_incomplete_past_due() {
        let conn = open_db();
        insert(&conn, "a", Some(100), "page").unwrap();
        insert(&conn, "b", Some(100_000), "page").unwrap();
        let done = insert(&conn, "c", Some(100), "page").unwrap();
        set_done(&conn, &done.id, true).unwrap();
        assert_eq!(overdue_count(&conn, 1_000).unwrap(), 1);
    }
}
