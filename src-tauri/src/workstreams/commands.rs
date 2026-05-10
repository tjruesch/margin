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
