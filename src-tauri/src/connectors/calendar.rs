//! Provider-agnostic calendar storage layer (#63).
//!
//! Both `microsoft_graph` and (future) `google_calendar` connectors
//! map their provider-specific JSON into `CalendarEvent` and call
//! `upsert_window` to persist into the shared `calendar_events` /
//! `calendar_attendees` tables. The "Coming up" UI strip (#62) and AI
//! ask Schedule section (#64) read through `list_events_in_range` /
//! `get_event_details` without caring which connector produced the
//! data — the `connector_id` foreign key carries that.

use std::collections::HashSet;

use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CalendarEvent {
    pub id: String,
    pub connector_id: String,
    pub external_id: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub all_day: bool,
    pub location: Option<String>,
    pub description: Option<String>,
    pub source_calendar: Option<String>,
    pub status: Option<String>,
    pub raw_etag: Option<String>,
    pub modified_ms: i64,
    /// Path to the note bundle the user linked to this event (via the
    /// "Coming up" strip click handler). Set lazily on first click;
    /// preserved across re-syncs (#62). Null until the user opens the
    /// event for the first time.
    pub linked_note_id: Option<String>,
    /// Graph's master event id (namespaced `{connector_id}::{...}`)
    /// when this row is an occurrence of a recurring series; None for
    /// one-off meetings (#109). Powers `collapse_recurring` so the
    /// synth prompt / CO_ATTENDED counts / embeddings worker stop
    /// multi-counting weekly standups and recurring 1:1s.
    pub series_master_id: Option<String>,
    pub attendees: Vec<CalendarAttendee>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalendarAttendee {
    pub email: String,
    pub display_name: Option<String>,
    pub response_status: Option<String>,
    pub is_self: bool,
    pub is_organizer: bool,
    pub team_member_id: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UpsertReport {
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
}

/// Replace the events for `connector_id` in `[window_start_ms,
/// window_end_ms]` with `events`. Events outside the window are not
/// touched — this is the "rolling window" model: each sync covers a
/// fixed range relative to now.
///
/// Runs in a single transaction. Mirrors `index::reconcile`'s
/// delete-orphan pattern: build a snapshot of in-window IDs, upsert
/// the incoming set, delete in-window IDs that didn't appear.
pub fn upsert_window(
    conn: &mut Connection,
    connector_id: &str,
    events: &[CalendarEvent],
    window_start_ms: i64,
    window_end_ms: i64,
) -> rusqlite::Result<UpsertReport> {
    let tx = conn.transaction()?;

    // Snapshot existing in-window event IDs.
    let existing: HashSet<String> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM calendar_events \
             WHERE connector_id = ?1 AND start_ms BETWEEN ?2 AND ?3",
        )?;
        let rows = stmt.query_map(
            params![connector_id, window_start_ms, window_end_ms],
            |r| r.get::<_, String>(0),
        )?;
        let mut s = HashSet::new();
        for row in rows {
            s.insert(row?);
        }
        s
    };

    let incoming: HashSet<&str> = events.iter().map(|e| e.id.as_str()).collect();

    // Self team_member id — cached once per upsert pass for events
    // emission (#106). NULL when there's no `is_self` row.
    let self_id: Option<String> = tx
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();

    let mut report = UpsertReport::default();
    for ev in events {
        let pre_existed = existing.contains(&ev.id);
        upsert_event(&tx, ev)?;
        if pre_existed {
            report.updated += 1;
        } else {
            // Live event emission (#106). Actor = self (the calendar is
            // the user's own; per-attendee ATTENDED edges live in the
            // edges table separately).
            let payload = serde_json::json!({
                "title": ev.title,
                "all_day": ev.all_day,
            });
            crate::events::emit(
                &tx,
                ev.start_ms,
                "meeting",
                self_id.as_deref(),
                "event",
                &ev.id,
                &payload,
            )?;
            report.added += 1;
        }
    }

    // Delete in-window orphans (event no longer returned by the
    // upstream sync). CASCADE drops attendee rows.
    for id in existing.iter() {
        if !incoming.contains(id.as_str()) {
            tx.execute(
                "DELETE FROM calendar_events WHERE id = ?1",
                params![id],
            )?;
            report.removed += 1;
        }
    }

    tx.commit()?;
    Ok(report)
}

