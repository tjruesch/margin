//! Tauri IPC surface for `profile_observations`.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;

use super::persist::{
    delete as delete_row, list_by_member, pending_counts, set_status, ObservationStatus,
    ProfileObservation,
};

#[tauri::command]
pub fn list_profile_observations(
    member_id: String,
    status: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<ProfileObservation>, String> {
    let parsed_status = match status.as_deref() {
        None | Some("") => None,
        Some(s) => match ObservationStatus::parse(s) {
            Some(s) => Some(s),
            None => return Err(format!("unknown status: {s}")),
        },
    };
    let c = conn.lock().map_err(|e| e.to_string())?;
    list_by_member(&c, &member_id, parsed_status).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pending_observation_counts(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<HashMap<String, i64>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    pending_counts(&c).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn accept_profile_observation(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    transition(&conn, &id, ObservationStatus::Accepted, true)
}

#[tauri::command]
pub fn reject_profile_observation(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    transition(&conn, &id, ObservationStatus::Rejected, false)
}

#[tauri::command]
pub fn delete_profile_observation(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    delete_row(&tx, &id).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Shared status-flip path. When `emit_event` is true, also emit an
/// `observation_accepted` row into `events` so the #107 profile worker
/// picks the person up on its next tick.
fn transition(
    conn: &Mutex<Connection>,
    id: &str,
    status: ObservationStatus,
    emit_event: bool,
) -> Result<(), String> {
    let now = crate::events::current_unix_ms();
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let member_id = set_status(&tx, id, status, now).map_err(|e| e.to_string())?;
    let member_id = match member_id {
        Some(m) => m,
        None => {
            // Nothing to do; the row vanished between the user's click
            // and now. Treat as a successful no-op rather than an error.
            tx.commit().map_err(|e| e.to_string())?;
            return Ok(());
        }
    };
    if emit_event {
        crate::events::emit(
            &tx,
            now,
            "observation_accepted",
            Some(&member_id),
            "observation",
            id,
            &serde_json::json!({}),
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_member(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO team_members \
                (id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES (?1, ?2, '', ?3, 0, 0, 0)",
            params![id, id, format!("/x/{id}.md")],
        )
        .unwrap();
    }

    fn seed_note(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO notes (id, bundle_id, title, modified_ms, preview, body_size) \
             VALUES (?1, ?2, 'T', 0, '', 0)",
            params![id, format!("b_{id}")],
        )
        .unwrap();
    }

    fn insert_pending_for(conn: &mut Connection, member_id: &str, note_id: &str) -> String {
        let tx = conn.transaction().unwrap();
        let id = super::super::persist::insert_pending(&tx, member_id, note_id, "body", 1).unwrap();
        tx.commit().unwrap();
        id
    }

    #[test]
    fn accept_flips_status_and_emits_event() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a");
        seed_note(&conn, "n1");
        let id = insert_pending_for(&mut conn, "tm_a", "n1");

        let lock = Mutex::new(conn);
        transition(&lock, &id, ObservationStatus::Accepted, true).unwrap();
        let conn = lock.into_inner().unwrap();

        let rows = list_by_member(&conn, "tm_a", None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ObservationStatus::Accepted);
        assert!(rows[0].reviewed_ms.is_some());

        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events \
                  WHERE kind = 'observation_accepted' \
                    AND actor_id = 'tm_a' \
                    AND ref_kind = 'observation' \
                    AND ref_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);
    }

    #[test]
    fn reject_flips_status_without_event() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a");
        seed_note(&conn, "n1");
        let id = insert_pending_for(&mut conn, "tm_a", "n1");

        let lock = Mutex::new(conn);
        transition(&lock, &id, ObservationStatus::Rejected, false).unwrap();
        let conn = lock.into_inner().unwrap();

        let rows = list_by_member(&conn, "tm_a", None).unwrap();
        assert_eq!(rows[0].status, ObservationStatus::Rejected);

        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'observation_accepted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 0);
    }

    #[test]
    fn transition_on_missing_id_is_noop() {
        let conn = open_db();
        let lock = Mutex::new(conn);
        transition(&lock, "obs_missing", ObservationStatus::Accepted, true).unwrap();
        let conn = lock.into_inner().unwrap();
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(event_count, 0);
    }
}
