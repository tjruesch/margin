//! Waiting-direction signal mining for the v3 profile worker (#120).
//!
//! The hybrid: deterministic SQL surfaces candidate "waiting" items
//! per direction from email_messages, teams_messages, and
//! calendar_events; the worker prompt hands them to Claude, which
//! filters out resolved/stale ones, rephrases the preview into a
//! one-sentence description, and emits the most consequential as
//! `WaitingItem`s on the snapshot body.
//!
//! Conventions:
//!   - **from_me** = items the team member is waiting on the user for
//!                   (you owe them).
//!   - **for_them** = items the user is waiting on the team member for
//!                    (they owe you).
//!
//! All queries are recency-windowed (`RECENCY_WINDOW_MS`) and capped
//! per direction (`CANDIDATES_PER_DIRECTION_CAP`). Meeting candidates
//! are sub-capped so they can't crowd out higher-signal email/Teams
//! items.

use rusqlite::{params, Connection};
use serde::Serialize;

pub const RECENCY_WINDOW_MS: i64 = 30 * 24 * 3_600 * 1_000;
pub const CANDIDATES_PER_DIRECTION_CAP: usize = 20;
pub const MEETING_SUBCAP: usize = 5;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct WaitingCandidate {
    pub source_kind: String,
    pub source_ref_id: String,
    pub since_ms: i64,
    pub preview: String,
}

/// Items the team member is waiting on the user to act on. Combines
/// inbound email candidates, inbound Teams candidates, and past
/// meetings that lack a note. Sorted by recency desc, capped.
pub fn candidates_from_me(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let cutoff = now_ms - RECENCY_WINDOW_MS;
    let mut out = Vec::new();
    out.extend(inbound_email(conn, person_id, cutoff)?);
    out.extend(inbound_teams(conn, person_id, cutoff)?);
    out.extend(meeting_past_without_note(
        conn, person_id, cutoff, now_ms, MEETING_SUBCAP,
    )?);
    sort_and_cap(&mut out, CANDIDATES_PER_DIRECTION_CAP);
    Ok(out)
}

/// Items the user is waiting on the team member to act on. Combines
/// outbound email candidates, outbound Teams candidates, and future
/// meetings the member organized that the user hasn't accepted.
pub fn candidates_for_them(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let cutoff = now_ms - RECENCY_WINDOW_MS;
    let mut out = Vec::new();
    out.extend(outbound_email(conn, person_id, cutoff)?);
    out.extend(outbound_teams(conn, person_id, cutoff)?);
    out.extend(meeting_future_unaccepted(
        conn, person_id, now_ms, MEETING_SUBCAP,
    )?);
    sort_and_cap(&mut out, CANDIDATES_PER_DIRECTION_CAP);
    Ok(out)
}

fn sort_and_cap(items: &mut Vec<WaitingCandidate>, cap: usize) {
    items.sort_by(|a, b| b.since_ms.cmp(&a.since_ms));
    items.truncate(cap);
}

// ---------- Email ---------------------------------------------------------

/// Person → self, no self reply in the same thread, within the recency
/// window. Uses the `self_alias_emails` CTE pattern from `activity.rs`
/// to identify the user across address forms.
fn inbound_email(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
             WHERE a.kind = 'email' AND a.member_id = ?1 \
        ) \
        SELECT em.id, em.sent_at_ms, \
               COALESCE(NULLIF(em.body_preview, ''), em.subject, '') AS preview \
          FROM email_messages em \
         WHERE em.sent_at_ms >= ?2 \
           AND lower(em.from_email) IN (SELECT email FROM their_emails) \
           AND EXISTS ( \
                SELECT 1 FROM email_recipients er \
                 WHERE er.message_id = em.id \
                   AND er.recipient_type IN ('to', 'cc') \
                   AND ( \
                       er.team_member_id = (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1) \
                       OR lower(er.email) IN (SELECT email FROM self_emails) \
                   ) \
           ) \
           AND NOT EXISTS ( \
                SELECT 1 FROM email_messages em2 \
                 WHERE em2.thread_id = em.thread_id \
                   AND em2.sent_at_ms > em.sent_at_ms \
                   AND lower(em2.from_email) IN (SELECT email FROM self_emails) \
           ) \
         ORDER BY em.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_email_candidate,
    )?;
    collect_rows(rows)
}

