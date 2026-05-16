//! Daily activity rollup (#110).
//!
//! Single read-only Tauri command `get_daily_activity` that returns a
//! `DailyActivitySummary` of today's numbers. Pure SQL — no LLM, no
//! caching, no background polling. The frontend popover refetches on
//! every open. All queries are indexed point-lookups against the
//! entity tables; expected runtime is sub-50ms even on large DBs.

use std::sync::Mutex;

use chrono::{Local, TimeZone};
use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::State;

#[derive(Debug, Serialize, Default, Clone)]
pub struct DailyActivitySummary {
    pub day_start_ms: i64,
    pub day_end_ms: i64,
    pub now_ms: i64,
    pub emails_today: u32,
    pub emails_actionable: u32,
    pub teams_messages_today: u32,
    pub meetings_held: u32,
    pub meetings_upcoming: u32,
    pub meetings_missing_note: u32,
    pub people_interacted: u32,
    /// Currently-unresolved questions across all notes (#113).
    pub open_questions_count: u32,
}

#[tauri::command]
pub fn get_daily_activity(
    conn: State<'_, Mutex<Connection>>,
) -> Result<DailyActivitySummary, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    compute_daily_activity(&c).map_err(|e| e.to_string())
}

/// Single event row surfaced in the daily activity popover's
/// "RECENT" section (#116). Today-windowed, capped at 20, hydrated
/// with the actor's display name and (for observation_accepted) the
/// observation body. Filtered Rust-side to drop orphan-actor rows
/// and accepts whose status drifted to `rejected`.
#[derive(Debug, Serialize, Clone)]
pub struct ActivityEventRow {
    pub ts_ms: i64,
    pub kind: String,
    pub actor_id: String,
    pub actor_display_name: String,
    pub ref_kind: String,
    pub ref_id: String,
    pub body: String,
    pub current_status: Option<String>,
}

const RECENT_ACTIVITY_CAP: usize = 20;

#[tauri::command]
pub fn list_recent_activity(
    conn: State<'_, Mutex<Connection>>,
) -> Result<Vec<ActivityEventRow>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let (day_start_ms, day_end_ms, _now_ms) = today_window();
    compute_recent_activity(&c, day_start_ms, day_end_ms).map_err(|e| e.to_string())
}

