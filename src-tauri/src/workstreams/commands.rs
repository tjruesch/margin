//! Tauri commands for the workstreams module.

use std::sync::Mutex;

use rusqlite::Connection;
use tauri::AppHandle;

use super::{persist, synthesizer, ClusterReport, Workstream, WorkstreamDetail};

#[tauri::command]
pub async fn synthesize_workstreams(
    app: AppHandle,
    force: bool,
) -> Result<ClusterReport, String> {
    synthesizer::maybe_cluster(&app, force).await
}

#[tauri::command]
pub fn list_workstreams(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<Workstream>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::list_workstreams_active(&c).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_workstream_details(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<WorkstreamDetail>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::get_workstream_detail(&c, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_workstream_action_done(
    action_id: String,
    done: bool,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_action_done(&c, &action_id, done).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_workstream_status(
    id: String,
    status: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    if !matches!(status.as_str(), "active" | "archived" | "snoozed") {
        return Err(format!("invalid status: {status}"));
    }
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_status(&c, &id, &status).map_err(|e| e.to_string())
}

/// Update a workstream's user-authored context (#77). Whitespace-only
/// input is treated as a clear (persists `NULL`) so the prompt-omission
/// logic downstream can `filter(|s| !s.is_empty())` cleanly.
#[tauri::command]
pub fn set_workstream_user_notes(
    id: String,
    notes: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let trimmed = notes.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_user_notes(&c, &id, trimmed).map_err(|e| e.to_string())
}

/// List archived workstreams for the Workstreams view's collapsed
/// "Archived (N)" accordion (#78). Most recently archived first.
#[tauri::command]
pub fn list_archived_workstreams(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<Workstream>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::list_workstreams_archived(&c).map_err(|e| e.to_string())
}

/// Clear the `reopened_at_ms` marker on a workstream (#78). Called by
/// the detail view's unmount cleanup once the user has visited a
/// reopened workstream — the "Reopened" badge stops showing on
/// subsequent list renders.
#[tauri::command]
pub fn mark_workstream_seen(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::mark_seen(&c, &id).map_err(|e| e.to_string())
}
