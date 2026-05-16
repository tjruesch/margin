//! Single emission helper for the activity stream (#106).
//!
//! Every entity write path that lands a new row in an entity table
//! also drops a row here in the same transaction. Downstream consumers
//! (profile worker #107, activity popover #110, edge evidence) read
//! from this table generically; the per-source `kind` + `payload`
//! schema is a soft convention documented at each call site.
//!
//! Idempotency is the caller's responsibility — `events.id` is
//! autoincrement and there's no natural unique key. Each emit is a
//! pure INSERT; callers gate emission on "the entity row was newly
//! inserted" (or, for action_completed, on a done-flag transition).

use rusqlite::{params, Transaction};
use serde_json::Value;

/// Insert one events row inside the caller's transaction.
///
/// `ts_ms` is the event's logical timestamp — use the entity's own
/// source-of-truth timestamp (sent_at_ms, start_ms, modified_ms) when
/// available, NOT `now()`, so the events stream is consistent with
/// the historical record.
pub fn emit(
    tx: &Transaction<'_>,
    ts_ms: i64,
    kind: &str,
    actor_id: Option<&str>,
    ref_kind: &str,
    ref_id: &str,
    payload: &Value,
) -> rusqlite::Result<()> {
    let payload_str = payload.to_string();
    let created_ms = current_unix_ms();
    tx.execute(
        "INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![ts_ms, kind, actor_id, ref_kind, ref_id, payload_str, created_ms],
    )?;
    Ok(())
}

pub(crate) fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

    #[test]
    fn emit_round_trips_a_row() {
        let mut conn = open_db();
        // Seed the team_member referenced by actor_id; events.actor_id
        // has an FK to team_members(id).
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES ('tm_self', 'Me', '', 1, 0, 0)",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        emit(
            &tx,
            1_000,
            "email_sent",
            Some("tm_self"),
            "email",
            "mg:test::m1",
            &serde_json::json!({"thread_id": "t1", "subject": "hi"}),
        )
        .unwrap();
        tx.commit().unwrap();

        let (ts_ms, kind, actor_id, ref_kind, ref_id, payload): (
            i64,
            String,
            Option<String>,
            String,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT ts_ms, kind, actor_id, ref_kind, ref_id, payload FROM events WHERE ref_id = 'mg:test::m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(ts_ms, 1_000);
        assert_eq!(kind, "email_sent");
        assert_eq!(actor_id.as_deref(), Some("tm_self"));
        assert_eq!(ref_kind, "email");
        assert_eq!(ref_id, "mg:test::m1");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["subject"], "hi");
    }
}
