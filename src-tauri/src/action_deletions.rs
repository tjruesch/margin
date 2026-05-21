//! Universal action deletion log (#147).
//!
//! Every path that removes an action row — user delete, user dismiss,
//! worker auto-resolve at threshold — calls [`log_deletion`] inside the
//! same transaction. Readers in #148/#149/#150 query this log to feed
//! the user's rejections back into the LLM prompts that produce action
//! items.
//!
//! The log row is a verbatim snapshot of the action's identity at the
//! moment of deletion (text, origin metadata, subject, assignee) plus a
//! `cause` discriminator. Snapshot fields are deliberately not FKs:
//! the action row is gone by the time a reader cares, and dangling
//! team-member ids don't break the learning signal.

use rusqlite::{params, OptionalExtension, Transaction};

/// Cause discriminator written into `action_deletions.cause`. Readers
/// gate on these values — most consumers ignore [`Cause::AutoResolved`]
/// because worker omissions are weak signal, not user intent.
#[derive(Debug, Clone, Copy)]
pub enum Cause {
    /// User explicitly removed the row from any surface (Home, sidebar,
    /// workstream detail, etc.).
    UserDelete,
    /// User pressed "Ignore" on a worker-extracted waiting action.
    UserDismiss,
    /// Profile worker's hysteresis threshold flipped `done = 1` after
    /// repeated LLM omissions of the source ref.
    AutoResolved,
}

impl Cause {
    fn as_str(self) -> &'static str {
        match self {
            Cause::UserDelete => "user_delete",
            Cause::UserDismiss => "user_dismiss",
            Cause::AutoResolved => "auto_resolved",
        }
    }
}

