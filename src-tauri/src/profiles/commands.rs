//! Tauri IPCs for profile snapshots (#107).

use std::sync::Mutex;

use rusqlite::Connection;
use tauri::AppHandle;

use super::persist::{self, ProfileSnapshot};

/// Return the latest snapshot for `member_id`, or `null` when the
/// worker hasn't computed one yet. The frontend renders an
/// empty-state with a "Compute now" button in that case.
#[tauri::command]
pub fn get_profile_snapshot(
    member_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<ProfileSnapshot>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::get_latest_for_person(&c, &member_id).map_err(|e| e.to_string())
}

/// Force an immediate recompute for `member_id` regardless of the
/// 24h TTL guard. Clears any active rate-limit backoff so the user
/// can self-serve after fixing a key/billing issue.
#[tauri::command]
pub async fn force_recompute_profile(
    member_id: String,
    app: AppHandle,
) -> Result<ProfileSnapshot, String> {
    super::worker::recompute_one_for_ipc(&app, &member_id).await
}

/// Return the latest snapshot strictly older than `before_ms` (#118).
/// Drives the "Compared to: 7d / 30d ago" dropdown on the Profile tab.
#[tauri::command]
pub fn get_profile_snapshot_at(
    member_id: String,
    before_ms: i64,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<ProfileSnapshot>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::get_snapshot_before(&c, &member_id, before_ms).map_err(|e| e.to_string())
}

/// Return the oldest snapshot recorded for `member_id` (#118). Used by
/// the "Compared to: first snapshot" dropdown option.
#[tauri::command]
pub fn get_first_profile_snapshot(
    member_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<ProfileSnapshot>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::get_first_snapshot(&c, &member_id).map_err(|e| e.to_string())
}

/// Total number of snapshots stored for `member_id` (#118). The
/// frontend renders an empty-state when this is `<= 1` since there's
/// nothing to compare.
#[tauri::command]
pub fn count_profile_snapshots(
    member_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<i64, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::count_snapshots_for(&c, &member_id).map_err(|e| e.to_string())
}

