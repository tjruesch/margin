//! Tauri command handlers for the connector module.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::oauth;
use super::providers;
use super::ConnectorInfo;
use super::ConnectorRegistry;

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

#[derive(Serialize, Clone)]
pub struct OAuthProviderInfo {
    pub kind: String,
    pub display_name: String,
}

/// Returns the OAuth providers whose client_id is set at build time.
/// Drives the "Add connector" picker — providers without a configured
/// client_id are hidden so users don't see a button that can't work.
#[tauri::command]
pub fn list_oauth_providers() -> Vec<OAuthProviderInfo> {
    providers::list_configured()
        .into_iter()
        .map(|p| OAuthProviderInfo {
            kind: p.kind.to_string(),
            display_name: p.display_name.to_string(),
        })
        .collect()
}

/// Run the OAuth flow for `kind`, persist tokens + DB row, and return
/// the resulting `connector_id`.
///
/// Same `connector_id` repeats: the row is updated (tokens rotated)
/// rather than duplicated. This is also the "Reconnect" path — the
/// frontend can call this again with the same kind for an existing
/// account; the user just re-authenticates and the new tokens replace
/// the old ones.
#[tauri::command]
pub async fn start_oauth_connector(
    app: AppHandle,
    kind: String,
    conn: tauri::State<'_, Mutex<Connection>>,
    registry: tauri::State<'_, Arc<ConnectorRegistry>>,
) -> Result<String, String> {
    let provider = providers::lookup(&kind).ok_or_else(|| format!("unknown kind: {kind}"))?;

    let result = oauth::run_authorization_flow(&app, &kind)
        .await
        .map_err(|e| e.to_string())?;

    let connector_id = format!("{}:{}", kind, result.email);
    let display_name = format!("{} ({})", provider.display_name, result.email);
    let now_ms = current_unix_ms();

    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES (?1, ?2, ?3, 1, '{}', ?4, ?4) \
             ON CONFLICT(id) DO UPDATE SET \
                display_name = excluded.display_name, \
                enabled = 1, \
                updated_ms = excluded.updated_ms",
            rusqlite::params![&connector_id, &kind, &display_name, now_ms],
        )
        .map_err(|e| e.to_string())?;

        // Mark the connector due for an immediate sync the next time
        // the runner ticks.
        c.execute(
            "INSERT INTO sync_status(connector_id, next_due_ms) \
             VALUES (?1, 0) \
             ON CONFLICT(connector_id) DO UPDATE SET \
                next_due_ms = 0, \
                last_error = NULL",
            rusqlite::params![&connector_id],
        )
        .map_err(|e| e.to_string())?;
    }

    // Re-instantiate connectors. If a factory for this kind is
    // registered (#61, #63 onward), this picks the new row up; if
    // not, the row is skipped and the runner logs "no factory" until
    // a future build registers one.
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        if let Err(e) = registry.rebuild_instances(&app, &c) {
            eprintln!("[connectors] rebuild after OAuth flow failed: {e}");
        }
    }

    let _ = app.emit(
        "connector-status",
        serde_json::json!({
            "connector_id": connector_id,
            "state": "added",
            "message": null,
        }),
    );

    Ok(connector_id)
}

