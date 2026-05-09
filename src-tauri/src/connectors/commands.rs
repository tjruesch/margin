//! Tauri command handlers for the connector module. Only `list_connectors`
//! ships in #59 — the add/remove flow lands with #60's OAuth work.

use std::sync::Mutex;

use rusqlite::Connection;

use super::ConnectorInfo;

#[tauri::command]
pub fn list_connectors(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<ConnectorInfo>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let mut stmt = c
        .prepare(
            "SELECT c.id, c.kind, c.display_name, c.enabled, \
                    s.last_sync_ms, s.last_success_ms, s.last_error, \
                    COALESCE(s.next_due_ms, 0) AS next_due_ms \
             FROM connectors c \
             LEFT JOIN sync_status s ON s.connector_id = c.id \
             ORDER BY c.kind, c.display_name",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ConnectorInfo {
                id: r.get(0)?,
                kind: r.get(1)?,
                display_name: r.get(2)?,
                enabled: r.get::<_, i64>(3)? != 0,
                last_sync_ms: r.get(4)?,
                last_success_ms: r.get(5)?,
                last_error: r.get(6)?,
                next_due_ms: r.get(7)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}
