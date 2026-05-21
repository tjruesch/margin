//! Per-workstream "recently rejected actions" surface (#150).
//!
//! Reads `action_deletions` (#147) and attributes each rejection to
//! the workstream(s) it should inform. The synthesizer renders the
//! resulting per-workstream lists in the cluster prompt so subsequent
//! passes stop re-emitting texts the user already deleted.
//!
//! Three attribution paths, all in a 30-day window with cause
//! `user_delete` or `user_dismiss` (auto_resolved is excluded —
//! worker omissions are weak signal):
//!   1. **Synth attached via current pivots** — `origin_kind='synth'`,
//!      with `(origin_synth_kind, origin_synth_id)` still matching a
//!      `workstream_signals` pivot on W. Catches synth-emitted actions
//!      the user deleted on signals that remain attached.
//!   2. **Subject is a workstream member** — `subject_member_id`
//!      appears in W's derived members. Catches profile-worker waiting
//!      rows (email/teams/meeting_waiting) deleted on a counterparty
//!      who belongs to W.
//!   3. **Reconcile-origin from a series whose events pivot to W** —
//!      `origin_kind='reconcile'` with `source_series_master_id`
//!      matching a series whose `calendar_events` rows are in W's
//!      `workstream_signals` (kind='event'). Catches recurring-meeting
//!      action items the user deleted.
//!
//! Per-workstream output: dedupe by normalized text, recency desc,
//! cap at `MAX_PER_WORKSTREAM`.

use std::collections::{HashMap, HashSet};

use rusqlite::{params_from_iter, Connection};

use super::Workstream;

const WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Cap on rejected texts surfaced per workstream. Matches the spec's
/// "limit ~15" so the prompt block stays compact even for workstreams
/// with active cleanup cycles.
pub const MAX_PER_WORKSTREAM: usize = 15;

#[derive(Debug, Clone)]
struct DeletionRow {
    origin_kind: String,
    origin_synth_kind: Option<String>,
    origin_synth_id: Option<String>,
    subject_member_id: Option<String>,
    source_series_master_id: Option<String>,
    text: String,
}

/// Lowercase + collapse whitespace. Same normalization used by #148 so
/// dedup behaves consistently across the three consumers.
fn normalize_for_dedup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.extend(ch.to_lowercase());
            last_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Pull every in-window user-driven deletion in one query. Filter