fn upsert_event(tx: &rusqlite::Transaction<'_>, e: &CalendarEvent) -> rusqlite::Result<()> {
    // ON CONFLICT preserves `linked_note_id` — it's owned by the
    // user (set on first click of the event), not by the connector.
    // Re-syncing the event must not clobber it.
    tx.execute(
        "INSERT INTO calendar_events(\
            id, connector_id, external_id, title, start_ms, end_ms, all_day, \
            location, description, source_calendar, status, raw_etag, modified_ms, \
            linked_note_id, series_master_id\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
         ON CONFLICT(id) DO UPDATE SET \
            title = excluded.title, \
            start_ms = excluded.start_ms, \
            end_ms = excluded.end_ms, \
            all_day = excluded.all_day, \
            location = excluded.location, \
            description = excluded.description, \
            source_calendar = excluded.source_calendar, \
            status = excluded.status, \
            raw_etag = excluded.raw_etag, \
            modified_ms = excluded.modified_ms, \
            series_master_id = excluded.series_master_id",
        params![
            e.id,
            e.connector_id,
            e.external_id,
            e.title,
            e.start_ms,
            e.end_ms,
            e.all_day as i64,
            e.location,
            e.description,
            e.source_calendar,
            e.status,
            e.raw_etag,
            e.modified_ms,
            e.linked_note_id,
            e.series_master_id,
        ],
    )?;

    // Attendees: replace wholesale. Cheaper than diffing for the
    // small attendee counts typical of meetings (< 50).
    tx.execute(
        "DELETE FROM calendar_attendees WHERE event_id = ?1",
        params![e.id],
    )?;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO calendar_attendees(\
            event_id, email, display_name, response_status, is_self, is_organizer, team_member_id\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for a in &e.attendees {
        stmt.execute(params![
            e.id,
            a.email,
            a.display_name,
            a.response_status,
            a.is_self as i64,
            a.is_organizer as i64,
            a.team_member_id,
        ])?;
    }
    Ok(())
}

/// Read events whose start time falls in `[start_ms, end_ms]`.
/// Optional `connector_id` filter. Includes attendees.
pub fn list_events_in_range(
    conn: &Connection,
    start_ms: i64,
    end_ms: i64,
    connector_id: Option<&str>,
) -> rusqlite::Result<Vec<CalendarEvent>> {
    let sql = match connector_id {
        Some(_) => {
            "SELECT id, connector_id, external_id, title, start_ms, end_ms, all_day, \
                    location, description, source_calendar, status, raw_etag, modified_ms, \
                    linked_note_id, series_master_id \
             FROM calendar_events \
             WHERE start_ms BETWEEN ?1 AND ?2 AND connector_id = ?3 \
             ORDER BY start_ms ASC"
        }
        None => {
            "SELECT id, connector_id, external_id, title, start_ms, end_ms, all_day, \
                    location, description, source_calendar, status, raw_etag, modified_ms, \
                    linked_note_id, series_master_id \
             FROM calendar_events \
             WHERE start_ms BETWEEN ?1 AND ?2 \
             ORDER BY start_ms ASC"
        }
    };
    let mut stmt = conn.prepare(sql)?;

    let row_to_event = |r: &rusqlite::Row<'_>| -> rusqlite::Result<CalendarEvent> {
        Ok(CalendarEvent {
            id: r.get(0)?,
            connector_id: r.get(1)?,
            external_id: r.get(2)?,
            title: r.get(3)?,
            start_ms: r.get(4)?,
            end_ms: r.get(5)?,
            all_day: r.get::<_, i64>(6)? != 0,
            location: r.get(7)?,
            description: r.get(8)?,
            source_calendar: r.get(9)?,
            status: r.get(10)?,
            raw_etag: r.get(11)?,
            modified_ms: r.get(12)?,
            linked_note_id: r.get(13)?,
            series_master_id: r.get(14)?,
            attendees: Vec::new(),
        })
    };

    let mut events: Vec<CalendarEvent> = match connector_id {
        Some(id) => {
            let rows = stmt.query_map(params![start_ms, end_ms, id], row_to_event)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        None => {
            let rows = stmt.query_map(params![start_ms, end_ms], row_to_event)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
    };

    // Bulk-load attendees in one query rather than N+1.
    if !events.is_empty() {
        let attendees_by_event = load_attendees_for(conn, events.iter().map(|e| e.id.as_str()))?;
        for ev in &mut events {
            if let Some(rows) = attendees_by_event.get(&ev.id) {
                ev.attendees = rows.clone();
            }
        }
    }
    Ok(events)
}

/// Update an event's `linked_note_id`. Called after the user clicks
/// an event card in the "Coming up" strip and we've created (or
/// rediscovered) the linked note bundle.
///
/// Linking a note is unambiguously a self action — only the user
/// can drive this from the UI. We emit one `counterparty_replied`
/// event per non-self attendee so the profile worker marks them
/// dirty (#121); their "past meeting without note" waiting action
/// then clears on the next tick.
pub fn set_linked_note_id(
    conn: &mut Connection,
    event_id: &str,
    note_path: &str,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE calendar_events SET linked_note_id = ?1 WHERE id = ?2",
        params![note_path, event_id],
    )?;
    let cp_ids: Vec<String> = {
        let mut cp_stmt = tx.prepare(
            "SELECT DISTINCT team_member_id FROM calendar_attendees \
              WHERE event_id = ?1 \
                AND is_self = 0 \
                AND team_member_id IS NOT NULL",
        )?;
        let rows = cp_stmt.query_map(params![event_id], |r| r.get::<_, String>(0))?;
        rows.filter_map(Result::ok).collect()
    };
    let now = crate::events::current_unix_ms();
    for cp_id in cp_ids {
        crate::events::emit(
            &tx,
            now,
            "counterparty_replied",
            Some(&cp_id),
            "meeting",
            event_id,
            &serde_json::json!({ "source": "meeting" }),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// All known occurrences of a recurring series, oldest-first, with
/// attendees attached. Used by the AI ask `read_event_series` tool and
/// the `series_summary` block on `read_event_details` (#128). Bounded
/// by whatever the calendar connector has actually synced — the index
/// on `series_master_id` (#109 migration 033) keeps the lookup cheap
/// even on multi-year stores.
pub fn list_events_by_series_id(
    conn: &Connection,
    series_master_id: &str,
) -> rusqlite::Result<Vec<CalendarEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, connector_id, external_id, title, start_ms, end_ms, all_day, \
                location, description, source_calendar, status, raw_etag, modified_ms, \
                linked_note_id, series_master_id \
         FROM calendar_events \
         WHERE series_master_id = ?1 \
         ORDER BY start_ms ASC",
    )?;
    let rows = stmt.query_map(params![series_master_id], |r| {
        Ok(CalendarEvent {
            id: r.get(0)?,
            connector_id: r.get(1)?,
            external_id: r.get(2)?,
            title: r.get(3)?,
            start_ms: r.get(4)?,
            end_ms: r.get(5)?,
            all_day: r.get::<_, i64>(6)? != 0,
            location: r.get(7)?,
            description: r.get(8)?,
            source_calendar: r.get(9)?,
            status: r.get(10)?,
            raw_etag: r.get(11)?,
            modified_ms: r.get(12)?,
            linked_note_id: r.get(13)?,
            series_master_id: r.get(14)?,
            attendees: Vec::new(),
        })
    })?;
    let mut events: Vec<CalendarEvent> = rows.collect::<Result<Vec<_>, _>>()?;
    if !events.is_empty() {
        let by_event = load_attendees_for(conn, events.iter().map(|e| e.id.as_str()))?;
        for ev in &mut events {
            if let Some(att) = by_event.get(&ev.id) {
                ev.attendees = att.clone();
            }
        }
    }
    Ok(events)
}

pub fn get_event_details(
    conn: &Connection,
    event_id: &str,
) -> rusqlite::Result<Option<CalendarEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, connector_id, external_id, title, start_ms, end_ms, all_day, \
                location, description, source_calendar, status, raw_etag, modified_ms, \
                linked_note_id, series_master_id \
         FROM calendar_events WHERE id = ?1",
    )?;
    let mut event: Option<CalendarEvent> = stmt
        .query_row(params![event_id], |r| {
            Ok(CalendarEvent {
                id: r.get(0)?,
                connector_id: r.get(1)?,
                external_id: r.get(2)?,
                title: r.get(3)?,
                start_ms: r.get(4)?,
                end_ms: r.get(5)?,
                all_day: r.get::<_, i64>(6)? != 0,
                location: r.get(7)?,
                description: r.get(8)?,
                source_calendar: r.get(9)?,
                status: r.get(10)?,
                raw_etag: r.get(11)?,
                modified_ms: r.get(12)?,
                linked_note_id: r.get(13)?,
                series_master_id: r.get(14)?,
                attendees: Vec::new(),
            })
        })
        .ok();

    if let Some(ref mut ev) = event {
        let by_event = load_attendees_for(conn, std::iter::once(ev.id.as_str()))?;
        if let Some(rows) = by_event.get(&ev.id) {
            ev.attendees = rows.clone();
        }
    }
    Ok(event)
}

