//! Tauri IPCs for profile snapshots (#107).

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;
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

#[derive(Serialize, Clone, Debug, Default)]
pub struct TeamWaitingCounts {
    /// Items where the team member is waiting on the user to act.
    pub from_me: u32,
    /// Items where the user is waiting on the team member to act.
    pub for_them: u32,
    /// Snapshot's last_seen_active_ms if present — useful for the
    /// list view to show relative-time hints without a separate fetch.
    pub last_seen_active_ms: Option<i64>,
}

/// Bulk waiting/last-active counts across the whole team. Returns a
/// map keyed by `team_members.id`; rows without a snapshot map to a
/// zero-default entry (counts = 0, last-active = None). Populated by
/// the v3 worker (#120); until that lands, all counts stay 0 — the
/// frontend still renders the table structure, just with empty cells.
#[tauri::command]
pub fn team_waiting_counts(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<HashMap<String, TeamWaitingCounts>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    // Collect every team_member id (cheap — small table).
    let mut stmt = c
        .prepare("SELECT id FROM team_members")
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let snapshots =
        persist::get_latest_map(&c, &id_refs).map_err(|e| e.to_string())?;

    let mut out: HashMap<String, TeamWaitingCounts> = HashMap::new();
    for id in &ids {
        let entry = match snapshots.get(id) {
            Some(snap) => TeamWaitingCounts {
                from_me: snap.body.waiting_from_me.len() as u32,
                for_them: snap.body.waiting_for_them.len() as u32,
                last_seen_active_ms: snap.body.last_seen_active_ms,
            },
            None => TeamWaitingCounts::default(),
        };
        out.insert(id.clone(), entry);
    }
    Ok(out)
}
