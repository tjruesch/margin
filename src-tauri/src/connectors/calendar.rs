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
    pub linked_note_path: Option<String>,
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

    let mut report = UpsertReport::default();
    for ev in events {
        let pre_existed = existing.contains(&ev.id);
        upsert_event(&tx, ev)?;
        if pre_existed {
            report.updated += 1;
        } else {
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
    // ON CONFLICT preserves `linked_note_path` — it's owned by the
    // user (set on first click of the event), not by the connector.
    // Re-syncing the event must not clobber it.
    tx.execute(
        "INSERT INTO calendar_events(\
            id, connector_id, external_id, title, start_ms, end_ms, all_day, \
            location, description, source_calendar, status, raw_etag, modified_ms, \
            linked_note_path\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
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
            modified_ms = excluded.modified_ms",
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
            e.linked_note_path,
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
                    linked_note_path \
             FROM calendar_events \
             WHERE start_ms BETWEEN ?1 AND ?2 AND connector_id = ?3 \
             ORDER BY start_ms ASC"
        }
        None => {
            "SELECT id, connector_id, external_id, title, start_ms, end_ms, all_day, \
                    location, description, source_calendar, status, raw_etag, modified_ms, \
                    linked_note_path \
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
            linked_note_path: r.get(13)?,
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

/// Update an event's `linked_note_path`. Called after the user clicks
/// an event card in the "Coming up" strip and we've created (or
/// rediscovered) the linked note bundle.
pub fn set_linked_note_path(
    conn: &Connection,
    event_id: &str,
    note_path: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE calendar_events SET linked_note_path = ?1 WHERE id = ?2",
        params![note_path, event_id],
    )?;
    Ok(())
}

pub fn get_event_details(
    conn: &Connection,
    event_id: &str,
) -> rusqlite::Result<Option<CalendarEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, connector_id, external_id, title, start_ms, end_ms, all_day, \
                location, description, source_calendar, status, raw_etag, modified_ms, \
                linked_note_path \
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
                linked_note_path: r.get(13)?,
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
             CREATE TABLE team_members (id TEXT PRIMARY KEY);
             CREATE TABLE connectors (id TEXT PRIMARY KEY);
             INSERT INTO connectors(id) VALUES ('mg:test');",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/009_calendar.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/010_event_note_link.sql"))
            .unwrap();
        conn
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
            linked_note_path: None,
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
    fn set_linked_note_path_round_trips_and_survives_resync() {
        let mut conn = open_test_db();
        let event = make_event("a", 5_000, &[]);
        upsert_window(&mut conn, "mg:test", &[event.clone()], 0, 100_000).unwrap();

        // User clicks the event → set linked path.
        set_linked_note_path(&conn, &event.id, "/tmp/notes/x/note.md").unwrap();
        let after_link = get_event_details(&conn, &event.id).unwrap().unwrap();
        assert_eq!(after_link.linked_note_path.as_deref(), Some("/tmp/notes/x/note.md"));

        // Now re-sync the same event with `linked_note_path: None`
        // (the connector doesn't know about the link).
        let mut resynced = event.clone();
        resynced.title = "Renamed in calendar".to_string();
        resynced.linked_note_path = None;
        upsert_window(&mut conn, "mg:test", &[resynced], 0, 100_000).unwrap();

        // Title updated, link preserved (the COALESCE skip on
        // ON CONFLICT keeps the user-set value).
        let after_resync = get_event_details(&conn, &event.id).unwrap().unwrap();
        assert_eq!(after_resync.title, "Renamed in calendar");
        assert_eq!(
            after_resync.linked_note_path.as_deref(),
            Some("/tmp/notes/x/note.md"),
            "linked_note_path must survive a re-sync that doesn't carry it"
        );
    }
}
