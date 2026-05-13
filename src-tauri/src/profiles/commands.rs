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