/// One collapsed group from `collapse_recurring` (#109). The canonical
/// instance follows the issue's rule: prefer the next future
/// occurrence (smallest `start_ms >= now_ms`); else the most-recent
/// past one. `instance_count == 1` for one-off meetings.
#[derive(Debug, Clone)]
pub struct CollapsedEvent {
    pub canonical: CalendarEvent,
    pub instance_count: usize,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// Group events by `series_master_id` (singletons fall through with
/// `instance_count = 1`) and pick a canonical instance per group. The
/// output is sorted by `canonical.start_ms` ASC. Used by the workstream
/// synth prompt, the CO_ATTENDED edge pass (indirectly — its SQL
/// dedupes via `COALESCE(series_master_id, id)`), and the embeddings
/// worker so a single weekly standup counts once, not N times (#109).
pub fn collapse_recurring(events: Vec<CalendarEvent>, now_ms: i64) -> Vec<CollapsedEvent> {
    use std::collections::HashMap;
    let mut groups: HashMap<String, Vec<CalendarEvent>> = HashMap::new();
    let mut singletons: Vec<CalendarEvent> = Vec::new();
    for ev in events {
        match ev.series_master_id.clone() {
            Some(master) => groups.entry(master).or_default().push(ev),
            None => singletons.push(ev),
        }
    }
    let mut out: Vec<CollapsedEvent> = Vec::new();
    for ev in singletons {
        let ts = ev.start_ms;
        out.push(CollapsedEvent {
            canonical: ev,
            instance_count: 1,
            first_seen_ms: ts,
            last_seen_ms: ts,
        });
    }
    for (_, mut occurrences) in groups {
        if occurrences.is_empty() {
            continue;
        }
        // Future-leaning canonical: smallest start_ms >= now_ms, else
        // the largest start_ms < now_ms. Picks "the next meeting" as
        // the row the prompt / UI sees while the embeddings worker
        // separately uses the earliest occurrence.
        let canonical_idx = occurrences
            .iter()
            .enumerate()
            .filter(|(_, e)| e.start_ms >= now_ms)
            .min_by_key(|(_, e)| e.start_ms)
            .map(|(i, _)| i)
            .or_else(|| {
                occurrences
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, e)| e.start_ms)
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        let first_seen_ms = occurrences.iter().map(|e| e.start_ms).min().unwrap_or(0);
        let last_seen_ms = occurrences.iter().map(|e| e.start_ms).max().unwrap_or(0);
        let instance_count = occurrences.len();
        let canonical = occurrences.swap_remove(canonical_idx);
        out.push(CollapsedEvent {
            canonical,
            instance_count,
            first_seen_ms,
            last_seen_ms,
        });
    }
    out.sort_by_key(|c| c.canonical.start_ms);
    out
}

fn load_attendees_for<'a, I>(
    conn: &Connection,
    ids: I,
) -> rusqlite::Result<std::collections::HashMap<String, Vec<CalendarAttendee>>>
where
    I: IntoIterator<Item = &'a str>,
{
    let id_list: Vec<String> = ids.into_iter().map(|s| s.to_string()).collect();
    if id_list.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // SQL `IN (?, ?, ...)` with N placeholders. `rusqlite` needs the
    // placeholder count baked into the SQL; build it dynamically.
    let placeholders = std::iter::repeat("?")
        .take(id_list.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT event_id, email, display_name, response_status, is_self, is_organizer, team_member_id \
         FROM calendar_attendees WHERE event_id IN ({placeholders}) ORDER BY event_id, is_organizer DESC, email"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        id_list.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            CalendarAttendee {
                email: r.get(1)?,
                display_name: r.get(2)?,
                response_status: r.get(3)?,
                is_self: r.get::<_, i64>(4)? != 0,
                is_organizer: r.get::<_, i64>(5)? != 0,
                team_member_id: r.get(6)?,
            },
        ))
    })?;
    let mut out: std::collections::HashMap<String, Vec<CalendarAttendee>> =
        std::collections::HashMap::new();
    for row in rows {
        let (event_id, attendee) = row?;
        out.entry(event_id).or_default().push(attendee);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Replicate the relevant subset of the schema for unit tests.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE team_members (id TEXT PRIMARY KEY, is_self INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE connectors (id TEXT PRIMARY KEY);
             -- Minimal `events` stub for #106 live emission. Real schema
             -- lives in migration 022; this fixture skips that ladder.
             CREATE TABLE events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts_ms INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 actor_id TEXT,
                 ref_kind TEXT,
                 ref_id TEXT,
                 payload TEXT,
                 created_ms INTEGER NOT NULL
             );
             INSERT INTO connectors(id) VALUES ('mg:test');",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/009_calendar.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/010_event_note_link.sql"))
            .unwrap();
        // #112 renamed linked_note_path → linked_note_id. The fixture
        // re-creates only the column here (the rest of v26 doesn't
        // affect calendar tests). The DROP INDEX is required by
        // SQLite — DROP COLUMN refuses while an index references the
        // about-to-vanish column.
        conn.execute_batch(
            "ALTER TABLE calendar_events ADD COLUMN linked_note_id TEXT;\
             DROP INDEX IF EXISTS idx_events_linked_note;\
             ALTER TABLE calendar_events DROP COLUMN linked_note_path;",
        )
        .unwrap();
        // #109 added series_master_id; apply the migration so the
        // hydrating SELECT paths can return the new column.
        conn.execute_batch(include_str!(
            "../migrations/033_calendar_series_master_id.sql"
        ))
        .unwrap();
        conn
    }

    #[test]
    fn upsert_window_emits_meeting_event() {
        let mut conn = open_test_db();
        let event = make_event("e1", 1_000, &[]);
        let r = upsert_window(&mut conn, "mg:test", &[event.clone()], 0, 10_000).unwrap();
        assert_eq!(r.added, 1);

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ref_kind = 'event' AND kind = 'meeting'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // Re-upsert the same event: no new emission.
        let r2 = upsert_window(&mut conn, "mg:test", &[event], 0, 10_000).unwrap();
        assert_eq!(r2.added, 0);
        assert_eq!(r2.updated, 1);
        let n2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ref_kind = 'event'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n2, 1);
    }

    fn make_event(id: &str, start_ms: i64, attendee_emails: &[&str]) -> CalendarEvent {
        CalendarEvent {
            id: format!("mg:test::{id}"),
            connector_id: "mg:test".to_string(),
            external_id: id.to_string(),
            title: format!("Event {id}"),
            start_ms,
            end_ms: start_ms + 30 * 60 * 1000,
            all_day: false,
            location: None,
            description: None,
            source_calendar: None,
            status: Some("confirmed".to_string()),
            raw_etag: None,
            modified_ms: start_ms,
            linked_note_id: None,
            series_master_id: None,
            attendees: attendee_emails
                .iter()
                .map(|e| CalendarAttendee {
                    email: e.to_string(),
                    display_name: None,
                    response_status: None,
                    is_self: false,
                    is_organizer: false,
                    team_member_id: None,
                })
                .collect(),
        }
    }

    #[test]
    fn upsert_window_inserts_and_deletes_orphans() {
        let mut conn = open_test_db();
        let window_start = 1_000;
        let window_end = 100_000;

        // First sync: 3 events.
        let first = vec![
            make_event("a", 5_000, &["alice@example.com"]),
            make_event("b", 6_000, &[]),
            make_event("c", 7_000, &["bob@example.com", "alice@example.com"]),
        ];
        let report1 =
            upsert_window(&mut conn, "mg:test", &first, window_start, window_end).unwrap();
        assert_eq!(report1.added, 3);
        assert_eq!(report1.updated, 0);
        assert_eq!(report1.removed, 0);

        // Second sync: drop "b", update "a", add "d". "c" stays.
        let mut updated_a = make_event("a", 5_000, &["alice@example.com"]);
        updated_a.title = "Event a (renamed)".to_string();
        let second = vec![
            updated_a,
            make_event("c", 7_000, &["bob@example.com", "alice@example.com"]),
            make_event("d", 8_000, &[]),
        ];
        let report2 =
            upsert_window(&mut conn, "mg:test", &second, window_start, window_end).unwrap();
        assert_eq!(report2.added, 1, "d is new");
        assert_eq!(report2.updated, 2, "a + c existed");
        assert_eq!(report2.removed, 1, "b was orphaned");

        // Verify final state via list_events_in_range.
        let rows = list_events_in_range(&conn, window_start, window_end, Some("mg:test")).unwrap();
        let titles: Vec<&str> = rows.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Event a (renamed)", "Event c", "Event d"]);

        // a's attendees survived the upsert.
        let a = rows.iter().find(|e| e.external_id == "a").unwrap();
        assert_eq!(a.attendees.len(), 1);
        assert_eq!(a.attendees[0].email, "alice@example.com");
    }

    /// `series_master_id` round-trips through the INSERT and the
    /// hydrating SELECT paths (#109).
    #[test]
    fn upsert_window_persists_series_master_id() {
        let mut conn = open_test_db();
        let mut event = make_event("recurring", 5_000, &[]);
        event.series_master_id = Some("mg:test::master-1".into());
        upsert_window(&mut conn, "mg:test", &[event], 0, 100_000).unwrap();

        let got = get_event_details(&conn, "mg:test::recurring")
            .unwrap()
            .unwrap();
        assert_eq!(got.series_master_id.as_deref(), Some("mg:test::master-1"));

        let in_range = list_events_in_range(&conn, 0, 100_000, Some("mg:test")).unwrap();
        assert_eq!(in_range.len(), 1);
        assert_eq!(
            in_range[0].series_master_id.as_deref(),
            Some("mg:test::master-1")
        );
    }

    #[test]
    fn upsert_window_does_not_touch_outside_window_events() {
        let mut conn = open_test_db();
        // Pre-populate an event outside our window.
        upsert_window(
            &mut conn,
            "mg:test",
            &[make_event("outside", 999_999, &[])],
            999_990,
            1_000_000,
        )
        .unwrap();

        // Sync a tighter window. "outside" should survive.
        upsert_window(&mut conn, "mg:test", &[make_event("a", 5_000, &[])], 1_000, 100_000)
            .unwrap();

        let still_there =
            list_events_in_range(&conn, 999_990, 1_000_000, Some("mg:test")).unwrap();
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].external_id, "outside");
    }

    #[test]
    fn list_events_in_range_filters_by_connector() {
        let mut conn = open_test_db();
        // Add a second connector row.
        conn.execute("INSERT INTO connectors(id) VALUES ('gc:test')", [])
            .unwrap();

        upsert_window(&mut conn, "mg:test", &[make_event("a", 5_000, &[])], 0, 100_000).unwrap();

        let mut other = make_event("b", 6_000, &[]);
        other.id = "gc:test::b".to_string();
        other.connector_id = "gc:test".to_string();
        upsert_window(&mut conn, "gc:test", &[other], 0, 100_000).unwrap();

        let mg_only = list_events_in_range(&conn, 0, 100_000, Some("mg:test")).unwrap();
        assert_eq!(mg_only.len(), 1);
        assert_eq!(mg_only[0].connector_id, "mg:test");

        let all = list_events_in_range(&conn, 0, 100_000, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn get_event_details_returns_none_for_missing() {
        let conn = open_test_db();
        let result = get_event_details(&conn, "nope").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn set_linked_note_id_round_trips_and_survives_resync() {
        let mut conn = open_test_db();
        let event = make_event("a", 5_000, &[]);
        upsert_window(&mut conn, "mg:test", &[event.clone()], 0, 100_000).unwrap();

        // User clicks the event → set linked path.
        set_linked_note_id(&mut conn, &event.id, "/tmp/notes/x/note.md").unwrap();
        let after_link = get_event_details(&conn, &event.id).unwrap().unwrap();
        assert_eq!(after_link.linked_note_id.as_deref(), Some("/tmp/notes/x/note.md"));

        // Now re-sync the same event with `linked_note_id: None`
        // (the connector doesn't know about the link).
        let mut resynced = event.clone();
        resynced.title = "Renamed in calendar".to_string();
        resynced.linked_note_id = None;
        upsert_window(&mut conn, "mg:test", &[resynced], 0, 100_000).unwrap();

        // Title updated, link preserved (the COALESCE skip on
        // ON CONFLICT keeps the user-set value).
        let after_resync = get_event_details(&conn, &event.id).unwrap().unwrap();
        assert_eq!(after_resync.title, "Renamed in calendar");
        assert_eq!(
            after_resync.linked_note_id.as_deref(),
            Some("/tmp/notes/x/note.md"),
            "linked_note_id must survive a re-sync that doesn't carry it"
        );
    }

    fn cp_meeting_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM events \
              WHERE kind = 'counterparty_replied' AND ref_kind = 'meeting'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Linking a note to a meeting marks every non-self attendee
    /// with a resolved `team_member_id` dirty (#121).
    #[test]
    fn set_linked_note_id_emits_counterparty_replied_per_attendee() {
        let mut conn = open_test_db();
        conn.execute(
            "INSERT INTO team_members(id, is_self) VALUES \
                ('tm:self', 1), ('tm:alice', 0), ('tm:bob', 0)",
            [],
        )
        .unwrap();

        let event = CalendarEvent {
            id: "mg:test::e1".into(),
            connector_id: "mg:test".into(),
            external_id: "e1".into(),
            title: "Sync".into(),
            start_ms: 5_000,
            end_ms: 6_000,
            all_day: false,
            location: None,
            description: None,
            source_calendar: None,
            status: Some("confirmed".into()),
            raw_etag: None,
            modified_ms: 5_000,
            linked_note_id: None,
            series_master_id: None,
            attendees: vec![
                CalendarAttendee {
                    email: "me@x.io".into(),
                    display_name: None,
                    response_status: None,
                    is_self: true,
                    is_organizer: true,
                    team_member_id: Some("tm:self".into()),
                },
                CalendarAttendee {
                    email: "alice@x.io".into(),
                    display_name: None,
                    response_status: None,
                    is_self: false,
                    is_organizer: false,
                    team_member_id: Some("tm:alice".into()),
                },
                CalendarAttendee {
                    email: "bob@x.io".into(),
                    display_name: None,
                    response_status: None,
                    is_self: false,
                    is_organizer: false,
                    team_member_id: Some("tm:bob".into()),
                },
            ],
        };
        upsert_window(&mut conn, "mg:test", &[event], 0, 100_000).unwrap();

        set_linked_note_id(&mut conn, "mg:test::e1", "/tmp/notes/sync.md").unwrap();

        let mut actors: Vec<String> = conn
            .prepare(
                "SELECT actor_id FROM events \
                  WHERE kind = 'counterparty_replied' AND ref_kind = 'meeting'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        actors.sort();
        assert_eq!(actors, vec!["tm:alice".to_string(), "tm:bob".to_string()]);
    }

    /// External attendees (no `team_member_id`) must not produce
    /// `counterparty_replied` rows.
    #[test]
    fn set_linked_note_id_skips_external_attendees() {
        let mut conn = open_test_db();

        let event = CalendarEvent {
            id: "mg:test::e2".into(),
            connector_id: "mg:test".into(),
            external_id: "e2".into(),
            title: "External meeting".into(),
            start_ms: 5_000,
            end_ms: 6_000,
            all_day: false,
            location: None,
            description: None,
            source_calendar: None,
            status: Some("confirmed".into()),
            raw_etag: None,
            modified_ms: 5_000,
            linked_note_id: None,
            series_master_id: None,
            attendees: vec![CalendarAttendee {
                email: "vendor@external.com".into(),
                display_name: None,
                response_status: None,
                is_self: false,
                is_organizer: false,
                team_member_id: None,
            }],
        };
        upsert_window(&mut conn, "mg:test", &[event], 0, 100_000).unwrap();

        set_linked_note_id(&mut conn, "mg:test::e2", "/tmp/notes/ext.md").unwrap();
        assert_eq!(cp_meeting_count(&conn), 0);
    }

    // ---------- list_events_by_series_id (#128) ---------------------------

    #[test]
    fn list_events_by_series_id_returns_occurrences_with_attendees() {
        let mut conn = open_test_db();
        let mut master_a1 = make_event("a1", 1_000, &["alice@x.io"]);
        master_a1.series_master_id = Some("master-A".to_string());
        let mut master_a2 = make_event("a2", 2_000, &["alice@x.io", "bob@x.io"]);
        master_a2.series_master_id = Some("master-A".to_string());
        let oneoff = make_event("z", 3_000, &["eve@x.io"]); // unrelated singleton
        let mut master_b = make_event("b1", 4_000, &["x@x.io"]);
        master_b.series_master_id = Some("master-B".to_string());

        upsert_window(
            &mut conn,
            "mg:test",
            &[master_a1, master_a2, oneoff, master_b],
            0,
            10_000,
        )
        .unwrap();

        let rows = list_events_by_series_id(&conn, "master-A").unwrap();
        assert_eq!(rows.len(), 2, "two A occurrences");
        // Ordered by start_ms ASC.
        assert_eq!(rows[0].external_id, "a1");
        assert_eq!(rows[1].external_id, "a2");
        // Attendees loaded per row.
        let a2_emails: Vec<&str> = rows[1].attendees.iter().map(|a| a.email.as_str()).collect();
        assert!(a2_emails.contains(&"alice@x.io"));
        assert!(a2_emails.contains(&"bob@x.io"));
    }

    #[test]
    fn list_events_by_series_id_returns_empty_for_unknown_master() {
        let conn = open_test_db();
        let rows = list_events_by_series_id(&conn, "no-such-master").unwrap();
        assert!(rows.is_empty());
    }

    // ---------- collapse_recurring (#109) ---------------------------------

    fn ev_in_series(id: &str, start_ms: i64, series: Option<&str>) -> CalendarEvent {
        let mut e = make_event(id, start_ms, &[]);
        e.series_master_id = series.map(|s| s.to_string());
        e
    }

    /// Singletons (`series_master_id IS NULL`) pass through with
    /// `instance_count = 1`. Output is sorted by canonical start_ms.
    #[test]
    fn collapse_recurring_returns_events_unchanged_when_no_series() {
        let a = ev_in_series("a", 1_000, None);
        let b = ev_in_series("b", 3_000, None);
        let c = ev_in_series("c", 2_000, None);
        let out = collapse_recurring(vec![a, b, c], 5_000);
        assert_eq!(out.len(), 3);
        let starts: Vec<i64> = out.iter().map(|e| e.canonical.start_ms).collect();
        assert_eq!(starts, vec![1_000, 2_000, 3_000]);
        assert!(out.iter().all(|e| e.instance_count == 1));
    }

    /// When at least one occurrence is in the future, the next future
    /// one becomes the canonical row.
    #[test]
    fn collapse_recurring_keeps_next_future_occurrence_for_recurring_series() {
        // now = 5_000; one past, two future. Canonical = future #1.
        let past = ev_in_series("past", 1_000, Some("mg:test::m1"));
        let future1 = ev_in_series("f1", 7_000, Some("mg:test::m1"));
        let future2 = ev_in_series("f2", 9_000, Some("mg:test::m1"));
        let out = collapse_recurring(vec![past, future1, future2], 5_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].canonical.external_id, "f1");
        assert_eq!(out[0].instance_count, 3);
        assert_eq!(out[0].first_seen_ms, 1_000);
        assert_eq!(out[0].last_seen_ms, 9_000);
    }

    /// All occurrences in the past → the most-recent past one is
    /// canonical (largest start_ms < now_ms).
    #[test]
    fn collapse_recurring_falls_back_to_most_recent_past_when_no_future_occurrence() {
        // now = 10_000; everything past.
        let earliest = ev_in_series("p1", 1_000, Some("mg:test::m1"));
        let middle = ev_in_series("p2", 4_000, Some("mg:test::m1"));
        let latest_past = ev_in_series("p3", 7_000, Some("mg:test::m1"));
        let out = collapse_recurring(vec![earliest, middle, latest_past], 10_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].canonical.external_id, "p3");
        assert_eq!(out[0].instance_count, 3);
    }

    /// Mixed input: two recurring series + a singleton → three groups
    /// in the output, each carrying its own count.
    #[test]
    fn collapse_recurring_handles_multiple_series_independently() {
        let m1_a = ev_in_series("m1-a", 1_000, Some("mg:test::m1"));
        let m1_b = ev_in_series("m1-b", 4_000, Some("mg:test::m1"));
        let m2_a = ev_in_series("m2-a", 2_000, Some("mg:test::m2"));
        let m2_b = ev_in_series("m2-b", 3_000, Some("mg:test::m2"));
        let m2_c = ev_in_series("m2-c", 5_000, Some("mg:test::m2"));
        let singleton = ev_in_series("solo", 6_000, None);
        let out = collapse_recurring(
            vec![m1_a, m1_b, m2_a, m2_b, m2_c, singleton],
            10_000,
        );
        assert_eq!(out.len(), 3);
        // Sorted by canonical start_ms ASC.
        let labels: Vec<(&str, usize)> = out
            .iter()
            .map(|c| (c.canonical.external_id.as_str(), c.instance_count))
            .collect();
        assert_eq!(
            labels,
            vec![("m1-b", 2), ("m2-c", 3), ("solo", 1)]
        );
    }
}