/// downstream rather than per-workstream so we hit the table once
/// regardless of workstream count.
fn fetch_recent_deletions(
    conn: &Connection,
    window_start_ms: i64,
) -> rusqlite::Result<Vec<DeletionRow>> {
    let mut stmt = conn.prepare(
        "SELECT origin_kind, origin_synth_kind, origin_synth_id, \
                subject_member_id, source_series_master_id, text \
           FROM action_deletions \
          WHERE cause IN ('user_delete', 'user_dismiss') \
            AND deleted_ms > ?1 \
          ORDER BY deleted_ms DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![window_start_ms], |r| {
        Ok(DeletionRow {
            origin_kind: r.get(0)?,
            origin_synth_kind: r.get(1)?,
            origin_synth_id: r.get(2)?,
            subject_member_id: r.get(3)?,
            source_series_master_id: r.get(4)?,
            text: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// Build a `(kind, item_id) -> workstream_id` index from the current
/// `workstream_signals` pivots, scoped to the supplied workstreams.
/// Detached pivots (`manual_detached_ms IS NOT NULL`) are excluded —
/// if the user pulled the signal off the workstream, the rejection on
/// that signal shouldn't inform future passes of the same workstream.
fn signal_pivots(
    conn: &Connection,
    workstream_ids: &[String],
) -> rusqlite::Result<HashMap<(String, String), Vec<String>>> {
    if workstream_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; workstream_ids.len()].join(",");
    let sql = format!(
        "SELECT kind, item_id, workstream_id FROM workstream_signals \
          WHERE workstream_id IN ({placeholders}) \
            AND manual_detached_ms IS NULL"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<Box<dyn rusqlite::ToSql>> = workstream_ids
        .iter()
        .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out: HashMap<(String, String), Vec<String>> = HashMap::new();
    for row in rows {
        let (kind, item_id, ws_id) = row?;
        out.entry((kind, item_id)).or_default().push(ws_id);
    }
    Ok(out)
}

/// Build a `series_master_id -> workstream_ids` index by joining the
/// workstreams' event pivots back to `calendar_events.series_master_id`.
/// Only series with at least one occurrence pivoted to the workstream
/// contribute.
fn series_to_workstreams(
    conn: &Connection,
    workstream_ids: &[String],
) -> rusqlite::Result<HashMap<String, Vec<String>>> {
    if workstream_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; workstream_ids.len()].join(",");
    let sql = format!(
        "SELECT DISTINCT ce.series_master_id, ws.workstream_id \
           FROM workstream_signals ws \
           JOIN calendar_events ce ON ce.id = ws.item_id \
          WHERE ws.kind = 'event' \
            AND ws.manual_detached_ms IS NULL \
            AND ws.workstream_id IN ({placeholders}) \
            AND ce.series_master_id IS NOT NULL"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<Box<dyn rusqlite::ToSql>> = workstream_ids
        .iter()
        .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (series, ws_id) = row?;
        out.entry(series).or_default().push(ws_id);
    }
    Ok(out)
}

/// Compute the rejected-action text list per workstream. Caller passes
/// already-loaded workstreams (with `.members` populated) so we don't
/// re-derive the member set.
pub fn rejected_texts_by_workstream(
    conn: &Connection,
    workstreams: &[Workstream],
    now_ms: i64,
) -> rusqlite::Result<HashMap<String, Vec<String>>> {
    if workstreams.is_empty() {
        return Ok(HashMap::new());
    }
    let window_start = now_ms - WINDOW_MS;
    let workstream_ids: Vec<String> = workstreams.iter().map(|w| w.id.clone()).collect();

    let deletions = fetch_recent_deletions(conn, window_start)?;
    if deletions.is_empty() {
        return Ok(HashMap::new());
    }

    let pivot_index = signal_pivots(conn, &workstream_ids)?;
    let series_index = series_to_workstreams(conn, &workstream_ids)?;
    // member -> workstreams[]. A single team member often belongs to
    // multiple workstreams; the same deletion should inform all of
    // them.
    let mut member_index: HashMap<String, Vec<String>> = HashMap::new();
    for w in workstreams {
        for m in &w.members {
            member_index.entry(m.clone()).or_default().push(w.id.clone());
        }
    }

    // workstream_id -> ordered Vec<text> (most recent first, deduped).
    let mut acc: HashMap<String, Vec<String>> = HashMap::new();
    // Per-workstream seen set so a deletion attributed via two paths
    // (e.g. both signal pivot AND member subject) shows once.
    let mut seen: HashMap<String, HashSet<String>> = HashMap::new();

    for d in &deletions {
        let mut attributed: HashSet<String> = HashSet::new();

        // Source 1: synth attached via current pivots.
        if d.origin_kind == "synth" {
            if let (Some(kind), Some(item_id)) =
                (d.origin_synth_kind.as_deref(), d.origin_synth_id.as_deref())
            {
                if let Some(ws_ids) =
                    pivot_index.get(&(kind.to_string(), item_id.to_string()))
                {
                    for w in ws_ids {
                        attributed.insert(w.clone());
                    }
                }
            }
        }

        // Source 2: subject_member_id is a workstream member.
        if let Some(member_id) = d.subject_member_id.as_deref() {
            if let Some(ws_ids) = member_index.get(member_id) {
                for w in ws_ids {
                    attributed.insert(w.clone());
                }
            }
        }

        // Source 3: reconcile-origin from a series pivoted to the
        // workstream via its event signals.
        if d.origin_kind == "reconcile" {
            if let Some(series_id) = d.source_series_master_id.as_deref() {
                if let Some(ws_ids) = series_index.get(series_id) {
                    for w in ws_ids {
                        attributed.insert(w.clone());
                    }
                }
            }
        }

        if attributed.is_empty() {
            continue;
        }
        let key = normalize_for_dedup(&d.text);
        for ws_id in attributed {
            let entry = acc.entry(ws_id.clone()).or_default();
            if entry.len() >= MAX_PER_WORKSTREAM {
                continue;
            }
            let seen_set = seen.entry(ws_id).or_default();
            if seen_set.insert(key.clone()) {
                entry.push(d.text.clone());
            }
        }
    }

    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workstreams::Workstream;
    use rusqlite::params;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_workstream(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO workstreams \
                (id, title, summary, status, created_ms, updated_ms, last_activity_ms) \
             VALUES (?1, ?1, '', 'active', 0, 0, 0)",
            params![id],
        )
        .unwrap();
    }

    fn seed_signal(conn: &Connection, ws_id: &str, kind: &str, item_id: &str) {
        conn.execute(
            "INSERT INTO workstream_signals \
                (workstream_id, kind, item_id, added_ms) \
             VALUES (?1, ?2, ?3, 0)",
            params![ws_id, kind, item_id],
        )
        .unwrap();
    }

    fn seed_event(conn: &Connection, id: &str, series_master_id: Option<&str>) {
        // Minimal connector + calendar event.
        conn.execute(
            "INSERT OR IGNORE INTO connectors \
                (id, kind, display_name, enabled, config_json, \
                 created_ms, updated_ms) \
             VALUES ('mg:test', 'microsoft_graph', 'Test', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_events \
                (id, connector_id, external_id, title, start_ms, end_ms, \
                 all_day, modified_ms, series_master_id) \
             VALUES (?1, 'mg:test', ?1, 'Event', 0, 0, 0, 0, ?2)",
            params![id, series_master_id],
        )
        .unwrap();
    }

    fn make_workstream(id: &str, members: Vec<&str>) -> Workstream {
        Workstream {
            id: id.to_string(),
            title: id.to_string(),
            members: members.into_iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_deletion(
        conn: &Connection,
        deleted_ms: i64,
        origin_kind: &str,
        origin_synth_kind: Option<&str>,
        origin_synth_id: Option<&str>,
        subject_member_id: Option<&str>,
        source_series_master_id: Option<&str>,
        text: &str,
        cause: &str,
    ) {
        conn.execute(
            "INSERT INTO action_deletions \
                (deleted_ms, origin_kind, origin_synth_kind, origin_synth_id, \
                 subject_member_id, source_series_master_id, text, cause) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                deleted_ms,
                origin_kind,
                origin_synth_kind,
                origin_synth_id,
                subject_member_id,
                source_series_master_id,
                text,
                cause
            ],
        )
        .unwrap();
    }

    #[test]
    fn synth_rejected_block_pulls_from_attached_signals() {
        let conn = open_test_db();
        let now = 1_700_000_000_000;
        seed_workstream(&conn, "ws1");
        // The email signal currently pivots to ws1. A synth deletion on
        // the same (kind, item_id) attributes to ws1.
        seed_signal(&conn, "ws1", "email", "msg:42");
        seed_deletion(
            &conn,
            now - 1_000,
            "synth",
            Some("email"),
            Some("msg:42"),
            None,
            None,
            "Send the Q3 budget",
            "user_delete",
        );

        let ws = make_workstream("ws1", vec![]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        assert_eq!(out.get("ws1").map(|v| v.as_slice()), Some(&["Send the Q3 budget".to_string()][..]));
    }

    #[test]
    fn synth_rejected_block_includes_member_subject_rows() {
        let conn = open_test_db();
        let now = 1_700_000_000_000;
        seed_workstream(&conn, "ws1");
        seed_deletion(
            &conn,
            now - 1_000,
            "synth",
            Some("teams_waiting"),
            Some("ref:99"),
            Some("tm:heike"),
            None,
            "Follow up with Heike",
            "user_dismiss",
        );

        let ws = make_workstream("ws1", vec!["tm:heike"]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        assert_eq!(
            out.get("ws1").map(Vec::as_slice),
            Some(&["Follow up with Heike".to_string()][..]),
            "member-subject deletions must surface on the member's workstream"
        );
    }

    #[test]
    fn synth_rejected_block_includes_reconcile_series_rows() {
        let conn = open_test_db();
        let now = 1_700_000_000_000;
        seed_workstream(&conn, "ws1");
        // Recurring meeting: an event in series 'series:weekly'
        // currently pivots to ws1.
        seed_event(&conn, "ev:1", Some("series:weekly"));
        seed_signal(&conn, "ws1", "event", "ev:1");
        seed_deletion(
            &conn,
            now - 1_000,
            "reconcile",
            None,
            None,
            None,
            Some("series:weekly"),
            "Confirm next week's agenda",
            "user_delete",
        );

        let ws = make_workstream("ws1", vec![]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        assert_eq!(
            out.get("ws1").map(Vec::as_slice),
            Some(&["Confirm next week's agenda".to_string()][..])
        );
    }

    #[test]
    fn synth_rejected_block_excludes_auto_resolved() {
        let conn = open_test_db();
        let now = 1_700_000_000_000;
        seed_workstream(&conn, "ws1");
        seed_signal(&conn, "ws1", "email", "msg:42");
        seed_deletion(
            &conn,
            now - 1_000,
            "synth",
            Some("email"),
            Some("msg:42"),
            None,
            None,
            "Auto-resolved sweep",
            "auto_resolved",
        );

        let ws = make_workstream("ws1", vec![]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        assert!(
            out.get("ws1").is_none(),
            "auto_resolved deletions must not feed the synth prompt"
        );
    }

    #[test]
    fn synth_rejected_block_respects_window_and_cap() {
        let conn = open_test_db();
        // 100 days lets us seed both in-window and out-of-window rows
        // without going negative on deleted_ms.
        let now: i64 = 100 * 24 * 60 * 60 * 1000;
        seed_workstream(&conn, "ws1");
        seed_signal(&conn, "ws1", "email", "msg:keep");
        seed_signal(&conn, "ws1", "email", "msg:stale");
        // In-window: way over the cap, so the trim kicks in.
        for i in 0..(MAX_PER_WORKSTREAM + 5) {
            seed_deletion(
                &conn,
                now - (i as i64) * 1_000,
                "synth",
                Some("email"),
                Some("msg:keep"),
                None,
                None,
                &format!("Recent item #{i:02}"),
                "user_delete",
            );
        }
        // Out of window: 60 days old.
        seed_deletion(
            &conn,
            now - 60 * 24 * 60 * 60 * 1000,
            "synth",
            Some("email"),
            Some("msg:stale"),
            None,
            None,
            "Stale item",
            "user_delete",
        );

        let ws = make_workstream("ws1", vec![]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        let texts = out.get("ws1").unwrap();
        assert_eq!(texts.len(), MAX_PER_WORKSTREAM, "must cap at MAX_PER_WORKSTREAM");
        // Newest survives, oldest in-window also makes the cap of 15
        // because we only seeded 20 in-window.
        assert!(texts.iter().any(|t| t.contains("#00")));
        assert!(!texts.iter().any(|t| t == "Stale item"), "stale row must be dropped by window");
        // Past the cap, the older rows drop. With 20 in-window and cap 15,
        // items #15-#19 should NOT appear.
        assert!(
            !texts.iter().any(|t| t.contains(&format!("#{:02}", MAX_PER_WORKSTREAM + 4))),
            "tail must drop past the cap"
        );
    }

    #[test]
    fn synth_rejected_block_dedupes_across_attribution_paths() {
        let conn = open_test_db();
        let now = 1_700_000_000_000;
        seed_workstream(&conn, "ws1");
        seed_signal(&conn, "ws1", "email", "msg:42");
        // Same row matches via BOTH the signal-pivot path AND the
        // member-subject path. It should appear exactly once.
        seed_deletion(
            &conn,
            now - 1_000,
            "synth",
            Some("email"),
            Some("msg:42"),
            Some("tm:heike"),
            None,
            "Send the Q3 budget",
            "user_delete",
        );

        let ws = make_workstream("ws1", vec!["tm:heike"]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        let texts = out.get("ws1").unwrap();
        assert_eq!(
            texts.len(),
            1,
            "a row matching multiple attribution paths must dedupe to one entry"
        );
    }

    #[test]
    fn synth_rejected_block_ignores_detached_signals() {
        let conn = open_test_db();
        let now = 1_700_000_000_000;
        seed_workstream(&conn, "ws1");
        seed_signal(&conn, "ws1", "email", "msg:42");
        // Mark the pivot detached — the user pulled this signal off ws1.
        conn.execute(
            "UPDATE workstream_signals SET manual_detached_ms = ?1 \
              WHERE workstream_id = 'ws1' AND kind = 'email' AND item_id = 'msg:42'",
            params![now - 500],
        )
        .unwrap();
        seed_deletion(
            &conn,
            now - 1_000,
            "synth",
            Some("email"),
            Some("msg:42"),
            None,
            None,
            "Stale rejection",
            "user_delete",
        );

        let ws = make_workstream("ws1", vec![]);
        let out = rejected_texts_by_workstream(&conn, &[ws], now).unwrap();
        assert!(
            out.get("ws1").is_none(),
            "detached signals must not pull rejections back into the workstream"
        );
    }
}
