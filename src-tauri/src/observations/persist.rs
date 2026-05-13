//! SQLite read/write paths for `profile_observations`.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension, Result, Transaction};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ObservationStatus {
    Pending,
    Accepted,
    Rejected,
}

impl ObservationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationStatus::Pending => "pending",
            ObservationStatus::Accepted => "accepted",
            ObservationStatus::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ObservationStatus::Pending),
            "accepted" => Some(ObservationStatus::Accepted),
            "rejected" => Some(ObservationStatus::Rejected),
            _ => None,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ProfileObservation {
    pub id: String,
    pub member_id: String,
    pub source_note_id: String,
    pub source_note_title: Option<String>,
    pub body: String,
    pub status: ObservationStatus,
    pub created_ms: i64,
    pub reviewed_ms: Option<i64>,
}

/// Insert a fresh observation in the `pending` state. The caller owns the
/// transaction (reconcile post-processor bulk-inserts multiple rows in one tx).
/// Returns the generated `obs_<uuid>` id.
pub fn insert_pending(
    tx: &Transaction<'_>,
    member_id: &str,
    source_note_id: &str,
    body: &str,
    now_ms: i64,
) -> Result<String> {
    let id = format!("obs_{}", uuid::Uuid::new_v4());
    tx.execute(
        "INSERT INTO profile_observations \
            (id, member_id, source_note_id, body, status, created_ms) \
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
        params![id, member_id, source_note_id, body, now_ms],
    )?;
    Ok(id)
}

/// All observations for a member, optionally filtered by status. The source
/// note title is hydrated via LEFT JOIN against `notes` so the UI can render
/// a clickable label even after a note is renamed.
pub fn list_by_member(
    conn: &Connection,
    member_id: &str,
    status: Option<ObservationStatus>,
) -> Result<Vec<ProfileObservation>> {
    let (sql, status_param): (&str, Option<&'static str>) = match status {
        Some(s) => (
            "SELECT o.id, o.member_id, o.source_note_id, n.title, o.body, \
                    o.status, o.created_ms, o.reviewed_ms \
               FROM profile_observations o \
               LEFT JOIN notes n ON n.id = o.source_note_id \
              WHERE o.member_id = ?1 AND o.status = ?2 \
              ORDER BY o.created_ms DESC",
            Some(s.as_str()),
        ),
        None => (
            "SELECT o.id, o.member_id, o.source_note_id, n.title, o.body, \
                    o.status, o.created_ms, o.reviewed_ms \
               FROM profile_observations o \
               LEFT JOIN notes n ON n.id = o.source_note_id \
              WHERE o.member_id = ?1 \
              ORDER BY o.created_ms DESC",
            None,
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = match status_param {
        Some(s) => stmt.query(params![member_id, s])?.mapped(row_to_obs).collect(),
        None => stmt.query(params![member_id])?.mapped(row_to_obs).collect(),
    };
    rows
}

fn row_to_obs(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileObservation> {
    let status_str: String = row.get(5)?;
    let status = ObservationStatus::parse(&status_str)
        .unwrap_or(ObservationStatus::Pending);
    Ok(ProfileObservation {
        id: row.get(0)?,
        member_id: row.get(1)?,
        source_note_id: row.get(2)?,
        source_note_title: row.get(3)?,
        body: row.get(4)?,
        status,
        created_ms: row.get(6)?,
        reviewed_ms: row.get(7)?,
    })
}

/// Map of `member_id -> pending count` across the whole table. Used by
/// the Team list pane to render a "N pending" badge.
pub fn pending_counts(conn: &Connection) -> Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT member_id, COUNT(*) FROM profile_observations \
          WHERE status = 'pending' \
          GROUP BY member_id",
    )?;
    let mut out = HashMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (member_id, count) = row?;
        out.insert(member_id, count);
    }
    Ok(out)
}

/// Update an observation's status, stamping `reviewed_ms`. Returns the
/// `member_id` so the accept handler can emit an event scoped to the
/// right person. `None` when no row matched.
pub fn set_status(
    tx: &Transaction<'_>,
    id: &str,
    status: ObservationStatus,
    now_ms: i64,
) -> Result<Option<String>> {
    let member_id: Option<String> = tx
        .query_row(
            "SELECT member_id FROM profile_observations WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if member_id.is_none() {
        return Ok(None);
    }
    tx.execute(
        "UPDATE profile_observations \
            SET status = ?1, reviewed_ms = ?2 \
          WHERE id = ?3",
        params![status.as_str(), now_ms, id],
    )?;
    Ok(member_id)
}

