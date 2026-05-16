//! Provider-agnostic email storage layer (#69).
//!
//! Mirrors `connectors/calendar.rs`. The `microsoft_graph` connector
//! (and any future provider — Gmail, IMAP, …) maps its provider-specific
//! JSON onto `EmailMessage` and calls `upsert_messages` to persist into
//! the shared `email_messages` / `email_recipients` tables.
//!
//! Unlike the calendar layer, this layer is **count-based / accumulate-only**:
//! we don't define a sliding time window, so we don't delete orphans.
//! Old messages stay in the DB until an explicit retention policy is
//! introduced. Rationale: the workstream synthesizer (#70) may surface
//! a thread that started months ago, and we don't want to keep
//! re-fetching its lead.
//!
//! Bodies (`body_html`) are NOT populated at sync time — too expensive
//! at 200 messages/sync. They're lazy-fetched via `get_email_body`
//! Tauri command, persisted, and preserved across re-syncs via
//! `COALESCE(excluded.body_html, email_messages.body_html)` on UPSERT.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct EmailMessage {
    pub id: String,
    pub connector_id: String,
    pub external_id: String,
    pub thread_id: String,
    pub subject: String,
    pub from_email: String,
    pub from_name: Option<String>,
    pub sent_at_ms: i64,
    pub body_preview: Option<String>,
    /// Full HTML body. Always `None` from the connector's sync path —
    /// populated lazily via `get_email_body`. `set_message_body_html`
    /// is the only writer.
    pub body_html: Option<String>,
    pub has_attachments: bool,
    pub is_read: bool,
    pub raw_etag: Option<String>,
    pub modified_ms: i64,
    pub recipients: Vec<EmailRecipient>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailRecipient {
    pub email: String,
    pub display_name: Option<String>,
    /// "to" | "cc" | "bcc"
    pub recipient_type: String,
    pub team_member_id: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UpsertReport {
    pub added: u64,
    pub updated: u64,
    pub skipped: u64,
}

/// Insert or update each message + replace its recipient set, in one
/// transaction. Returns counts: `added` = new rows, `updated` = existing
/// rows refreshed, `skipped` = rows we couldn't store (e.g. lacking a
/// `from_email`; the connector layer is responsible for filtering those
/// before calling — this is a defensive count).
///
/// `body_html` is preserved across upserts via `COALESCE` so a re-sync
/// (which doesn't carry body) doesn't clobber a body the user already
/// triggered a lazy fetch for.
pub fn upsert_messages(
    conn: &mut Connection,
    connector_id: &str,
    messages: &[EmailMessage],
) -> rusqlite::Result<UpsertReport> {
    let tx = conn.transaction()?;

    let mut report = UpsertReport::default();
    for msg in messages {
        if msg.from_email.is_empty() {
            report.skipped += 1;
            continue;
        }
        let pre_existed: i64 = tx
            .query_row(
                "SELECT 1 FROM email_messages WHERE id = ?1",
                params![msg.id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        upsert_message(&tx, connector_id, msg)?;
        if pre_existed != 0 {
            report.updated += 1;
        } else {
            // Live event emission (#106) — only on first insert. Sender
            // resolution via team_member_aliases mirrors the #102 backfill.
            let sender_id: Option<String> = tx
                .query_row(
                    "SELECT m.id FROM team_members m \
                     JOIN team_member_aliases a ON a.member_id = m.id \
                     WHERE a.kind = 'email' AND lower(a.value) = lower(?1) LIMIT 1",
                    params![&msg.from_email],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let kind = match &sender_id {
                Some(id) => {
                    let is_self: i64 = tx
                        .query_row(
                            "SELECT is_self FROM team_members WHERE id = ?1",
                            params![id],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if is_self != 0 { "email_sent" } else { "email_received" }
                }
                None => "email_received",
            };
            let payload = serde_json::json!({
                "thread_id": msg.thread_id,
                "subject": msg.subject,
            });
            crate::events::emit(
                &tx,
                msg.sent_at_ms,
                kind,
                sender_id.as_deref(),
                "email",
                &msg.id,
                &payload,
            )?;
            // When self sends, mark every team_member recipient dirty
            // (#121). The waiting-action surface depends on the
            // profile worker re-running for each counterparty so any
            // outstanding "Waiting on you" entries clear on the next
            // tick instead of the 24h TTL. Filter excludes the sender
            // (cc-self) and any external recipient with no resolved
            // team_member_id.
            if kind == "email_sent" {
                let sender = sender_id.as_deref().unwrap_or("");
                let cp_ids: Vec<String> = {
                    let mut cp_stmt = tx.prepare(
                        "SELECT DISTINCT team_member_id FROM email_recipients \
                          WHERE message_id = ?1 \
                            AND team_member_id IS NOT NULL \
                            AND team_member_id != ?2",
                    )?;
                    let rows = cp_stmt
                        .query_map(params![&msg.id, sender], |r| r.get::<_, String>(0))?;
                    rows.filter_map(Result::ok).collect()
                };
                for cp_id in cp_ids {
                    crate::events::emit(
                        &tx,
                        msg.sent_at_ms,
                        "counterparty_replied",
                        Some(&cp_id),
                        "email",
                        &msg.id,
                        &serde_json::json!({ "source": "email" }),
                    )?;
                }
            }
            report.added += 1;
        }
    }

    tx.commit()?;
    Ok(report)
}

fn upsert_message(
    tx: &rusqlite::Transaction<'_>,
    _connector_id: &str,
    m: &EmailMessage,
) -> rusqlite::Result<()> {
    // ON CONFLICT(id): refresh metadata, but NOT body_html — preserve
    // any value populated by the lazy-fetch path. `excluded.body_html`
    // is always NULL on the sync path, so COALESCE keeps the existing
    // value. If a future caller does pass a body, COALESCE prefers the
    // new (non-null) one — same shape as a normal upsert.
    tx.execute(
        "INSERT INTO email_messages(\
            id, connector_id, external_id, thread_id, subject, from_email, from_name, \
            sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT(id) DO UPDATE SET \
            thread_id = excluded.thread_id, \
            subject = excluded.subject, \
            from_email = excluded.from_email, \
            from_name = excluded.from_name, \
            sent_at_ms = excluded.sent_at_ms, \
            body_preview = excluded.body_preview, \
            body_html = COALESCE(excluded.body_html, email_messages.body_html), \
            has_attachments = excluded.has_attachments, \
            is_read = excluded.is_read, \
            raw_etag = excluded.raw_etag, \
            modified_ms = excluded.modified_ms",
        params![
            m.id,
            m.connector_id,
            m.external_id,
            m.thread_id,
            m.subject,
            m.from_email,
            m.from_name,
            m.sent_at_ms,
            m.body_preview,
            m.body_html,
            m.has_attachments as i64,
            m.is_read as i64,
            m.raw_etag,
            m.modified_ms,
        ],
    )?;

    // Recipients: replace wholesale. Same pattern as calendar attendees.
    tx.execute(
        "DELETE FROM email_recipients WHERE message_id = ?1",
        params![m.id],
    )?;
    let mut stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO email_recipients(\
            message_id, email, display_name, recipient_type, team_member_id\
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for r in &m.recipients {
        if r.email.is_empty() {
            continue;
        }
        stmt.execute(params![
            m.id,
            r.email,
            r.display_name,
            r.recipient_type,
            r.team_member_id,
        ])?;
    }
    Ok(())
}

/// Most-recent-first listing of messages whose `sent_at_ms` falls in
/// `[sent_from_ms, sent_to_ms]`. Optional connector filter. Limited
/// to `limit` rows.
pub fn list_messages_in_range(
    conn: &Connection,
    sent_from_ms: i64,
    sent_to_ms: i64,
    connector_id: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<EmailMessage>> {
    let sql = match connector_id {
        Some(_) => {
            "SELECT id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                    sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms \
             FROM email_messages \
             WHERE sent_at_ms BETWEEN ?1 AND ?2 AND connector_id = ?3 \
             ORDER BY sent_at_ms DESC \
             LIMIT ?4"
        }
        None => {
            "SELECT id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                    sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms \
             FROM email_messages \
             WHERE sent_at_ms BETWEEN ?1 AND ?2 \
             ORDER BY sent_at_ms DESC \
             LIMIT ?3"
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let mut messages: Vec<EmailMessage> = match connector_id {
        Some(id) => {
            let rows = stmt.query_map(params![sent_from_ms, sent_to_ms, id, limit as i64], row_to_message)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        None => {
            let rows = stmt.query_map(params![sent_from_ms, sent_to_ms, limit as i64], row_to_message)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
    };
    attach_recipients(conn, &mut messages)?;
    Ok(messages)
}

/// All messages in a thread, oldest-first (so the UI can render the
/// conversation top-down).
pub fn list_messages_by_thread(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<EmailMessage>> {
    let mut stmt = conn.prepare(
        "SELECT id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms \
         FROM email_messages \
         WHERE thread_id = ?1 \
         ORDER BY sent_at_ms ASC",
    )?;
    let rows = stmt.query_map(params![thread_id], row_to_message)?;
    let mut messages: Vec<EmailMessage> = rows.collect::<Result<Vec<_>, _>>()?;
    attach_recipients(conn, &mut messages)?;
    Ok(messages)
}

pub fn get_message_details(
    conn: &Connection,
    message_id: &str,
) -> rusqlite::Result<Option<EmailMessage>> {
    use rusqlite::OptionalExtension;
    let mut stmt = conn.prepare(
        "SELECT id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms \
         FROM email_messages WHERE id = ?1",
    )?;
    let mut msg = stmt
        .query_row(params![message_id], row_to_message)
        .optional()?;
    if let Some(ref mut m) = msg {
        let mut as_slice = vec![std::mem::take(m)];
        attach_recipients(conn, &mut as_slice)?;
        *m = as_slice.pop().unwrap();
    }
    Ok(msg)
}

pub fn get_message_body_html(
    conn: &Connection,
    message_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT body_html FROM email_messages WHERE id = ?1",
        params![message_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => rusqlite::Error::QueryReturnedNoRows,
        other => other,
    })
}

pub fn set_message_body_html(
    conn: &Connection,
    message_id: &str,
    body_html: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE email_messages SET body_html = ?1 WHERE id = ?2",
        params![body_html, message_id],
    )?;
    Ok(())
}

/// Fetch `(connector_id, external_id)` for a message. Used by
/// `get_email_body` to locate the upstream record before issuing the
/// lazy Graph fetch.
pub fn get_message_origin(
    conn: &Connection,
    message_id: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    use rusqlite::OptionalExtension;
    let mut stmt = conn.prepare(
        "SELECT connector_id, external_id FROM email_messages WHERE id = ?1",
    )?;
    stmt.query_row(params![message_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .optional()
}

// ----- Internal helpers ----------------------------------------------------

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<EmailMessage> {
    Ok(EmailMessage {
        id: r.get(0)?,
        connector_id: r.get(1)?,
        external_id: r.get(2)?,
        thread_id: r.get(3)?,
        subject: r.get(4)?,
        from_email: r.get(5)?,
        from_name: r.get(6)?,
        sent_at_ms: r.get(7)?,
        body_preview: r.get(8)?,
        body_html: r.get(9)?,
        has_attachments: r.get::<_, i64>(10)? != 0,
        is_read: r.get::<_, i64>(11)? != 0,
        raw_etag: r.get(12)?,
        modified_ms: r.get(13)?,
        recipients: Vec::new(),
    })
}

impl Default for EmailMessage {
    fn default() -> Self {
        Self {
            id: String::new(),
            connector_id: String::new(),
            external_id: String::new(),
            thread_id: String::new(),
            subject: String::new(),
            from_email: String::new(),
            from_name: None,
            sent_at_ms: 0,
            body_preview: None,
            body_html: None,
            has_attachments: false,
            is_read: false,
            raw_etag: None,
            modified_ms: 0,
            recipients: Vec::new(),
        }
    }
}

fn attach_recipients(
    conn: &Connection,
    messages: &mut [EmailMessage],
) -> rusqlite::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?")
        .take(messages.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT message_id, email, display_name, recipient_type, team_member_id \
         FROM email_recipients WHERE message_id IN ({placeholders}) \
         ORDER BY message_id, recipient_type, email"
    );
    let mut stmt = conn.prepare(&sql)?;
    let id_refs: Vec<&dyn rusqlite::ToSql> = messages
        .iter()
        .map(|m| &m.id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(id_refs), |r| {
        Ok((
            r.get::<_, String>(0)?,
            EmailRecipient {
                email: r.get(1)?,
                display_name: r.get(2)?,
                recipient_type: r.get(3)?,
                team_member_id: r.get(4)?,
            },
        ))
    })?;
    let mut by_id: std::collections::HashMap<String, Vec<EmailRecipient>> =
        std::collections::HashMap::new();
    for row in rows {
        let (mid, rec) = row?;
        by_id.entry(mid).or_default().push(rec);
    }
    for m in messages.iter_mut() {
        if let Some(recs) = by_id.remove(&m.id) {
            m.recipients = recs;
        }
    }
    Ok(())
}

// ----- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta(key, value) VALUES ('schema_version', '10');
             CREATE TABLE team_members (id TEXT PRIMARY KEY, is_self INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE connectors (id TEXT PRIMARY KEY);
             CREATE TABLE team_member_aliases (
                 member_id TEXT NOT NULL,
                 kind      TEXT NOT NULL,
                 value     TEXT NOT NULL,
                 PRIMARY KEY (member_id, kind, value)
             );
             -- Minimal `events` stub for #106 live emission. Real schema
             -- in migration 022; this fixture skips that ladder so we
             -- drop a compatible no-FK stub here.
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
             INSERT INTO connectors(id) VALUES ('mg:test');
             INSERT INTO team_members(id) VALUES ('tm:heike');",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/011_email.sql"))
            .unwrap();
        conn
    }

    #[test]
    fn upsert_messages_emits_event_per_new_row() {
        let mut conn = open_test_db();
        let msg1 = make_msg("m1", "t1", 1_000, vec![]);
        let msg2 = make_msg("m2", "t1", 2_000, vec![]);
        let r = upsert_messages(&mut conn, "mg:test", &[msg1.clone(), msg2.clone()]).unwrap();
        assert_eq!(r.added, 2);

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ref_kind = 'email'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);

        // Re-upsert: no new events.
        let r2 = upsert_messages(&mut conn, "mg:test", &[msg1, msg2]).unwrap();
        assert_eq!(r2.added, 0);
        assert_eq!(r2.updated, 2);
        let n2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ref_kind = 'email'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n2, 2, "re-upsert must not duplicate event rows");
    }

    fn make_msg(
        external_id: &str,
        thread_id: &str,
        sent_at_ms: i64,
        recipients: Vec<EmailRecipient>,
    ) -> EmailMessage {
        EmailMessage {
            id: format!("mg:test::{external_id}"),
            connector_id: "mg:test".to_string(),
            external_id: external_id.to_string(),
            thread_id: thread_id.to_string(),
            subject: format!("Subject {external_id}"),
            from_email: "alice@example.com".to_string(),
            from_name: Some("Alice".to_string()),
            sent_at_ms,
            body_preview: Some("preview".to_string()),
            body_html: None,
            has_attachments: false,
            is_read: false,
            raw_etag: None,
            modified_ms: sent_at_ms,
            recipients,
        }
    }

    fn rcpt(email: &str, kind: &str, team_member_id: Option<&str>) -> EmailRecipient {
        EmailRecipient {
            email: email.to_string(),
            display_name: None,
            recipient_type: kind.to_string(),
            team_member_id: team_member_id.map(|s| s.to_string()),
        }
    }

    #[test]
    fn upsert_messages_inserts_new_and_updates_existing() {
        let mut conn = open_test_db();

        let first = vec![
            make_msg("a", "thread-1", 1_000, vec![rcpt("tj@e.com", "to", None)]),
            make_msg("b", "thread-1", 2_000, vec![]),
        ];
        let r1 = upsert_messages(&mut conn, "mg:test", &first).unwrap();
        assert_eq!(r1.added, 2);
        assert_eq!(r1.updated, 0);

        // Re-sync with updated subject on `a` and a new `c`.
        let mut a_updated = make_msg("a", "thread-1", 1_000, vec![rcpt("tj@e.com", "to", None)]);
        a_updated.subject = "Subject a (renamed)".to_string();
        let second = vec![
            a_updated,
            make_msg("c", "thread-2", 3_000, vec![]),
        ];
        let r2 = upsert_messages(&mut conn, "mg:test", &second).unwrap();
        assert_eq!(r2.added, 1, "c is new");
        assert_eq!(r2.updated, 1, "a existed");

        // Verify final row state — `b` survived (no orphan deletion).
        let all = list_messages_in_range(&conn, 0, 100_000, Some("mg:test"), 100).unwrap();
        assert_eq!(all.len(), 3);

        let titles: Vec<&str> = all.iter().map(|m| m.subject.as_str()).collect();
        // Most-recent-first: c (3000), b (2000), a (1000).
        assert_eq!(titles, vec!["Subject c", "Subject b", "Subject a (renamed)"]);
    }

    #[test]
    fn upsert_messages_preserves_body_html_across_resync() {
        let mut conn = open_test_db();
        let first = vec![make_msg("a", "thread-1", 1_000, vec![])];
        upsert_messages(&mut conn, "mg:test", &first).unwrap();

        // Lazy fetch populates body_html.
        set_message_body_html(&conn, "mg:test::a", "<p>hello</p>").unwrap();
        let body = get_message_body_html(&conn, "mg:test::a").unwrap();
        assert_eq!(body.as_deref(), Some("<p>hello</p>"));

        // Re-sync with an updated subject but body_html: None (the
        // connector path never carries body).
        let mut a_again = make_msg("a", "thread-1", 1_000, vec![]);
        a_again.subject = "New subject".to_string();
        a_again.body_html = None;
        upsert_messages(&mut conn, "mg:test", &[a_again]).unwrap();

        // Body must survive.
        let body = get_message_body_html(&conn, "mg:test::a").unwrap();
        assert_eq!(
            body.as_deref(),
            Some("<p>hello</p>"),
            "body_html must be preserved across a re-sync that doesn't carry it"
        );
        // But subject was updated.
        let m = get_message_details(&conn, "mg:test::a").unwrap().unwrap();
        assert_eq!(m.subject, "New subject");
    }

    #[test]
    fn list_messages_by_thread_orders_by_sent_at_asc() {
        let mut conn = open_test_db();
        let msgs = vec![
            make_msg("c", "thread-1", 3_000, vec![]),
            make_msg("a", "thread-1", 1_000, vec![]),
            make_msg("b", "thread-1", 2_000, vec![]),
            make_msg("z", "thread-2", 5_000, vec![]),
        ];
        upsert_messages(&mut conn, "mg:test", &msgs).unwrap();

        let thread1 = list_messages_by_thread(&conn, "thread-1").unwrap();
        let ids: Vec<&str> = thread1.iter().map(|m| m.external_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn get_message_details_returns_none_for_missing() {
        let conn = open_test_db();
        let result = get_message_details(&conn, "nope").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn recipients_are_replaced_wholesale_on_resync() {
        let mut conn = open_test_db();
        let first = vec![make_msg(
            "a",
            "thread-1",
            1_000,
            vec![
                rcpt("tj@e.com", "to", None),
                rcpt("bob@e.com", "cc", None),
            ],
        )];
        upsert_messages(&mut conn, "mg:test", &first).unwrap();

        // Re-sync with different recipient set.
        let second = vec![make_msg(
            "a",
            "thread-1",
            1_000,
            vec![rcpt("alice@e.com", "to", Some("tm:heike"))],
        )];
        upsert_messages(&mut conn, "mg:test", &second).unwrap();

        let m = get_message_details(&conn, "mg:test::a").unwrap().unwrap();
        assert_eq!(m.recipients.len(), 1);
        assert_eq!(m.recipients[0].email, "alice@e.com");
        assert_eq!(m.recipients[0].team_member_id.as_deref(), Some("tm:heike"));
    }

    #[test]
    fn upsert_skips_messages_with_empty_from_email() {
        let mut conn = open_test_db();
        let mut bad = make_msg("bad", "thread-x", 1_000, vec![]);
        bad.from_email = String::new();
        let report = upsert_messages(&mut conn, "mg:test", &[bad]).unwrap();
        assert_eq!(report.added, 0);
        assert_eq!(report.skipped, 1);

        let all = list_messages_in_range(&conn, 0, 100_000, Some("mg:test"), 100).unwrap();
        assert_eq!(all.len(), 0);
    }

    #[test]
    fn list_messages_in_range_filters_by_connector() {
        let mut conn = open_test_db();
        conn.execute("INSERT INTO connectors(id) VALUES ('gm:test')", [])
            .unwrap();

        upsert_messages(&mut conn, "mg:test", &[make_msg("a", "t-1", 1_000, vec![])]).unwrap();
        let mut other = make_msg("b", "t-2", 2_000, vec![]);
        other.id = "gm:test::b".into();
        other.connector_id = "gm:test".into();
        upsert_messages(&mut conn, "gm:test", &[other]).unwrap();

        let mg = list_messages_in_range(&conn, 0, 100_000, Some("mg:test"), 10).unwrap();
        assert_eq!(mg.len(), 1);
        assert_eq!(mg[0].connector_id, "mg:test");
        let all = list_messages_in_range(&conn, 0, 100_000, None, 10).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn get_message_origin_returns_connector_and_external_id() {
        let mut conn = open_test_db();
        upsert_messages(&mut conn, "mg:test", &[make_msg("ext-1", "t", 1_000, vec![])]).unwrap();

        let origin = get_message_origin(&conn, "mg:test::ext-1").unwrap().unwrap();
        assert_eq!(origin, ("mg:test".into(), "ext-1".into()));

        let none = get_message_origin(&conn, "mg:test::missing").unwrap();
        assert!(none.is_none());
    }

    /// Seed `team_members(id, is_self)` + a single email alias used by
    /// the sender-resolution JOIN inside `upsert_messages`.
    fn seed_member_with_email(
        conn: &Connection,
        id: &str,
        email: &str,
        is_self: bool,
    ) {
        // Test fixture's team_members table only has (id, is_self).
        conn.execute(
            "INSERT OR IGNORE INTO team_members(id, is_self) VALUES (?1, ?2)",
            params![id, is_self as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) \
             VALUES (?1, 'email', ?2)",
            params![id, email.to_lowercase()],
        )
        .unwrap();
    }

    fn cp_email_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM events \
              WHERE kind = 'counterparty_replied' AND ref_kind = 'email'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn make_msg_from(
        external_id: &str,
        thread_id: &str,
        sent_at_ms: i64,
        from_email: &str,
        recipients: Vec<EmailRecipient>,
    ) -> EmailMessage {
        let mut m = make_msg(external_id, thread_id, sent_at_ms, recipients);
        m.from_email = from_email.to_string();
        m
    }

    /// Outbound mail (self → Alice + Bob) emits one
    /// `counterparty_replied` per resolved team_member recipient (#121).
    #[test]
    fn outbound_emits_counterparty_replied_per_recipient() {
        let mut conn = open_test_db();
        seed_member_with_email(&conn, "tm:self", "me@x.io", true);
        // tm:heike pre-seeded by the fixture; add an alias for it.
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) \
             VALUES ('tm:heike', 'email', 'heike@x.io')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_members(id, is_self) VALUES ('tm:bob', 0)",
            [],
        )
        .unwrap();

        let msg = make_msg_from(
            "out-1",
            "t-1",
            1_000,
            "me@x.io",
            vec![
                rcpt("heike@x.io", "to", Some("tm:heike")),
                rcpt("bob@x.io", "cc", Some("tm:bob")),
            ],
        );
        upsert_messages(&mut conn, "mg:test", &[msg]).unwrap();

        let mut actors: Vec<String> = conn
            .prepare(
                "SELECT actor_id FROM events \
                  WHERE kind = 'counterparty_replied' AND ref_kind = 'email'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        actors.sort();
        assert_eq!(actors, vec!["tm:bob".to_string(), "tm:heike".to_string()]);
    }

    /// Inbound mail (Heike → self) must not emit counterparty_replied.
    #[test]
    fn inbound_does_not_emit_counterparty_replied() {
        let mut conn = open_test_db();
        seed_member_with_email(&conn, "tm:self", "me@x.io", true);
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) \
             VALUES ('tm:heike', 'email', 'heike@x.io')",
            [],
        )
        .unwrap();

        let msg = make_msg_from(
            "in-1",
            "t-1",
            1_000,
            "heike@x.io",
            vec![rcpt("me@x.io", "to", Some("tm:self"))],
        );
        upsert_messages(&mut conn, "mg:test", &[msg]).unwrap();
        assert_eq!(cp_email_count(&conn), 0);
    }

    /// External recipients (no resolved `team_member_id`) must not
    /// produce counterparty_replied rows.
    #[test]
    fn outbound_skips_external_recipients() {
        let mut conn = open_test_db();
        seed_member_with_email(&conn, "tm:self", "me@x.io", true);

        let msg = make_msg_from(
            "out-ext",
            "t-1",
            1_000,
            "me@x.io",
            vec![rcpt("vendor@external.com", "to", None)],
        );
        upsert_messages(&mut conn, "mg:test", &[msg]).unwrap();
        assert_eq!(cp_email_count(&conn), 0);
    }

    /// Self CC'd to own outbound mail must not produce a
    /// counterparty_replied row pointing at self.
    #[test]
    fn outbound_skips_self_cc() {
        let mut conn = open_test_db();
        seed_member_with_email(&conn, "tm:self", "me@x.io", true);
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) \
             VALUES ('tm:heike', 'email', 'heike@x.io')",
            [],
        )
        .unwrap();

        let msg = make_msg_from(
            "out-selfcc",
            "t-1",
            1_000,
            "me@x.io",
            vec![
                rcpt("heike@x.io", "to", Some("tm:heike")),
                rcpt("me@x.io", "cc", Some("tm:self")),
            ],
        );
        upsert_messages(&mut conn, "mg:test", &[msg]).unwrap();

        let actors: Vec<String> = conn
            .prepare(
                "SELECT actor_id FROM events \
                  WHERE kind = 'counterparty_replied' AND ref_kind = 'email'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(actors, vec!["tm:heike".to_string()]);
    }
}
