//! Workstream signal hydration layer (#85).
//!
//! Workstreams cite items from many domains: emails, calendar events,
//! notes, and (future) GitHub PRs, Slack threads, Linear issues, etc.
//! The DB-side pivot is uniform — `workstream_signals(workstream_id,
//! kind, item_id)` — but each domain has its own rich row type. This
//! module abstracts the per-domain hydration behind a `Signal` trait
//! plus a `SignalRegistry` so `get_workstream_detail` can dispatch
//! polymorphically without growing per-source branches.
//!
//! Adding a new source after this module lands is one file: define a
//! `Signal` impl, register it in `default_with_builtins`, done. No
//! changes to `persist.rs`, `synthesizer.rs`, or any UI code unless
//! the new kind also gets its own slot on `WorkstreamDetail` (it
//! doesn't have to — the registry could feed a generic `signals`
//! field eventually; we kept named fields for v1 minimal churn).

use std::collections::HashMap;
use std::sync::OnceLock;

use rusqlite::{params, Connection, OptionalExtension};

use super::NoteRef;
use crate::connectors::calendar::{self, CalendarEvent};
use crate::connectors::email::{self, EmailMessage};

/// One hydrated item, polymorphic over the registered kinds. The
/// closed enum is intentional for v1 — every consumer (persist,
/// AI ask, UI) needs to know what to do with each variant. New kinds
/// add a variant here AND a `Signal` impl. When the variant count
/// gets unwieldy, a follow-up issue can switch to a polymorphic
/// `WorkstreamItem` struct + per-source views.
#[derive(Debug)]
pub enum HydratedSignal {
    Email(EmailMessage),
    Event(CalendarEvent),
    Note(NoteRef),
}

/// Per-domain hydrator. `kind` is the discriminator stored in the
/// `workstream_signals.kind` column.
pub trait Signal: Send + Sync {
    /// The discriminator used in `workstream_signals.kind`. The
    /// registry already keys impls by the same string, so callers
    /// rarely need this — but it lets a `&dyn Signal` self-identify
    /// in logs and future generic dispatch.
    #[allow(dead_code)]
    fn kind(&self) -> &'static str;
    /// Fetch rich rows for the given item ids. Implementations should
    /// return items in recency-desc order (most recent first). Missing
    /// items are dropped silently — the pivot uses soft FKs and an
    /// item may have been deleted upstream between sync passes.
    fn hydrate(
        &self,
        conn: &Connection,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>>;
}

// ----- Built-in hydrators -------------------------------------------------

pub struct EmailSignal;

impl Signal for EmailSignal {
    fn kind(&self) -> &'static str {
        "email"
    }
    fn hydrate(
        &self,
        conn: &Connection,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>> {
        let mut messages: Vec<EmailMessage> = Vec::with_capacity(item_ids.len());
        for id in item_ids {
            if let Some(m) = email::get_message_details(conn, id)? {
                messages.push(m);
            }
        }
        // Most recent first — the existing detail view sorts the same way.
        messages.sort_by(|a, b| b.sent_at_ms.cmp(&a.sent_at_ms));
        Ok(messages.into_iter().map(HydratedSignal::Email).collect())
    }
}

pub struct EventSignal;

impl Signal for EventSignal {
    fn kind(&self) -> &'static str {
        "event"
    }
    fn hydrate(
        &self,
        conn: &Connection,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>> {
        let mut events: Vec<CalendarEvent> = Vec::with_capacity(item_ids.len());
        for id in item_ids {
            if let Some(e) = calendar::get_event_details(conn, id)? {
                events.push(e);
            }
        }
        events.sort_by(|a, b| b.start_ms.cmp(&a.start_ms));
        Ok(events.into_iter().map(HydratedSignal::Event).collect())
    }
}

pub struct NoteSignal;

impl Signal for NoteSignal {
    fn kind(&self) -> &'static str {
        "note"
    }
    /// Notes are looked up by `note_path` (the soft-FK item_id). Bulk
    /// SELECT keeps it to one query regardless of the input size.
    fn hydrate(
        &self,
        conn: &Connection,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>> {
        if item_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat("?")
            .take(item_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT n.note_path, COALESCE(n.title, ''), COALESCE(n.modified_ms, 0) \
             FROM notes n \
             WHERE n.note_path IN ({placeholders}) \
             ORDER BY n.modified_ms DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let id_refs: Vec<&dyn rusqlite::ToSql> = item_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(id_refs), |r| {
            Ok(NoteRef {
                note_path: r.get(0)?,
                title: r.get(1)?,
                modified_ms: r.get(2)?,
            })
        })?;
        let notes: Vec<NoteRef> = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(notes.into_iter().map(HydratedSignal::Note).collect())
    }
}

// ----- Registry ------------------------------------------------------------

pub struct SignalRegistry {
    by_kind: HashMap<&'static str, Box<dyn Signal>>,
}

impl SignalRegistry {
    pub fn default_with_builtins() -> Self {
        let mut by_kind: HashMap<&'static str, Box<dyn Signal>> = HashMap::new();
        by_kind.insert("email", Box::new(EmailSignal));
        by_kind.insert("event", Box::new(EventSignal));
        by_kind.insert("note", Box::new(NoteSignal));
        Self { by_kind }
    }