/// Self → person, no reply from the person in the same thread.
fn outbound_email(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
             WHERE a.kind = 'email' AND a.member_id = ?1 \
        ) \
        SELECT em.id, em.sent_at_ms, \
               COALESCE(NULLIF(em.body_preview, ''), em.subject, '') AS preview \
          FROM email_messages em \
         WHERE em.sent_at_ms >= ?2 \
           AND lower(em.from_email) IN (SELECT email FROM self_emails) \
           AND EXISTS ( \
                SELECT 1 FROM email_recipients er \
                 WHERE er.message_id = em.id \
                   AND er.recipient_type IN ('to', 'cc') \
                   AND ( \
                       er.team_member_id = ?1 \
                       OR lower(er.email) IN (SELECT email FROM their_emails) \
                   ) \
           ) \
           AND NOT EXISTS ( \
                SELECT 1 FROM email_messages em2 \
                 WHERE em2.thread_id = em.thread_id \
                   AND em2.sent_at_ms > em.sent_at_ms \
                   AND lower(em2.from_email) IN (SELECT email FROM their_emails) \
           ) \
         ORDER BY em.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_email_candidate,
    )?;
    collect_rows(rows)
}

fn row_to_email_candidate(r: &rusqlite::Row<'_>) -> rusqlite::Result<WaitingCandidate> {
    Ok(WaitingCandidate {
        source_kind: "email".into(),
        source_ref_id: r.get(0)?,
        since_ms: r.get(1)?,
        preview: r.get(2)?,
    })
}

// ---------- Teams ---------------------------------------------------------

/// Person in a chat → message; self hasn't posted in that chat since.
/// Matches the person via `teams_chat_members` (both email and aad_id
/// paths) so we catch messages even when `from_email` is NULL.
fn inbound_teams(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_chats AS ( \
            SELECT DISTINCT chat_id FROM teams_chat_members \
             WHERE team_member_id = ?1 \
        ) \
        SELECT tm.id, tm.sent_at_ms, COALESCE(tm.body_preview, '') AS preview \
          FROM teams_messages tm \
          JOIN teams_chat_members tcm \
            ON tcm.chat_id = tm.chat_id AND tcm.team_member_id = ?1 \
         WHERE tm.sent_at_ms >= ?2 \
           AND tm.chat_id IN (SELECT chat_id FROM their_chats) \
           AND ( \
                (tm.from_aad_id IS NOT NULL AND tm.from_aad_id = tcm.aad_id) \
                OR ( \
                    tm.from_email IS NOT NULL \
                    AND tcm.email IS NOT NULL \
                    AND lower(tm.from_email) = lower(tcm.email) \
                ) \
           ) \
           AND NOT EXISTS ( \
                SELECT 1 FROM teams_messages tm2 \
                 WHERE tm2.chat_id = tm.chat_id \
                   AND tm2.sent_at_ms > tm.sent_at_ms \
                   AND ( \
                       lower(COALESCE(tm2.from_email, '')) IN (SELECT email FROM self_emails) \
                       OR EXISTS ( \
                           SELECT 1 FROM teams_chat_members scm \
                            WHERE scm.chat_id = tm2.chat_id \
                              AND scm.is_self = 1 \
                              AND tm2.from_aad_id IS NOT NULL \
                              AND tm2.from_aad_id = scm.aad_id \
                       ) \
                   ) \
           ) \
         ORDER BY tm.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_teams_candidate,
    )?;
    collect_rows(rows)
}

/// Self → person in a chat; person hasn't posted in that chat since.
fn outbound_teams(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_chats AS ( \
            SELECT DISTINCT chat_id FROM teams_chat_members \
             WHERE team_member_id = ?1 \
        ) \
        SELECT tm.id, tm.sent_at_ms, COALESCE(tm.body_preview, '') AS preview \
          FROM teams_messages tm \
          JOIN teams_chat_members scm \
            ON scm.chat_id = tm.chat_id AND scm.is_self = 1 \
         WHERE tm.sent_at_ms >= ?2 \
           AND tm.chat_id IN (SELECT chat_id FROM their_chats) \
           AND ( \
                lower(COALESCE(tm.from_email, '')) IN (SELECT email FROM self_emails) \
                OR (tm.from_aad_id IS NOT NULL AND tm.from_aad_id = scm.aad_id) \
           ) \
           AND NOT EXISTS ( \
                SELECT 1 FROM teams_messages tm2 \
                  JOIN teams_chat_members tcm2 \
                    ON tcm2.chat_id = tm2.chat_id AND tcm2.team_member_id = ?1 \
                 WHERE tm2.chat_id = tm.chat_id \
                   AND tm2.sent_at_ms > tm.sent_at_ms \
                   AND ( \
                       (tm2.from_aad_id IS NOT NULL AND tm2.from_aad_id = tcm2.aad_id) \
                       OR ( \
                           tm2.from_email IS NOT NULL \
                           AND tcm2.email IS NOT NULL \
                           AND lower(tm2.from_email) = lower(tcm2.email) \
                       ) \
                   ) \
           ) \
         ORDER BY tm.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_teams_candidate,
    )?;
    collect_rows(rows)
}

