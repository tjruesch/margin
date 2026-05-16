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
/// map keyed by `team_members.id`. Waiting counts come from the
/// unified `actions` table (post #120 follow-up), filtered to rows
/// the profile worker created: `origin_synth_kind` matches a
/// `_waiting` variant, `done = 0`. `last_seen_active_ms` still
/// comes from the latest profile snapshot's body.
#[tauri::command]
pub fn team_waiting_counts(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<HashMap<String, TeamWaitingCounts>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let ids: Vec<String> = {
        let mut stmt = c
            .prepare("SELECT id FROM team_members")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    // last_seen_active_ms continues to come from snapshots.
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let snapshots =
        persist::get_latest_map(&c, &id_refs).map_err(|e| e.to_string())?;

    let self_id: Option<String> = c
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();

    let mut out: HashMap<String, TeamWaitingCounts> = HashMap::new();
    for id in &ids {
        let from_me_count = self_id
            .as_ref()
            .map(|s| count_waiting(&c, s.as_str(), id.as_str()).unwrap_or(0))
            .unwrap_or(0);
        let for_them_count = self_id
            .as_ref()
            .map(|s| count_waiting(&c, id.as_str(), s.as_str()).unwrap_or(0))
            .unwrap_or(0);
        let last_seen = snapshots
            .get(id)
            .and_then(|snap| snap.body.last_seen_active_ms);
        out.insert(
            id.clone(),
            TeamWaitingCounts {
                from_me: from_me_count,
                for_them: for_them_count,
                last_seen_active_ms: last_seen,
            },
        );
    }
    Ok(out)
}

/// Count open `_waiting` actions where `assignee_id = ?1` and
/// `subject_member_id = ?2`. Helper for `team_waiting_counts`.
fn count_waiting(conn: &Connection, assignee_id: &str, subject_id: &str) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM actions \
          WHERE done = 0 \
            AND origin_kind = 'synth' \
            AND origin_synth_kind IN ('email_waiting','teams_waiting','meeting_waiting') \
            AND assignee_id = ?1 \
            AND subject_member_id = ?2",
        rusqlite::params![assignee_id, subject_id],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}