    pub fn hydrate(
        &self,
        conn: &Connection,
        kind: &str,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>> {
        match self.by_kind.get(kind) {
            Some(sig) => sig.hydrate(conn, item_ids),
            None => {
                eprintln!(
                    "[workstreams] no Signal impl for kind={kind}; {n} ids dropped",
                    n = item_ids.len()
                );
                Ok(Vec::new())
            }
        }
    }
}

/// Process-wide registry. Initialized lazily so tests that build a
/// fresh `SignalRegistry` directly don't pay the global-init cost.
pub fn registry() -> &'static SignalRegistry {
    static R: OnceLock<SignalRegistry> = OnceLock::new();
    R.get_or_init(SignalRegistry::default_with_builtins)
}

// ----- Convenience: load + dispatch -------------------------------------

/// Read all (kind, item_id) pairs for a workstream and hydrate them
/// through the registry, grouped by kind. Returns a HashMap keyed by
/// kind so callers can route into per-kind named fields without
/// having to scan the result.
pub fn load_and_hydrate_for_workstream(
    conn: &Connection,
    workstream_id: &str,
) -> rusqlite::Result<HashMap<String, Vec<HydratedSignal>>> {
    let mut stmt = conn.prepare(
        "SELECT kind, item_id FROM workstream_signals \
         WHERE workstream_id = ?1 \
         ORDER BY kind, added_ms DESC",
    )?;
    let mut by_kind: HashMap<String, Vec<String>> = HashMap::new();
    let rows = stmt.query_map(params![workstream_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (kind, item_id) = row?;
        by_kind.entry(kind).or_default().push(item_id);
    }

    let reg = registry();
    let mut out: HashMap<String, Vec<HydratedSignal>> = HashMap::new();
    for (kind, ids) in by_kind {
        let hydrated = reg.hydrate(conn, &kind, &ids)?;
        if !hydrated.is_empty() {
            out.insert(kind, hydrated);
        }
    }
    Ok(out)
}

// `OptionalExtension` is imported above to keep the module self-contained
// for any future hydrators that want it; the existing impls use it
// transitively via `email::get_message_details`.
#[allow(unused_imports)]
use OptionalExtension as _OptionalExtensionUsed;

// ----- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta(key, value) VALUES ('schema_version', '11');
             CREATE TABLE team_members (id TEXT PRIMARY KEY);
             CREATE TABLE connectors (id TEXT PRIMARY KEY);
             INSERT INTO connectors(id) VALUES ('mg:test');
             CREATE TABLE notes (
                 note_path  TEXT PRIMARY KEY,
                 title      TEXT NOT NULL,
                 modified_ms INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/009_calendar.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/010_event_note_link.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/011_email.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/012_workstreams.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/013_workstream_user_notes.sql"))
            .unwrap();
        conn.execute_batch(include_str!(
            "../migrations/014_workstream_archive_resurface.sql"
        ))
        .unwrap();
        conn.execute_batch(include_str!("../migrations/015_workstream_owner.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/016_workstream_signals.sql"))
            .unwrap();
        conn
    }

    fn seed_email(conn: &Connection, id: &str, sent_at: i64) {
        conn.execute(
            "INSERT INTO email_messages(\
                id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 't', 'Sub', 'a@e', NULL, ?2, NULL, NULL, 0, 0, NULL, ?2)",
            params![id, sent_at],
        )
        .unwrap();
    }

    fn seed_event(conn: &Connection, id: &str, start: i64) {
        conn.execute(
            "INSERT INTO calendar_events(\
                id, connector_id, external_id, title, start_ms, end_ms, all_day, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 'Ev', ?2, ?2, 0, ?2)",
            params![id, start],
        )
        .unwrap();
    }

    fn seed_note(conn: &Connection, path: &str, modified: i64) {
        conn.execute(
            "INSERT INTO notes(note_path, title, modified_ms) VALUES (?1, ?2, ?3)",
            params![path, "Note", modified],
        )
        .unwrap();
    }

    #[test]
    fn email_signal_returns_recency_desc() {
        let conn = open_test_db();
        seed_email(&conn, "mg:test::a", 1_000);
        seed_email(&conn, "mg:test::b", 5_000);
        seed_email(&conn, "mg:test::c", 3_000);

        let ids = vec![
            "mg:test::a".to_string(),
            "mg:test::b".to_string(),
            "mg:test::c".to_string(),
        ];
        let out = EmailSignal.hydrate(&conn, &ids).unwrap();
        let order: Vec<&str> = out
            .iter()
            .map(|h| match h {
                HydratedSignal::Email(m) => m.id.as_str(),
                _ => panic!("expected Email"),
            })
            .collect();
        assert_eq!(order, vec!["mg:test::b", "mg:test::c", "mg:test::a"]);
    }

    #[test]
    fn email_signal_skips_missing_silently() {
        let conn = open_test_db();
        seed_email(&conn, "mg:test::a", 1_000);
        let ids = vec![
            "mg:test::a".to_string(),
            "mg:test::missing".to_string(),
        ];
        let out = EmailSignal.hydrate(&conn, &ids).unwrap();
        assert_eq!(out.len(), 1, "missing item is dropped, no error");
    }

    #[test]
    fn event_signal_returns_recency_desc() {
        let conn = open_test_db();
        seed_event(&conn, "mg:test::e1", 1_000);
        seed_event(&conn, "mg:test::e2", 5_000);
        let ids = vec!["mg:test::e1".to_string(), "mg:test::e2".to_string()];
        let out = EventSignal.hydrate(&conn, &ids).unwrap();
        match (&out[0], &out[1]) {
            (HydratedSignal::Event(a), HydratedSignal::Event(b)) => {
                assert!(a.start_ms > b.start_ms);
            }
            _ => panic!("expected two Event variants"),
        }
    }

    #[test]
    fn note_signal_returns_recency_desc_and_skips_missing() {
        let conn = open_test_db();
        seed_note(&conn, "/n/a.md", 1_000);
        seed_note(&conn, "/n/b.md", 5_000);
        let ids = vec![
            "/n/a.md".to_string(),
            "/n/b.md".to_string(),
            "/n/missing.md".to_string(),
        ];
        let out = NoteSignal.hydrate(&conn, &ids).unwrap();
        let order: Vec<&str> = out
            .iter()
            .map(|h| match h {
                HydratedSignal::Note(n) => n.note_path.as_str(),
                _ => panic!("expected Note"),
            })
            .collect();
        assert_eq!(order, vec!["/n/b.md", "/n/a.md"], "missing dropped, recency desc");
    }

    #[test]
    fn registry_returns_empty_on_unknown_kind() {
        let conn = open_test_db();
        let reg = SignalRegistry::default_with_builtins();
        let out = reg
            .hydrate(&conn, "github_pr", &vec!["abc".to_string()])
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_and_hydrate_for_workstream_groups_by_kind() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_event(&conn, "mg:test::e1", 2_000);
        seed_note(&conn, "/n/a.md", 3_000);
        // Need a workstream row for the FK on workstream_signals.
        conn.execute(
            "INSERT INTO workstreams(id, title, summary, status, last_activity_ms, created_ms, updated_ms) \
             VALUES ('ws_x', 'WS', '', 'active', 0, 0, 0)",
            [],
        )
        .unwrap();
        // Manually attach signals — we don't go through write_workstream
        // here because that path lives in persist.rs.
        let tx = conn.transaction().unwrap();
        for (kind, id) in [
            ("email", "mg:test::m1"),
            ("event", "mg:test::e1"),
            ("note", "/n/a.md"),
        ] {
            tx.execute(
                "INSERT INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
                 VALUES ('ws_x', ?1, ?2, 100)",
                params![kind, id],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let by_kind = load_and_hydrate_for_workstream(&conn, "ws_x").unwrap();
        assert_eq!(by_kind.len(), 3);
        assert!(matches!(by_kind.get("email").unwrap()[0], HydratedSignal::Email(_)));
        assert!(matches!(by_kind.get("event").unwrap()[0], HydratedSignal::Event(_)));
        assert!(matches!(by_kind.get("note").unwrap()[0], HydratedSignal::Note(_)));
    }
}