fn row_to_teams_candidate(r: &rusqlite::Row<'_>) -> rusqlite::Result<WaitingCandidate> {
    Ok(WaitingCandidate {
        source_kind: "teams".into(),
        source_ref_id: r.get(0)?,
        since_ms: r.get(1)?,
        preview: r.get(2)?,
    })
}

// ---------- Meetings ------------------------------------------------------

/// Past meeting that included the person and has no linked note.
/// Heuristic: "you owe them a note / write-up."
fn meeting_past_without_note(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
    now_ms: i64,
    cap: usize,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        SELECT ce.id, ce.start_ms, \
               'Meeting: ' || COALESCE(NULLIF(ce.title, ''), '(untitled)') AS preview \
          FROM calendar_events ce \
          JOIN calendar_attendees ca \
            ON ca.event_id = ce.id AND ca.team_member_id = ?1 \
         WHERE ce.end_ms < ?2 \
           AND ce.end_ms >= ?3 \
           AND ce.linked_note_id IS NULL \
           AND (ce.status IS NULL OR ce.status != 'cancelled') \
         ORDER BY ce.start_ms DESC \
         LIMIT ?4";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, now_ms, cutoff_ms, cap as i64],
        |r| {
            Ok(WaitingCandidate {
                source_kind: "meeting".into(),
                source_ref_id: r.get(0)?,
                since_ms: r.get(1)?,
                preview: r.get(2)?,
            })
        },
    )?;
    collect_rows(rows)
}

/// Future meeting organized by the person where the user has not
/// accepted or tentatively-accepted yet.
fn meeting_future_unaccepted(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
    cap: usize,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        SELECT ce.id, ce.start_ms, \
               'Meeting: ' || COALESCE(NULLIF(ce.title, ''), '(untitled)') AS preview \
          FROM calendar_events ce \
          JOIN calendar_attendees co \
            ON co.event_id = ce.id \
           AND co.team_member_id = ?1 \
           AND co.is_organizer = 1 \
          LEFT JOIN calendar_attendees cs \
            ON cs.event_id = ce.id AND cs.is_self = 1 \
         WHERE ce.start_ms >= ?2 \
           AND (ce.status IS NULL OR ce.status != 'cancelled') \
           AND ( \
                cs.response_status IS NULL \
                OR cs.response_status NOT IN ('accepted', 'tentative', 'tentativelyAccepted') \
           ) \
         ORDER BY ce.start_ms ASC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![person_id, now_ms, cap as i64], |r| {
        Ok(WaitingCandidate {
            source_kind: "meeting".into(),
            source_ref_id: r.get(0)?,
            since_ms: r.get(1)?,
            preview: r.get(2)?,
        })
    })?;
    collect_rows(rows)
}