/// Snapshot the action row identified by `action_id` and insert a row
/// into `action_deletions`. No-op (returns Ok) when the action row is
/// already gone — keeps the call safe to invoke optimistically before
/// path-specific delete logic that may race.
///
/// `deleted_ms` is the current wall clock. Callers pass it in so the
/// log timestamp matches whatever timestamp the surrounding write uses
/// (e.g. the `auto_resolved_ms` stamp written by the worker on the
/// same tick).
pub fn log_deletion(
    tx: &Transaction<'_>,
    action_id: &str,
    cause: Cause,
    deleted_ms: i64,
) -> rusqlite::Result<()> {
    let row: Option<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    )> = tx
        .query_row(
            "SELECT origin_kind, origin_synth_kind, origin_synth_id, \
                    origin_note_id, subject_member_id, assignee_id, text \
               FROM actions WHERE id = ?1",
            params![action_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        origin_kind,
        origin_synth_kind,
        origin_synth_id,
        origin_note_id,
        subject_member_id,
        assignee_id,
        text,
    )) = row
    else {
        return Ok(());
    };

    // series_master_id only makes sense for reconcile-origin rows —
    // they're the ones tied to a meeting note bundle whose calendar
    // event may be part of a recurring series. For other origins,
    // leave NULL so #148 doesn't grab unrelated rows.
    let source_series_master_id: Option<String> = if origin_kind == "reconcile" {
        origin_note_id
            .as_deref()
            .and_then(|note_id| {
                tx.query_row(
                    "SELECT series_master_id FROM calendar_events \
                      WHERE linked_note_id = ?1 \
                        AND series_master_id IS NOT NULL \
                      LIMIT 1",
                    params![note_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .ok()
                .flatten()
                .flatten()
            })
    } else {
        None
    };

    tx.execute(
        "INSERT INTO action_deletions \
            (deleted_ms, origin_kind, origin_synth_kind, origin_synth_id, \
             origin_note_id, subject_member_id, assignee_id, text, \
             source_series_master_id, cause) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            deleted_ms,
            origin_kind,
            origin_synth_kind,
            origin_synth_id,
            origin_note_id,
            subject_member_id,
            assignee_id,
            text,
            source_series_master_id,
            cause.as_str(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Minimal schema replica for testing the helper in isolation.
    /// Mirrors the columns the helper reads and writes — not a full
    /// production schema. The real migration ladder is exercised by
    /// the persist + notes test fixtures.
    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE actions (
                 id                TEXT PRIMARY KEY,
                 origin_kind       TEXT NOT NULL,
                 origin_synth_kind TEXT,
                 origin_synth_id   TEXT,
                 origin_note_id    TEXT,
                 subject_member_id TEXT,
                 assignee_id       TEXT,
                 text              TEXT NOT NULL
             );
             CREATE TABLE calendar_events (
                 id                TEXT PRIMARY KEY,
                 linked_note_id    TEXT,
                 series_master_id  TEXT
             );
             CREATE TABLE action_deletions (
                 id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                 deleted_ms              INTEGER NOT NULL,
                 origin_kind             TEXT NOT NULL,
                 origin_synth_kind       TEXT,
                 origin_synth_id         TEXT,
                 origin_note_id          TEXT,
                 subject_member_id       TEXT,
                 assignee_id             TEXT,
                 text                    TEXT NOT NULL,
                 source_series_master_id TEXT,
                 cause                   TEXT NOT NULL DEFAULT 'user_delete'
             );",
        )
        .unwrap();
        conn
    }

    fn seed_action(
        conn: &Connection,
        id: &str,
        origin_kind: &str,
        origin_synth_kind: Option<&str>,
        origin_synth_id: Option<&str>,
        origin_note_id: Option<&str>,
        subject_member_id: Option<&str>,
        assignee_id: Option<&str>,
        text: &str,
    ) {
        conn.execute(
            "INSERT INTO actions (id, origin_kind, origin_synth_kind, \
                origin_synth_id, origin_note_id, subject_member_id, \
                assignee_id, text) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                origin_kind,
                origin_synth_kind,
                origin_synth_id,
                origin_note_id,
                subject_member_id,
                assignee_id,
                text,
            ],
        )
        .unwrap();
    }

    #[test]
    fn log_records_user_delete_with_full_snapshot() {
        let mut conn = open_test_db();
        seed_action(
            &conn,
            "a1",
            "synth",
            Some("email_waiting"),
            Some("e:42"),
            None,
            Some("tm:heike"),
            Some("tm:self"),
            "Reply to Heike re Q3 budget",
        );
        let tx = conn.transaction().unwrap();
        log_deletion(&tx, "a1", Cause::UserDelete, 1_234).unwrap();
        tx.commit().unwrap();

        let row: (
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT deleted_ms, origin_kind, origin_synth_kind, \
                        origin_synth_id, origin_note_id, subject_member_id, \
                        assignee_id, text, source_series_master_id, cause \
                   FROM action_deletions",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, 1_234);
        assert_eq!(row.1, "synth");
        assert_eq!(row.2.as_deref(), Some("email_waiting"));
        assert_eq!(row.3.as_deref(), Some("e:42"));
        assert!(row.4.is_none());
        assert_eq!(row.5.as_deref(), Some("tm:heike"));
        assert_eq!(row.6.as_deref(), Some("tm:self"));
        assert_eq!(row.7, "Reply to Heike re Q3 budget");
        assert!(
            row.8.is_none(),
            "synth-origin row has no series_master_id"
        );
        assert_eq!(row.9, "user_delete");
    }

    #[test]
    fn log_records_user_dismiss_with_correct_cause() {
        let mut conn = open_test_db();
        seed_action(
            &conn,
            "a2",
            "synth",
            Some("teams_waiting"),
            Some("t:7"),
            None,
            Some("tm:davis"),
            Some("tm:self"),
            "Follow up with Davis",
        );
        let tx = conn.transaction().unwrap();
        log_deletion(&tx, "a2", Cause::UserDismiss, 2_000).unwrap();
        tx.commit().unwrap();

        let cause: String = conn
            .query_row(
                "SELECT cause FROM action_deletions WHERE text = ?1",
                params!["Follow up with Davis"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cause, "user_dismiss");
    }

    #[test]
    fn log_populates_series_master_id_for_recurring_meeting_actions() {
        let mut conn = open_test_db();
        seed_action(
            &conn,
            "a3",
            "reconcile",
            None,
            None,
            Some("note:bundle-7"),
            None,
            Some("tm:self"),
            "Confirm next week's agenda",
        );
        conn.execute(
            "INSERT INTO calendar_events (id, linked_note_id, series_master_id) \
             VALUES ('ev:1', 'note:bundle-7', 'series:weekly-standup')",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        log_deletion(&tx, "a3", Cause::UserDelete, 3_000).unwrap();
        tx.commit().unwrap();

        let series: Option<String> = conn
            .query_row(
                "SELECT source_series_master_id FROM action_deletions WHERE text = ?1",
                params!["Confirm next week's agenda"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(series.as_deref(), Some("series:weekly-standup"));
    }

    #[test]
    fn log_leaves_series_null_when_event_is_oneoff() {
        let mut conn = open_test_db();
        seed_action(
            &conn,
            "a4",
            "reconcile",
            None,
            None,
            Some("note:bundle-9"),
            None,
            None,
            "Send recap",
        );
        // Event linked to the bundle but with no series_master_id.
        conn.execute(
            "INSERT INTO calendar_events (id, linked_note_id, series_master_id) \
             VALUES ('ev:2', 'note:bundle-9', NULL)",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        log_deletion(&tx, "a4", Cause::UserDelete, 4_000).unwrap();
        tx.commit().unwrap();

        let series: Option<String> = conn
            .query_row(
                "SELECT source_series_master_id FROM action_deletions",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(series.is_none(), "one-off meeting must not get a series id");
    }

    #[test]
    fn log_handles_deletes_after_action_row_is_already_gone_gracefully() {
        let mut conn = open_test_db();
        // No seed — action 'phantom' does not exist.
        let tx = conn.transaction().unwrap();
        log_deletion(&tx, "phantom", Cause::UserDelete, 5_000).unwrap();
        tx.commit().unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM action_deletions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "missing rows must not be logged");
    }
}
