//! profile_snapshots persistence (#107).
//!
//! Row shapes + the four operations the worker / IPCs need:
//!   - `get_latest_for_person(conn, id)` — single read for the IPC.
//!   - `get_latest_map(conn, ids)` — bulk read for reconcile / ask.
//!   - `dirty_members(conn)` — tick-time candidate selection.
//!   - `insert_snapshot(conn, &row)` — worker write path.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Stored body shape — written into `profile_snapshots.body_json`.
/// Fields are all optional so the model can omit when nothing in the
/// inputs justifies a value (better than hallucinating).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProfileSnapshotBody {
    pub role_observed: Option<String>,
    #[serde(default)]
    pub frequent_collaborators: Vec<CollaboratorScore>,
    #[serde(default)]
    pub recent_focus: Vec<FocusItem>,
    pub working_hours_observed: Option<WorkingHours>,
    pub communication_style_notes: Option<String>,
    pub last_seen_active_ms: Option<i64>,
    /// Pointers into `profile_observations` (#52). Empty in v1.
    #[serde(default)]
    pub evidence_observation_ids: Vec<String>,
    /// v3 fields (#120) — prose summary + waiting-direction analysis.
    /// The worker prompt doesn't emit these yet; `#[serde(default)]`
    /// keeps old snapshots deserializing cleanly. Frontend already
    /// renders them via the list / detail surfaces (zero-state until
    /// the v3 worker ships).
    #[serde(default)]
    pub summary_prose: Option<String>,
    #[serde(default)]
    pub waiting_from_me: Vec<WaitingItem>,
    #[serde(default)]
    pub waiting_for_them: Vec<WaitingItem>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WaitingItem {
    pub description: String,
    pub source_kind: String,
    pub source_ref_id: String,
    pub since_ms: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CollaboratorScore {
    pub person_id: String,
    /// 0.0 .. 1.0 — relative collaboration strength among the team.
    pub score: f64,
    /// Short label naming which edge_kinds contributed
    /// (e.g. `"CO_ATTENDED + REPLIED_TO"`).
    pub evidence: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FocusItem {
    pub workstream_id: String,
    pub title: String,
    /// 0.0 .. 1.0.
    pub confidence: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkingHours {
    /// 24h local-time string, `"09:30"`.
    pub start_local: String,
    pub end_local: String,
}

/// Wire shape returned to the frontend + read by reconcile/ask.
#[derive(Serialize, Debug, Clone)]
pub struct ProfileSnapshot {
    pub person_id: String,
    pub computed_ms: i64,
    pub body: ProfileSnapshotBody,
    pub source_hash: String,
}

/// Most-recent snapshot for `person_id`, or `None` when never
/// computed.
pub fn get_latest_for_person(
    conn: &Connection,
    person_id: &str,
) -> rusqlite::Result<Option<ProfileSnapshot>> {
    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT computed_ms, body_json, source_hash \
               FROM profile_snapshots \
              WHERE person_id = ?1 \
              ORDER BY computed_ms DESC \
              LIMIT 1",
            params![person_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (computed_ms, body_json, source_hash) = match row {
        Some(t) => t,
        None => return Ok(None),
    };
    // A malformed body_json shouldn't take down callers — log + return
    // an empty body. The worker will recompute on the next eligible
    // pass.
    let body = serde_json::from_str::<ProfileSnapshotBody>(&body_json)
        .unwrap_or_else(|e| {
            eprintln!("[profiles] body_json parse failed for {person_id}: {e}");
            ProfileSnapshotBody::default()
        });
    Ok(Some(ProfileSnapshot {
        person_id: person_id.to_string(),
        computed_ms,
        body,
        source_hash,
    }))
}

/// Bulk-fetch the latest snapshot for each id in `member_ids`. Ids
/// without a snapshot row are simply absent from the result map;
/// callers should treat absence as "snapshot pending".
pub fn get_latest_map(
    conn: &Connection,
    member_ids: &[&str],
) -> rusqlite::Result<HashMap<String, ProfileSnapshot>> {
    let mut out = HashMap::new();
    if member_ids.is_empty() {
        return Ok(out);
    }
    let placeholders = std::iter::repeat("?")
        .take(member_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    // Two-step: collect (person_id, max(computed_ms)) first, then
    // pull body_json. Avoids the per-person LIMIT 1 subquery dance
    // and keeps the row hydration in one prepared statement.
    let sql = format!(
        "SELECT s.person_id, s.computed_ms, s.body_json, s.source_hash \
           FROM profile_snapshots s \
           JOIN ( \
                SELECT person_id, MAX(computed_ms) AS m \
                  FROM profile_snapshots \
                 WHERE person_id IN ({placeholders}) \
                 GROUP BY person_id \
           ) latest ON latest.person_id = s.person_id \
                   AND latest.m = s.computed_ms"
    );
    let mut stmt = conn.prepare(&sql)?;
    let id_params: Vec<&dyn rusqlite::ToSql> = member_ids
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(id_params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (person_id, computed_ms, body_json, source_hash) = row?;
        let body = serde_json::from_str::<ProfileSnapshotBody>(&body_json)
            .unwrap_or_else(|e| {
                eprintln!("[profiles] body_json parse failed for {person_id}: {e}");
                ProfileSnapshotBody::default()
            });
        out.insert(
            person_id.clone(),
            ProfileSnapshot {
                person_id,
                computed_ms,
                body,
                source_hash,
            },
        );
    }
    Ok(out)
}

/// Tick-time candidate selection. Returns the set of team_members
/// who either (a) have never had a snapshot computed or (b) have a
/// new `events` row since their latest snapshot. Self (`is_self=1`)
/// is excluded by default — the user doesn't need a derived snapshot
/// of themselves.
///
/// Dirtiness is derived live from the `events` table, not from a
/// per-row column — keeps event emission cheap.
pub fn dirty_members(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT m.id \
           FROM team_members m \
           LEFT JOIN ( \
                SELECT person_id, MAX(computed_ms) AS last_ms \
                  FROM profile_snapshots \
                 GROUP BY person_id \
           ) p ON p.person_id = m.id \
          WHERE m.is_self = 0 \
            AND ( \
                p.last_ms IS NULL \
                OR EXISTS ( \
                    SELECT 1 FROM events e \
                     WHERE e.actor_id = m.id \
                       AND e.ts_ms > p.last_ms \
                     LIMIT 1 \
                ) \
            )",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// Return the set of team_members eligible for recompute on this
/// tick: dirty members whose latest snapshot is older than
/// `ttl_ms` (or has never been computed). When `force = true`, the
/// TTL is ignored — every dirty member is returned.
///
/// Combines `dirty_members` with a per-person TTL check in a
/// single pass so the worker doesn't fan out one query per person.
pub fn ttl_eligible_members(
    conn: &Connection,
    now_ms: i64,
    ttl_ms: i64,
    force: bool,
) -> rusqlite::Result<Vec<String>> {
    let cutoff = if force { i64::MAX } else { now_ms - ttl_ms };
    let mut stmt = conn.prepare(
        "SELECT m.id \
           FROM team_members m \
           LEFT JOIN ( \
                SELECT person_id, MAX(computed_ms) AS last_ms \
                  FROM profile_snapshots \
                 GROUP BY person_id \
           ) p ON p.person_id = m.id \
          WHERE m.is_self = 0 \
            AND ( \
                p.last_ms IS NULL \
                OR p.last_ms < ?1 \
            ) \
            AND ( \
                p.last_ms IS NULL \
                OR EXISTS ( \
                    SELECT 1 FROM events e \
                     WHERE e.actor_id = m.id \
                       AND e.ts_ms > p.last_ms \
                     LIMIT 1 \
                ) \
            ) \
          ORDER BY p.last_ms ASC NULLS FIRST",
    )?;
    let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Most recent `events.ts_ms` where this person is the actor (#119).
/// Returns `None` when the person has zero events on record — typical
/// for a freshly-added teammate before any sync. Index-backed via
/// `idx_events_actor (actor_id, ts_ms DESC)`.
pub fn last_event_ms_for(
    conn: &rusqlite::Connection,
    person_id: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(ts_ms) FROM events WHERE actor_id = ?1",
        rusqlite::params![person_id],
        |r| r.get::<_, Option<i64>>(0),
    )
}

/// Drop a fresh row into `profile_snapshots`. Always INSERTs a new
/// row — history retention is part of the contract; UPDATEs would
/// defeat that. Returns the inserted row hydrated as a
/// `ProfileSnapshot`.
pub fn insert_snapshot(
    tx: &rusqlite::Transaction<'_>,
    person_id: &str,
    computed_ms: i64,
    body: &ProfileSnapshotBody,
    source_hash: &str,
) -> rusqlite::Result<ProfileSnapshot> {
    let body_json = serde_json::to_string(body)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    tx.execute(
        "INSERT INTO profile_snapshots \
            (person_id, computed_ms, body_json, source_hash) \
         VALUES (?1, ?2, ?3, ?4)",
        params![person_id, computed_ms, body_json, source_hash],
    )?;
    Ok(ProfileSnapshot {
        person_id: person_id.to_string(),
        computed_ms,
        body: body.clone(),
        source_hash: source_hash.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_member(conn: &Connection, id: &str, is_self: bool) {
        // `aliases` dropped in #017 (typed_aliases); `profile_md_path`
        // dropped in #117 (DB-backed snapshots replace the on-disk file).
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, \
                                       is_self, created_ms, updated_ms) \
             VALUES (?1, ?1, '', ?2, 0, 0)",
            params![id, is_self as i64],
        )
        .unwrap();
    }

    fn seed_event(conn: &Connection, actor_id: &str, ts_ms: i64) {
        conn.execute(
            "INSERT INTO events(ts_ms, kind, actor_id, ref_kind, ref_id, \
                                 payload, created_ms) \
             VALUES (?2, 'note_modified', ?1, 'note', 'n', '{}', ?2)",
            params![actor_id, ts_ms],
        )
        .unwrap();
    }

    /// Test wrapper: open a tx, insert, commit. The production
    /// `insert_snapshot` requires a `&Transaction` (#116) so each test
    /// either uses this helper or opens its own tx.
    fn ins(
        conn: &mut Connection,
        person_id: &str,
        computed_ms: i64,
        body: &ProfileSnapshotBody,
        source_hash: &str,
    ) -> rusqlite::Result<ProfileSnapshot> {
        let tx = conn.transaction()?;
        let snap = insert_snapshot(&tx, person_id, computed_ms, body, source_hash)?;
        tx.commit()?;
        Ok(snap)
    }

    #[test]
    fn get_latest_returns_most_recent() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        let body = ProfileSnapshotBody {
            role_observed: Some("Engineer".into()),
            ..Default::default()
        };
        ins(&mut conn, "tm_a", 100, &body, "hash-v1").unwrap();
        let mut newer = body.clone();
        newer.role_observed = Some("Senior engineer".into());
        ins(&mut conn, "tm_a", 200, &newer, "hash-v2").unwrap();

        let snap = get_latest_for_person(&conn, "tm_a").unwrap().unwrap();
        assert_eq!(snap.computed_ms, 200);
        assert_eq!(snap.source_hash, "hash-v2");
        assert_eq!(snap.body.role_observed.as_deref(), Some("Senior engineer"));
    }

    #[test]
    fn get_latest_returns_none_when_absent() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        assert!(get_latest_for_person(&conn, "tm_a").unwrap().is_none());
    }

    #[test]
    fn get_latest_map_returns_only_requested_ids() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        seed_member(&conn, "tm_b", false);
        seed_member(&conn, "tm_c", false);
        let body = ProfileSnapshotBody::default();
        ins(&mut conn, "tm_a", 100, &body, "h-a").unwrap();
        ins(&mut conn, "tm_b", 100, &body, "h-b").unwrap();
        ins(&mut conn, "tm_c", 100, &body, "h-c").unwrap();

        let map = get_latest_map(&conn, &["tm_a", "tm_c"]).unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("tm_a"));
        assert!(map.contains_key("tm_c"));
        assert!(!map.contains_key("tm_b"));
    }

    #[test]
    fn dirty_members_picks_never_computed() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        // No snapshot, no events — still dirty (first-pass case).
        let dirty = dirty_members(&conn).unwrap();
        assert!(dirty.contains("tm_a"));
    }

    #[test]
    fn dirty_members_picks_changed_actors() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        seed_member(&conn, "tm_b", false);
        let body = ProfileSnapshotBody::default();
        // Both have an existing snapshot at t=100.
        ins(&mut conn, "tm_a", 100, &body, "h-a").unwrap();
        ins(&mut conn, "tm_b", 100, &body, "h-b").unwrap();
        // Only tm_a has a new event since.
        seed_event(&conn, "tm_a", 200);

        let dirty = dirty_members(&conn).unwrap();
        assert!(dirty.contains("tm_a"));
        assert!(!dirty.contains("tm_b"));
    }

    #[test]
    fn dirty_members_excludes_self() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_a", false);
        // Self has no snapshot but is excluded by the WHERE clause.
        let dirty = dirty_members(&conn).unwrap();
        assert!(dirty.contains("tm_a"));
        assert!(!dirty.contains("tm_self"));
    }

    #[test]
    fn ttl_eligible_drops_fresh_snapshots() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        let body = ProfileSnapshotBody::default();
        // Snapshot at t=1000.
        ins(&mut conn, "tm_a", 1000, &body, "h-a").unwrap();
        // Event at t=2000 (so it IS dirty).
        seed_event(&conn, "tm_a", 2000);
        // now=2500, ttl=24h. Snapshot is only 1500ms old → not eligible.
        let elig = ttl_eligible_members(&conn, 2500, 24 * 3600 * 1000, false).unwrap();
        assert!(elig.is_empty(), "fresh snapshot must not be recomputed");
    }

    #[test]
    fn ttl_eligible_picks_stale_dirty() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        let body = ProfileSnapshotBody::default();
        ins(&mut conn, "tm_a", 1000, &body, "h-a").unwrap();
        seed_event(&conn, "tm_a", 2_000_000_000);
        let now = 1_000 + 25 * 3600 * 1000; // 25h later
        let elig = ttl_eligible_members(&conn, now, 24 * 3600 * 1000, false).unwrap();
        assert!(elig.contains(&"tm_a".to_string()));
    }

    #[test]
    fn ttl_eligible_force_bypasses_ttl() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        let body = ProfileSnapshotBody::default();
        ins(&mut conn, "tm_a", 1000, &body, "h-a").unwrap();
        seed_event(&conn, "tm_a", 2000);
        // Fresh snapshot, but force=true.
        let elig = ttl_eligible_members(&conn, 2500, 24 * 3600 * 1000, true).unwrap();
        assert_eq!(elig, vec!["tm_a".to_string()]);
    }

    #[test]
    fn ttl_eligible_includes_never_computed() {
        let mut conn = fresh_conn();
        seed_member(&conn, "tm_a", false);
        // No snapshot, no events — first-pass case.
        let elig = ttl_eligible_members(&conn, 5000, 24 * 3600 * 1000, false).unwrap();
        assert_eq!(elig, vec!["tm_a".to_string()]);
    }

    /// MAX(ts_ms) wins over insertion order — the helper sorts on the
    /// timestamp value, not the rowid.
    #[test]
    fn last_event_ms_for_returns_max_ts_for_actor() {
        let conn = fresh_conn();
        seed_member(&conn, "tm_alice", false);
        seed_event(&conn, "tm_alice", 1_000);
        seed_event(&conn, "tm_alice", 5_000);
        seed_event(&conn, "tm_alice", 3_000);
        assert_eq!(last_event_ms_for(&conn, "tm_alice").unwrap(), Some(5_000));
    }

    /// A freshly-added teammate with no events returns None — drives
    /// the Profile tab's existing `last_seen_active_ms != null` gate.
    #[test]
    fn last_event_ms_for_returns_none_when_no_events() {
        let conn = fresh_conn();
        seed_member(&conn, "tm_alice", false);
        assert_eq!(last_event_ms_for(&conn, "tm_alice").unwrap(), None);
    }

    /// The `WHERE actor_id = ?1` clause isolates per person — Bob's
    /// later activity must not leak into Alice's last-seen value.
    #[test]
    fn last_event_ms_for_isolates_by_actor() {
        let conn = fresh_conn();
        seed_member(&conn, "tm_alice", false);
        seed_member(&conn, "tm_bob", false);
        seed_event(&conn, "tm_alice", 2_000);
        seed_event(&conn, "tm_bob", 9_000);
        assert_eq!(last_event_ms_for(&conn, "tm_alice").unwrap(), Some(2_000));
        assert_eq!(last_event_ms_for(&conn, "tm_bob").unwrap(), Some(9_000));
    }
}