pub(crate) fn compute_recent_activity(
    conn: &Connection,
    day_start_ms: i64,
    day_end_ms: i64,
) -> rusqlite::Result<Vec<ActivityEventRow>> {
    let mut stmt = conn.prepare(
        "SELECT \
            e.ts_ms, \
            e.kind, \
            e.actor_id, \
            COALESCE(tm.display_name, '') AS actor_display_name, \
            COALESCE(e.ref_kind, '') AS ref_kind, \
            COALESCE(e.ref_id, '') AS ref_id, \
            COALESCE(po.body, '') AS body, \
            po.status AS current_status \
           FROM events e \
           LEFT JOIN team_members tm ON tm.id = e.actor_id \
           LEFT JOIN profile_observations po \
             ON e.kind = 'observation_accepted' AND po.id = e.ref_id \
          WHERE e.ts_ms >= ?1 \
            AND e.ts_ms < ?2 \
            AND e.kind IN ('observation_accepted', 'profile_snapshot_created') \
          ORDER BY e.ts_ms DESC \
          LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![day_start_ms, day_end_ms, RECENT_ACTIVITY_CAP as i64],
        |r| {
            Ok(ActivityEventRow {
                ts_ms: r.get(0)?,
                kind: r.get(1)?,
                actor_id: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                actor_display_name: r.get(3)?,
                ref_kind: r.get(4)?,
                ref_id: r.get(5)?,
                body: r.get(6)?,
                current_status: r.get(7)?,
            })
        },
    )?;
    let mut out: Vec<ActivityEventRow> = Vec::new();
    for row in rows {
        let r = row?;
        // Drop orphaned actors (unlinked or member deleted before FK
        // CASCADE caught up). The display name carries the resolved
        // label; empty = nothing to show.
        if r.actor_display_name.is_empty() {
            continue;
        }
        // Drop accepts that have since been rejected — the user moved
        // on; the activity row would be a lie.
        if r.kind == "observation_accepted"
            && r.current_status.as_deref() == Some("rejected")
        {
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

pub(crate) fn compute_daily_activity(
    conn: &Connection,
) -> rusqlite::Result<DailyActivitySummary> {
    let (day_start_ms, day_end_ms, now_ms) = today_window();
    Ok(DailyActivitySummary {
        day_start_ms,
        day_end_ms,
        now_ms,
        emails_today: count_emails_today(conn, day_start_ms, day_end_ms)?,
        emails_actionable: count_emails_actionable_today(conn, day_start_ms, day_end_ms)?,
        teams_messages_today: count_teams_messages_today(conn, day_start_ms, day_end_ms)?,
        meetings_held: count_meetings_held(conn, day_start_ms, now_ms)?,
        meetings_upcoming: count_meetings_upcoming(conn, now_ms, day_end_ms)?,
        meetings_missing_note: count_meetings_missing_note(conn, day_start_ms, now_ms)?,
        people_interacted: count_people_interacted(conn, day_start_ms, day_end_ms)?,
        open_questions_count: count_open_questions(conn)?,
    })
}

fn count_open_questions(conn: &Connection) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM note_open_questions WHERE resolved = 0",
        [],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// `(day_start_ms, day_end_ms, now_ms)` in UTC. `day_start_ms` is the
/// user's local midnight today. Mirrors the timezone handling used by
/// `notes::*` and `reminders::*`.
pub(crate) fn today_window() -> (i64, i64, i64) {
    let local_today = Local::now().date_naive();
    let local_midnight = local_today
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    let day_start_dt = Local
        .from_local_datetime(&local_midnight)
        .single()
        .expect("non-ambiguous local midnight");
    let day_start_ms = day_start_dt.timestamp_millis();
    let day_end_ms = day_start_ms + 24 * 3600 * 1000;
    let now_ms = Local::now().timestamp_millis();
    (day_start_ms, day_end_ms, now_ms)
}

fn count_emails_today(
    conn: &Connection,
    day_start_ms: i64,
    day_end_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM email_messages \
         WHERE sent_at_ms >= ?1 AND sent_at_ms < ?2",
        params![day_start_ms, day_end_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// Actionable = received today (sender ≠ self) addressed to-or-cc me,
/// with no follow-up email from me in the same thread.
fn count_emails_actionable_today(
    conn: &Connection,
    day_start_ms: i64,
    day_end_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT em.id) FROM email_messages em \
         JOIN email_recipients er ON er.message_id = em.id \
         WHERE em.sent_at_ms >= ?1 AND em.sent_at_ms < ?2 \
           AND er.recipient_type IN ('to', 'cc') \
           AND er.team_member_id IS NOT NULL \
           AND er.team_member_id = (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1) \
           AND lower(em.from_email) NOT IN ( \
             SELECT lower(a.value) FROM team_member_aliases a \
             JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
           ) \
           AND NOT EXISTS ( \
             SELECT 1 FROM email_messages em2 \
             WHERE em2.thread_id = em.thread_id \
               AND em2.sent_at_ms > em.sent_at_ms \
               AND lower(em2.from_email) IN ( \
                 SELECT lower(a.value) FROM team_member_aliases a \
                 JOIN team_members m ON m.id = a.member_id \
                 WHERE a.kind = 'email' AND m.is_self = 1 \
               ) \
           )",
        params![day_start_ms, day_end_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

fn count_teams_messages_today(
    conn: &Connection,
    day_start_ms: i64,
    day_end_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM teams_messages \
         WHERE sent_at_ms >= ?1 AND sent_at_ms < ?2",
        params![day_start_ms, day_end_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

fn count_meetings_held(
    conn: &Connection,
    day_start_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM calendar_events \
         WHERE start_ms >= ?1 AND start_ms < ?2 \
           AND (status IS NULL OR status != 'cancelled')",
        params![day_start_ms, now_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

fn count_meetings_upcoming(
    conn: &Connection,
    now_ms: i64,
    day_end_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM calendar_events \
         WHERE start_ms >= ?1 AND start_ms < ?2 \
           AND (status IS NULL OR status != 'cancelled')",
        params![now_ms, day_end_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

fn count_meetings_missing_note(
    conn: &Connection,
    day_start_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM calendar_events \
         WHERE start_ms >= ?1 AND end_ms <= ?2 \
           AND linked_note_id IS NULL \
           AND (status IS NULL OR status != 'cancelled')",
        params![day_start_ms, now_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// Distinct people involved in today's activity, deduped by a
/// synthetic identity string `'tm:'+id` or `'em:'+lower(email)`.
/// Excludes self. Skips degenerate rows where both team_member_id
/// and email are unset.
fn count_people_interacted(
    conn: &Connection,
    day_start_ms: i64,
    day_end_ms: i64,
) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "WITH self_alias_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
            JOIN team_members m ON m.id = a.member_id \
            WHERE a.kind = 'email' AND m.is_self = 1 \
         ), \
         involved AS ( \
            SELECT \
              CASE WHEN er.team_member_id IS NOT NULL \
                   THEN 'tm:' || er.team_member_id \
                   ELSE 'em:' || lower(er.email) \
              END AS identity \
            FROM email_recipients er \
            JOIN email_messages em ON em.id = er.message_id \
            WHERE em.sent_at_ms >= ?1 AND em.sent_at_ms < ?2 \
              AND er.email IS NOT NULL AND er.email != '' \
            UNION \
            SELECT \
              CASE WHEN a.member_id IS NOT NULL \
                   THEN 'tm:' || a.member_id \
                   ELSE 'em:' || lower(em.from_email) \
              END \
            FROM email_messages em \
            LEFT JOIN team_member_aliases a \
              ON a.kind = 'email' AND lower(a.value) = lower(em.from_email) \
            WHERE em.sent_at_ms >= ?1 AND em.sent_at_ms < ?2 \
            UNION \
            SELECT \
              CASE WHEN ca.team_member_id IS NOT NULL \
                   THEN 'tm:' || ca.team_member_id \
                   ELSE 'em:' || lower(ca.email) \
              END \
            FROM calendar_attendees ca \
            JOIN calendar_events ce ON ce.id = ca.event_id \
            WHERE ce.start_ms >= ?1 AND ce.start_ms < ?2 \
            UNION \
            SELECT \
              CASE WHEN tcm.team_member_id IS NOT NULL \
                   THEN 'tm:' || tcm.team_member_id \
                   ELSE 'em:' || lower(COALESCE(tcm.email, '')) \
              END \
            FROM teams_chat_members tcm \
            WHERE tcm.chat_id IN ( \
              SELECT DISTINCT chat_id FROM teams_messages \
              WHERE sent_at_ms >= ?1 AND sent_at_ms < ?2 \
            ) \
         ) \
         SELECT COUNT(DISTINCT identity) FROM involved \
         WHERE identity != 'em:' AND identity != 'tm:' \
           AND identity != 'tm:' || COALESCE( \
               (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1), '' \
           ) \
           AND NOT ( \
             identity LIKE 'em:%' \
             AND substr(identity, 4) IN (SELECT email FROM self_alias_emails) \
           )",
        params![day_start_ms, day_end_ms],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_self(conn: &Connection, id: &str, email: &str) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES (?1, 'Me', '', 1, 0, 0)",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES (?1, 'email', ?2)",
            params![id, email],
        )
        .unwrap();
    }

    fn seed_teammate(conn: &Connection, id: &str, email: &str, display_name: &str) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES (?1, ?2, '', 0, 0, 0)",
            params![id, display_name],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES (?1, 'email', ?2)",
            params![id, email],
        )
        .unwrap();
    }

    fn seed_connector(conn: &Connection) {
        conn.execute(
            "INSERT OR IGNORE INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES ('mg:test', 'microsoft_graph', 'Test', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
    }

    fn seed_email(
        conn: &Connection,
        id: &str,
        thread_id: &str,
        from: &str,
        sent_at: i64,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO email_messages(\
                id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, ?2, 'Sub', ?3, NULL, ?4, NULL, NULL, 0, 0, NULL, ?4)",
            params![id, thread_id, from, sent_at],
        )
        .unwrap();
    }

    fn seed_email_recipient(
        conn: &Connection,
        message_id: &str,
        email: &str,
        rtype: &str,
        team_member_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO email_recipients(message_id, email, display_name, recipient_type, team_member_id) \
             VALUES (?1, ?2, NULL, ?3, ?4)",
            params![message_id, email, rtype, team_member_id],
        )
        .unwrap();
    }

    fn seed_meeting(
        conn: &Connection,
        id: &str,
        start_ms: i64,
        end_ms: i64,
        linked_note: Option<&str>,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO calendar_events(\
                id, connector_id, external_id, title, start_ms, end_ms, all_day, modified_ms, linked_note_id\
             ) VALUES (?1, 'mg:test', ?1, 'M', ?2, ?3, 0, ?2, ?4)",
            params![id, start_ms, end_ms, linked_note],
        )
        .unwrap();
    }

    fn seed_attendee(conn: &Connection, event_id: &str, email: &str, member_id: Option<&str>) {
        conn.execute(
            "INSERT INTO calendar_attendees(event_id, email, team_member_id, is_self, is_organizer) \
             VALUES (?1, ?2, ?3, 0, 0)",
            params![event_id, email, member_id],
        )
        .unwrap();
    }

    fn seed_teams_msg(conn: &Connection, id: &str, chat: &str, sent_at: i64) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO teams_messages(\
                id, connector_id, external_id, chat_id, chat_kind, chat_topic, \
                sent_at_ms, from_aad_id, from_email, from_name, \
                body_html, body_preview, reply_to_id, modified_ms, raw_etag\
             ) VALUES (?1, 'mg:test', ?1, ?2, 'oneOnOne', NULL, ?3, NULL, NULL, NULL, NULL, NULL, NULL, ?3, NULL)",
            params![id, chat, sent_at],
        )
        .unwrap();
    }

    #[test]
    fn empty_db_returns_all_zeros() {
        let conn = open_db();
        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(s.emails_today, 0);
        assert_eq!(s.emails_actionable, 0);
        assert_eq!(s.teams_messages_today, 0);
        assert_eq!(s.meetings_held, 0);
        assert_eq!(s.meetings_upcoming, 0);
        assert_eq!(s.meetings_missing_note, 0);
        assert_eq!(s.people_interacted, 0);
        assert!(s.day_start_ms > 0);
        assert!(s.day_end_ms > s.day_start_ms);
    }

    #[test]
    fn counts_today_emails_and_teams_and_meetings() {
        let conn = open_db();
        let (day_start, _, now) = today_window();
        let yesterday = day_start - 3_600_000;
        let earlier_today = day_start + 1_000;
        let later_today = now + 3_600_000;

        seed_email(&conn, "mg:test::e1", "t1", "alice@x.io", earlier_today);
        seed_email(&conn, "mg:test::e2", "t2", "bob@x.io", earlier_today);
        seed_email(&conn, "mg:test::e_y", "t3", "carl@x.io", yesterday);
        seed_teams_msg(&conn, "mg:test::tm1", "c1", earlier_today);
        seed_teams_msg(&conn, "mg:test::tm2", "c1", earlier_today);
        seed_teams_msg(&conn, "mg:test::tm3", "c1", earlier_today);
        seed_teams_msg(&conn, "mg:test::tm_y", "c1", yesterday);
        // 1 past meeting today (held), 2 upcoming today, 1 yesterday.
        seed_meeting(&conn, "mg:test::m1", earlier_today, now - 1_000, None);
        seed_meeting(&conn, "mg:test::m2", later_today, later_today + 60_000, None);
        seed_meeting(
            &conn,
            "mg:test::m3",
            later_today + 100_000,
            later_today + 200_000,
            None,
        );
        seed_meeting(&conn, "mg:test::m_y", yesterday, yesterday + 60_000, None);

        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(s.emails_today, 2);
        assert_eq!(s.teams_messages_today, 3);
        assert_eq!(s.meetings_held, 1);
        assert_eq!(s.meetings_upcoming, 2);
    }

    #[test]
    fn actionable_email_heuristic_basic() {
        let conn = open_db();
        let (day_start, _, _) = today_window();
        let earlier_today = day_start + 1_000;

        seed_self(&conn, "tm_self", "me@x.io");
        seed_email(&conn, "mg:test::e1", "thread1", "alice@x.io", earlier_today);
        seed_email_recipient(&conn, "mg:test::e1", "me@x.io", "to", Some("tm_self"));

        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(s.emails_actionable, 1, "received today, no reply yet");

        // Add a follow-up email from me in the same thread → no longer actionable.
        seed_email(
            &conn,
            "mg:test::e2",
            "thread1",
            "me@x.io",
            earlier_today + 1_000,
        );
        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(s.emails_actionable, 0, "I replied; no longer actionable");
    }

    #[test]
    fn meeting_without_note_only_counts_past_meetings() {
        let conn = open_db();
        let (day_start, _, now) = today_window();
        // Past meeting, no note → counts.
        seed_meeting(&conn, "mg:test::past1", day_start + 1_000, now - 1_000, None);
        // Past meeting WITH note → doesn't count.
        seed_meeting(
            &conn,
            "mg:test::past2",
            day_start + 2_000,
            now - 2_000,
            Some("/n/abc/note.md"),
        );
        // Upcoming meeting, no note → doesn't count.
        seed_meeting(&conn, "mg:test::up1", now + 60_000, now + 120_000, None);

        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(s.meetings_missing_note, 1);
    }

    #[test]
    fn people_dedup_across_sources() {
        let conn = open_db();
        let (day_start, _, _) = today_window();
        let now = day_start + 1_000;

        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io", "Alice");

        // Email today addressed to Alice.
        seed_email(&conn, "mg:test::e1", "t1", "me@x.io", now);
        seed_email_recipient(&conn, "mg:test::e1", "alice@x.io", "to", Some("tm_alice"));

        // Meeting today with Alice as attendee.
        seed_meeting(&conn, "mg:test::m1", now, now + 60_000, None);
        seed_attendee(&conn, "mg:test::m1", "alice@x.io", Some("tm_alice"));

        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(
            s.people_interacted, 1,
            "Alice present in two sources should dedup to 1"
        );
    }

    #[test]
    fn excludes_self_from_people_interacted() {
        let conn = open_db();
        let (day_start, _, _) = today_window();
        let now = day_start + 1_000;

        seed_self(&conn, "tm_self", "me@x.io");

        // Email I sent today (self as sender).
        seed_email(&conn, "mg:test::e1", "t1", "me@x.io", now);
        // Meeting today with self as attendee.
        seed_meeting(&conn, "mg:test::m1", now, now + 60_000, None);
        seed_attendee(&conn, "mg:test::m1", "me@x.io", Some("tm_self"));

        let s = compute_daily_activity(&conn).unwrap();
        assert_eq!(s.people_interacted, 0, "self never counts");
    }

    // ---------- list_recent_activity (#116) ----------------------------

    fn seed_note(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms, preview, body_size) \
             VALUES (?1, ?2, 'T', 0, '', 0)",
            params![id, format!("b_{id}")],
        )
        .unwrap();
    }

    fn seed_observation(
        conn: &Connection,
        id: &str,
        member_id: &str,
        note_id: &str,
        body: &str,
        status: &str,
        created_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO profile_observations \
                (id, member_id, source_note_id, body, status, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, member_id, note_id, body, status, created_ms],
        )
        .unwrap();
    }

    fn seed_event_row(
        conn: &Connection,
        ts_ms: i64,
        kind: &str,
        actor_id: Option<&str>,
        ref_kind: &str,
        ref_id: &str,
    ) {
        conn.execute(
            "INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', ?1)",
            params![ts_ms, kind, actor_id, ref_kind, ref_id],
        )
        .unwrap();
    }

    #[test]
    fn list_recent_activity_returns_today_events_descending() {
        let conn = open_db();
        let (day_start, day_end, _) = today_window();
        seed_teammate(&conn, "tm_alice", "alice@x.io", "Alice");
        seed_teammate(&conn, "tm_bob", "bob@x.io", "Bob");
        seed_note(&conn, "n1");
        seed_observation(
            &conn, "obs_1", "tm_alice", "n1", "Async-first.", "accepted",
            day_start + 1_000,
        );

        seed_event_row(
            &conn, day_start + 1_000, "observation_accepted",
            Some("tm_alice"), "observation", "obs_1",
        );
        seed_event_row(
            &conn, day_start + 2_000, "profile_snapshot_created",
            Some("tm_bob"), "person", "tm_bob",
        );

        let rows = compute_recent_activity(&conn, day_start, day_end).unwrap();
        assert_eq!(rows.len(), 2);
        // Most recent first.
        assert_eq!(rows[0].kind, "profile_snapshot_created");
        assert_eq!(rows[0].actor_display_name, "Bob");
        assert_eq!(rows[1].kind, "observation_accepted");
        assert_eq!(rows[1].actor_display_name, "Alice");
        assert_eq!(rows[1].body, "Async-first.");
        assert_eq!(rows[1].current_status.as_deref(), Some("accepted"));
    }

    #[test]
    fn list_recent_activity_drops_accepts_now_rejected() {
        let conn = open_db();
        let (day_start, day_end, _) = today_window();
        seed_teammate(&conn, "tm_alice", "alice@x.io", "Alice");
        seed_note(&conn, "n1");
        // User accepted then changed their mind — observation status is
        // now 'rejected' but the original accept event still exists.
        seed_observation(
            &conn, "obs_1", "tm_alice", "n1", "x.", "rejected", day_start + 1_000,
        );
        seed_event_row(
            &conn, day_start + 1_000, "observation_accepted",
            Some("tm_alice"), "observation", "obs_1",
        );

        let rows = compute_recent_activity(&conn, day_start, day_end).unwrap();
        assert!(rows.is_empty(), "rejected accept should not surface");
    }

    #[test]
    fn list_recent_activity_drops_null_actor_rows() {
        let conn = open_db();
        let (day_start, day_end, _) = today_window();
        // Events with no actor_id (the column is nullable). Display
        // name resolves to empty via COALESCE — Rust filter drops it.
        // FK CASCADE makes "team_member deleted but event survives"
        // impossible, so the realistic orphan case is a NULL actor.
        seed_event_row(
            &conn, day_start + 1_000, "profile_snapshot_created",
            None, "person", "tm_ghost",
        );

        let rows = compute_recent_activity(&conn, day_start, day_end).unwrap();
        assert!(rows.is_empty(), "null-actor row should not surface");
    }

    #[test]
    fn list_recent_activity_only_today_window() {
        let conn = open_db();
        let (day_start, day_end, _) = today_window();
        seed_teammate(&conn, "tm_alice", "alice@x.io", "Alice");
        // Yesterday's event.
        seed_event_row(
            &conn, day_start - 1_000, "profile_snapshot_created",
            Some("tm_alice"), "person", "tm_alice",
        );

        let rows = compute_recent_activity(&conn, day_start, day_end).unwrap();
        assert!(rows.is_empty(), "yesterday's event must not surface");
    }

    #[test]
    fn list_recent_activity_caps_at_20() {
        let conn = open_db();
        let (day_start, day_end, _) = today_window();
        seed_teammate(&conn, "tm_alice", "alice@x.io", "Alice");
        for i in 0..30 {
            seed_event_row(
                &conn, day_start + i, "profile_snapshot_created",
                Some("tm_alice"), "person", "tm_alice",
            );
        }
        let rows = compute_recent_activity(&conn, day_start, day_end).unwrap();
        assert_eq!(rows.len(), 20);
    }
}