/// Remove a connector's DB row + keychain tokens. Idempotent — calling
/// twice is safe.
#[tauri::command]
pub async fn delete_connector(
    app: AppHandle,
    connector_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
    registry: tauri::State<'_, Arc<ConnectorRegistry>>,
) -> Result<(), String> {
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        // CASCADE on sync_status takes care of the status row.
        c.execute(
            "DELETE FROM connectors WHERE id = ?1",
            rusqlite::params![&connector_id],
        )
        .map_err(|e| e.to_string())?;
    }

    if let Err(e) = oauth::forget_tokens(&connector_id) {
        // Log but don't fail — the DB row is gone, which is the
        // source of truth. Orphan keychain entries are inert.
        eprintln!("[connectors] forget_tokens failed for {connector_id}: {e}");
    }

    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        if let Err(e) = registry.rebuild_instances(&app, &c) {
            eprintln!("[connectors] rebuild after delete failed: {e}");
        }
    }

    let _ = app.emit(
        "connector-status",
        serde_json::json!({
            "connector_id": connector_id,
            "state": "removed",
            "message": null,
        }),
    );

    Ok(())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- Calendar query commands (#63) -------------------------------------

#[tauri::command]
pub fn list_calendar_events(
    start_ms: i64,
    end_ms: i64,
    connector_id: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<super::calendar::CalendarEvent>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    super::calendar::list_events_in_range(&c, start_ms, end_ms, connector_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_event_details(
    event_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<super::calendar::CalendarEvent>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    super::calendar::get_event_details(&c, &event_id).map_err(|e| e.to_string())
}

/// Click handler for the "Coming up" strip (#62). Returns a path to a
/// note bundle that "belongs" to this calendar event:
///   - If the event already has a `linked_note_path` AND the file still
///     exists on disk, return that path.
///   - Otherwise, create a fresh bundle, write a starter body with
///     calendar metadata in frontmatter, persist meeting attendees in
///     the team module, and store the path on the event row for next
///     time.
#[tauri::command]
pub fn open_or_create_event_note(
    event_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<String, String> {
    use std::fs;
    use std::path::Path;

    let event = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        super::calendar::get_event_details(&c, &event_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("event {event_id} not found"))?
    };

    // Reuse the linked bundle if it still exists on disk.
    if let Some(path) = &event.linked_note_path {
        if Path::new(path).exists() {
            return Ok(path.clone());
        }
    }

    // Create a fresh bundle. Mirrors notes::create_note's body — we
    // can't call that directly because it's a Tauri command; the
    // create-dir + write-file + index touch is small enough to
    // duplicate.
    let id = uuid::Uuid::new_v4().to_string();
    let dir = crate::paths::notes_dir().join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let note_path = dir.join(crate::notes::NOTE_FILENAME);

    // Compose starter body with frontmatter. AI ask (#64) will key off
    // `calendar_event_id` to join the note back to its event in
    // future contextual prompts.
    let body = format_event_note_body(&event);
    fs::write(&note_path, body).map_err(|e| e.to_string())?;

    let note_path_str = note_path.to_string_lossy().into_owned();

    // Persist attendees + the link in one lock window.
    {
        let mut c = conn.lock().map_err(|e| e.to_string())?;
        // Index the new bundle so it shows up in list_notes / search
        // immediately.
        if let Err(e) = crate::index::upsert(&mut c, &note_path) {
            eprintln!("[connectors] index upsert for new event note failed: {e}");
        }
        // Save attendees that resolved to known team_members. Mirrors
        // team::set_meeting_attendees inline (the latter is a Tauri
        // command and awkward to call from here).
        let member_ids: Vec<String> = event
            .attendees
            .iter()
            .filter_map(|a| a.team_member_id.clone())
            .collect();
        if !member_ids.is_empty() {
            let tx = c.transaction().map_err(|e| e.to_string())?;
            tx.execute(
                "DELETE FROM meeting_attendees WHERE note_path = ?1",
                rusqlite::params![&note_path_str],
            )
            .map_err(|e| e.to_string())?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO meeting_attendees(note_path, member_id) VALUES (?1, ?2) \
                         ON CONFLICT(note_path, member_id) DO NOTHING",
                    )
                    .map_err(|e| e.to_string())?;
                for member_id in &member_ids {
                    stmt.execute(rusqlite::params![&note_path_str, member_id])
                        .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
        }
        super::calendar::set_linked_note_path(&c, &event.id, &note_path_str)
            .map_err(|e| e.to_string())?;
    }

    Ok(note_path_str)
}

fn format_event_note_body(event: &super::calendar::CalendarEvent) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("calendar_event_id: {}\n", yaml_escape(&event.id)));
    s.push_str(&format!("meeting_start_ms: {}\n", event.start_ms));
    s.push_str(&format!("meeting_end_ms: {}\n", event.end_ms));
    if let Some(loc) = &event.location {
        s.push_str(&format!("location: {}\n", yaml_escape(loc)));
    }
    s.push_str("---\n\n");
    s.push_str(&format!("# {}\n\n", event.title));
    s
}

/// Minimal YAML string escape — sufficient for IDs / locations the
/// connector hands us. If the value contains any special chars
/// (colon, quotes, newline, leading/trailing whitespace) we wrap it
/// in double quotes and backslash-escape internal quotes/backslashes.
fn yaml_escape(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains(':')
        || s.contains('\n')
        || s.contains('"')
        || s.contains('\'')
        || s.contains('#');
    if !needs_quoting {
        return s.to_string();
    }
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