fn collect_rows<I>(rows: I) -> rusqlite::Result<Vec<WaitingCandidate>>
where
    I: Iterator<Item = rusqlite::Result<WaitingCandidate>>,
{
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ---------- Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_self(conn: &Connection, id: &str, email: &str) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES (?1, 'Me', '', '', 1, 0, 0)",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES (?1, 'email', ?2)",
            params![id, email],
        )
        .unwrap();
    }

    fn seed_teammate(conn: &Connection, id: &str, email: &str) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES (?1, ?1, '', '', 0, 0, 0)",
            params![id],
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
        thread: &str,
        from: &str,
        sent_at: i64,
        preview: &str,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO email_messages(id, connector_id, external_id, thread_id, subject, \
                                          from_email, sent_at_ms, body_preview, modified_ms) \
             VALUES (?1, 'mg:test', ?1, ?2, 'Sub', ?3, ?4, ?5, ?4)",
            params![id, thread, from, sent_at, preview],
        )
        .unwrap();
    }

    fn seed_recipient(conn: &Connection, message_id: &str, email: &str, member_id: Option<&str>) {
        conn.execute(
            "INSERT INTO email_recipients(message_id, email, recipient_type, team_member_id) \
             VALUES (?1, ?2, 'to', ?3)",
            params![message_id, email, member_id],
        )
        .unwrap();
    }

    fn seed_teams_chat_member(
        conn: &Connection,
        chat: &str,
        aad: &str,
        email: Option<&str>,
        member_id: Option<&str>,
        is_self: bool,
    ) {
        conn.execute(
            "INSERT INTO teams_chat_members(chat_id, aad_id, email, team_member_id, is_self) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chat, aad, email, member_id, is_self as i64],
        )
        .unwrap();
    }

    fn seed_teams_msg(
        conn: &Connection,
        id: &str,
        chat: &str,
        from_email: Option<&str>,
        from_aad: Option<&str>,
        sent_at: i64,
        preview: &str,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO teams_messages(id, connector_id, external_id, chat_id, chat_kind, \
                                          sent_at_ms, from_aad_id, from_email, body_preview, \
                                          modified_ms) \
             VALUES (?1, 'mg:test', ?1, ?2, 'oneOnOne', ?3, ?4, ?5, ?6, ?3)",
            params![id, chat, sent_at, from_aad, from_email, preview],
        )
        .unwrap();
    }

    fn seed_meeting(
        conn: &Connection,
        id: &str,
        start: i64,
        end: i64,
        linked_note: Option<&str>,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO calendar_events(id, connector_id, external_id, title, start_ms, end_ms, \
                                          all_day, modified_ms, linked_note_id) \
             VALUES (?1, 'mg:test', ?1, 'M', ?2, ?3, 0, ?2, ?4)",
            params![id, start, end, linked_note],
        )
        .unwrap();
    }

    fn seed_attendee(
        conn: &Connection,
        event: &str,
        email: &str,
        member_id: Option<&str>,
        is_self: bool,
        is_organizer: bool,
        response: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO calendar_attendees(event_id, email, response_status, is_self, \
                                              is_organizer, team_member_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![event, email, response, is_self as i64, is_organizer as i64, member_id],
        )
        .unwrap();
    }

    // ---------- Email ------------------------------------------------------

    #[test]
    fn inbound_email_picks_unanswered() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 1_000, "ping?");
        seed_recipient(&conn, "e1", "me@x.io", Some("tm_self"));

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "e1");
        assert_eq!(got[0].source_kind, "email");
        assert_eq!(got[0].preview, "ping?");
    }

    #[test]
    fn inbound_email_excludes_after_self_reply() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 2_000, "ping?");
        seed_recipient(&conn, "e1", "me@x.io", Some("tm_self"));
        seed_email(&conn, "e2", "t1", "me@x.io", now - 1_000, "pong");

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert!(got.is_empty(), "self replied → should be cleared");
    }

    #[test]
    fn inbound_email_filters_recency_window() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        // 60 days old → outside the 30-day window.
        let old = now - 60 * 24 * 3_600 * 1_000;
        seed_email(&conn, "e_old", "t1", "alice@x.io", old, "old");
        seed_recipient(&conn, "e_old", "me@x.io", Some("tm_self"));

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn inbound_email_recipient_via_alias_match() {
        // Recipient row has no team_member_id; alias-email match path
        // still surfaces it.
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 1_000, "alias-route");
        seed_recipient(&conn, "e1", "me@x.io", None);

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn outbound_email_picks_unanswered() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "me@x.io", now - 1_000, "any update?");
        seed_recipient(&conn, "e1", "alice@x.io", Some("tm_alice"));

        let got = outbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "e1");
    }

    #[test]
    fn outbound_email_excludes_after_their_reply() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "me@x.io", now - 2_000, "any update?");
        seed_recipient(&conn, "e1", "alice@x.io", Some("tm_alice"));
        seed_email(&conn, "e2", "t1", "alice@x.io", now - 1_000, "yes!");

        let got = outbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert!(got.is_empty());
    }

    // ---------- Teams ------------------------------------------------------

    #[test]
    fn inbound_teams_picks_unanswered_via_aad() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(&conn, "m1", "c1", None, Some("aad-alice"), now - 1_000, "got a sec?");

        let got = inbound_teams(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m1");
        assert_eq!(got[0].source_kind, "teams");
    }

    #[test]
    fn inbound_teams_excludes_after_self_reply() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(&conn, "m1", "c1", None, Some("aad-alice"), now - 2_000, "?");
        seed_teams_msg(&conn, "m2", "c1", Some("me@x.io"), None, now - 1_000, "ok");

        let got = inbound_teams(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn outbound_teams_picks_unanswered() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(&conn, "m1", "c1", Some("me@x.io"), None, now - 1_000, "ping");

        let got = outbound_teams(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m1");
    }

    // ---------- Meetings --------------------------------------------------

    #[test]
    fn meeting_past_without_note_picks_up() {
        let conn = open_db();
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_meeting(&conn, "m_past", now - 3_600_000, now - 1_800_000, None);
        seed_attendee(&conn, "m_past", "alice@x.io", Some("tm_alice"), false, false, None);
        seed_meeting(
            &conn, "m_past_with_note", now - 7_200_000, now - 5_400_000,
            Some("note1"),
        );
        seed_attendee(
            &conn, "m_past_with_note", "alice@x.io", Some("tm_alice"), false, false, None,
        );

        let got = meeting_past_without_note(
            &conn, "tm_alice", now - RECENCY_WINDOW_MS, now, MEETING_SUBCAP,
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m_past");
        assert_eq!(got[0].source_kind, "meeting");
    }

    #[test]
    fn meeting_future_unaccepted_picks_up() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_meeting(&conn, "m_fut", now + 3_600_000, now + 7_200_000, None);
        // Alice organizes, self attendee row response is needsAction.
        seed_attendee(
            &conn, "m_fut", "alice@x.io", Some("tm_alice"), false, true, None,
        );
        seed_attendee(
            &conn, "m_fut", "me@x.io", Some("tm_self"), true, false, Some("needsAction"),
        );

        let got = meeting_future_unaccepted(&conn, "tm_alice", now, MEETING_SUBCAP).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m_fut");
    }

    #[test]
    fn meeting_future_skips_already_accepted() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_meeting(&conn, "m_fut", now + 3_600_000, now + 7_200_000, None);
        seed_attendee(
            &conn, "m_fut", "alice@x.io", Some("tm_alice"), false, true, None,
        );
        seed_attendee(
            &conn, "m_fut", "me@x.io", Some("tm_self"), true, false, Some("accepted"),
        );

        let got = meeting_future_unaccepted(&conn, "tm_alice", now, MEETING_SUBCAP).unwrap();
        assert!(got.is_empty());
    }

    // ---------- Direction wrappers ----------------------------------------

    #[test]
    fn direction_cap_truncates() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        // 25 unanswered inbound emails — only CANDIDATES_PER_DIRECTION_CAP
        // should survive.
        for i in 0..25 {
            let id = format!("e{i}");
            let thread = format!("t{i}");
            seed_email(&conn, &id, &thread, "alice@x.io", now - 1_000 - i as i64, "x");
            seed_recipient(&conn, &id, "me@x.io", Some("tm_self"));
        }

        let got = candidates_from_me(&conn, "tm_alice", now).unwrap();
        assert_eq!(got.len(), CANDIDATES_PER_DIRECTION_CAP);
    }

    #[test]
    fn from_me_combines_email_and_teams_and_meeting() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;

        // 1× inbound email
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 1_000, "x");
        seed_recipient(&conn, "e1", "me@x.io", Some("tm_self"));
        // 1× inbound teams
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(&conn, "m1", "c1", None, Some("aad-alice"), now - 2_000, "?");
        // 1× past meeting without note
        seed_meeting(&conn, "mp", now - 5_000, now - 4_000, None);
        seed_attendee(&conn, "mp", "alice@x.io", Some("tm_alice"), false, false, None);

        let got = candidates_from_me(&conn, "tm_alice", now).unwrap();
        let kinds: Vec<&str> = got.iter().map(|c| c.source_kind.as_str()).collect();
        assert!(kinds.contains(&"email"));
        assert!(kinds.contains(&"teams"));
        assert!(kinds.contains(&"meeting"));
        // Recency-desc ordering: email (1k ago) before teams (2k ago) before meeting (5k ago).
        assert_eq!(got[0].source_kind, "email");
    }
}