pub fn delete(tx: &Transaction<'_>, id: &str) -> Result<()> {
    tx.execute("DELETE FROM profile_observations WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_member(conn: &Connection, id: &str, display: &str) {
        conn.execute(
            "INSERT INTO team_members \
                (id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES (?1, ?2, '', ?3, 0, 0, 0)",
            params![id, display, format!("/x/{id}.md")],
        )
        .unwrap();
    }

    fn seed_note(conn: &Connection, id: &str, title: &str) {
        conn.execute(
            "INSERT INTO notes (id, bundle_id, title, modified_ms, preview, body_size) \
             VALUES (?1, ?2, ?3, 0, '', 0)",
            params![id, format!("b_{id}"), title],
        )
        .unwrap();
    }

    #[test]
    fn insert_pending_round_trips() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_note(&conn, "n1", "First meeting");
        let tx = conn.transaction().unwrap();
        let id = insert_pending(&tx, "tm_a", "n1", "Async-first communicator.", 1_000).unwrap();
        tx.commit().unwrap();
        assert!(id.starts_with("obs_"));

        let rows = list_by_member(&conn, "tm_a", None).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.member_id, "tm_a");
        assert_eq!(r.source_note_id, "n1");
        assert_eq!(r.source_note_title.as_deref(), Some("First meeting"));
        assert_eq!(r.body, "Async-first communicator.");
        assert_eq!(r.status, ObservationStatus::Pending);
        assert_eq!(r.created_ms, 1_000);
        assert!(r.reviewed_ms.is_none());
    }

    #[test]
    fn list_by_member_filters_by_status() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_note(&conn, "n1", "Meeting one");
        let tx = conn.transaction().unwrap();
        let id1 = insert_pending(&tx, "tm_a", "n1", "Pending one.", 1).unwrap();
        let _id2 = insert_pending(&tx, "tm_a", "n1", "Pending two.", 2).unwrap();
        set_status(&tx, &id1, ObservationStatus::Accepted, 3).unwrap();
        tx.commit().unwrap();

        let pending = list_by_member(&conn, "tm_a", Some(ObservationStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "Pending two.");

        let accepted = list_by_member(&conn, "tm_a", Some(ObservationStatus::Accepted)).unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].body, "Pending one.");
        assert_eq!(accepted[0].reviewed_ms, Some(3));

        let all = list_by_member(&conn, "tm_a", None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn pending_counts_groups_by_member() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_member(&conn, "tm_b", "Bob");
        seed_note(&conn, "n1", "M1");
        let tx = conn.transaction().unwrap();
        let _ = insert_pending(&tx, "tm_a", "n1", "x", 1).unwrap();
        let _ = insert_pending(&tx, "tm_a", "n1", "y", 2).unwrap();
        let id_b = insert_pending(&tx, "tm_b", "n1", "z", 3).unwrap();
        set_status(&tx, &id_b, ObservationStatus::Rejected, 4).unwrap();
        tx.commit().unwrap();

        let counts = pending_counts(&conn).unwrap();
        assert_eq!(counts.get("tm_a"), Some(&2));
        // tm_b's only row was rejected, so it shouldn't appear.
        assert_eq!(counts.get("tm_b"), None);
    }

    #[test]
    fn set_status_returns_member_id() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_note(&conn, "n1", "M1");
        let tx = conn.transaction().unwrap();
        let id = insert_pending(&tx, "tm_a", "n1", "body", 1).unwrap();
        let got = set_status(&tx, &id, ObservationStatus::Accepted, 2).unwrap();
        tx.commit().unwrap();
        assert_eq!(got.as_deref(), Some("tm_a"));
    }

    #[test]
    fn set_status_missing_id_returns_none() {
        let mut conn = open_db();
        let tx = conn.transaction().unwrap();
        let got = set_status(&tx, "obs_doesnotexist", ObservationStatus::Accepted, 1).unwrap();
        tx.commit().unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn cascade_on_note_delete_removes_obs() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_note(&conn, "n1", "M1");
        let tx = conn.transaction().unwrap();
        let _ = insert_pending(&tx, "tm_a", "n1", "body", 1).unwrap();
        tx.commit().unwrap();
        assert_eq!(list_by_member(&conn, "tm_a", None).unwrap().len(), 1);

        conn.execute("DELETE FROM notes WHERE id = 'n1'", [])
            .unwrap();
        assert_eq!(list_by_member(&conn, "tm_a", None).unwrap().len(), 0);
    }

    #[test]
    fn cascade_on_member_delete_removes_obs() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_note(&conn, "n1", "M1");
        let tx = conn.transaction().unwrap();
        let _ = insert_pending(&tx, "tm_a", "n1", "body", 1).unwrap();
        tx.commit().unwrap();
        assert_eq!(list_by_member(&conn, "tm_a", None).unwrap().len(), 1);

        conn.execute("DELETE FROM team_members WHERE id = 'tm_a'", [])
            .unwrap();
        assert_eq!(list_by_member(&conn, "tm_a", None).unwrap().len(), 0);
    }

    #[test]
    fn delete_removes_row() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a", "Alice");
        seed_note(&conn, "n1", "M1");
        let tx = conn.transaction().unwrap();
        let id = insert_pending(&tx, "tm_a", "n1", "body", 1).unwrap();
        delete(&tx, &id).unwrap();
        tx.commit().unwrap();
        assert_eq!(list_by_member(&conn, "tm_a", None).unwrap().len(), 0);
    }
}
