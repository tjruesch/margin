//! Storage layer for workstreams + their pivots and actions.
//!
//! Mirrors the per-domain pattern from `connectors/calendar.rs` /
//! `connectors/email.rs`: small, transparent functions that take a
//! `Connection` (or a `Transaction` on the write side), no hidden
//! state, no caching. The synthesizer composes these into the
//! end-to-end cluster pass.

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::{NoteRef, WorkstreamDetail, Workstream, WorkstreamAction, WriteCounts};
use crate::connectors::calendar;
use crate::connectors::email;

const META_LAST_CLUSTERED: &str = "last_clustered_ms";

// ----- Synthesizer input shape (parsed from Claude's JSON) -----------------

#[derive(Debug, Clone)]
pub struct SynthesizedWorkstream {
    /// `Some(id)` to update an existing workstream; `None` to insert
    /// a fresh one (we generate the id).
    pub id: Option<String>,
    pub title: String,
    pub summary: String,
    pub member_emails: Vec<String>,
    pub member_events: Vec<String>,
    pub member_notes: Vec<String>,
    pub actions: Vec<SynthesizedAction>,
    /// Optional status hint from Claude (#78). When set to `"active"`
    /// for a workstream that's currently archived, the synthesizer
    /// runs `resurrect_if_archived`. Other values are ignored — archive
    /// flow is user-driven only.
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SynthesizedAction {
    pub text: String,
    pub due_ms: Option<i64>,
    pub source_kind: String,
    pub source_id: String,
}

// ----- meta key/value ------------------------------------------------------

pub fn last_clustered_ms(conn: &Connection) -> rusqlite::Result<i64> {
    let s: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![META_LAST_CLUSTERED],
            |r| r.get(0),
        )
        .optional()?;
    Ok(s.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0))
}

pub fn set_last_clustered_ms(conn: &Connection, ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![META_LAST_CLUSTERED, ms.to_string()],
    )?;
    Ok(())
}

// ----- Read helpers --------------------------------------------------------

pub fn list_workstreams_active(conn: &Connection) -> rusqlite::Result<Vec<Workstream>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.title, w.summary, w.status, w.last_activity_ms, w.created_ms, w.updated_ms, \
                w.user_notes, w.archived_at_ms, w.reopened_at_ms, w.owner_member_id, \
                COALESCE((SELECT COUNT(*) FROM workstream_emails WHERE workstream_id = w.id), 0) AS ec, \
                COALESCE((SELECT COUNT(*) FROM workstream_events WHERE workstream_id = w.id), 0) AS evc, \
                COALESCE((SELECT COUNT(*) FROM workstream_notes  WHERE workstream_id = w.id), 0) AS nc, \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0) AS ac \
         FROM workstreams w \
         WHERE w.status = 'active' \
         ORDER BY w.last_activity_ms DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Workstream {
            id: r.get(0)?,
            title: r.get(1)?,
            summary: r.get(2)?,
            status: r.get(3)?,
            last_activity_ms: r.get(4)?,
            created_ms: r.get(5)?,
            updated_ms: r.get(6)?,
            user_notes: r.get(7)?,
            archived_at_ms: r.get(8)?,
            reopened_at_ms: r.get(9)?,
            owner_member_id: r.get(10)?,
            members: Vec::new(),
            email_count: r.get::<_, i64>(11)? as u32,
            event_count: r.get::<_, i64>(12)? as u32,
            note_count: r.get::<_, i64>(13)? as u32,
            open_action_count: r.get::<_, i64>(14)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    attach_members(conn, &mut out)?;
    Ok(out)
}

/// Archived workstreams ordered by archive time (most recently archived
/// first). Used by the Workstreams view's "Archived (N)" accordion (#78).
pub fn list_workstreams_archived(
    conn: &Connection,
) -> rusqlite::Result<Vec<Workstream>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.title, w.summary, w.status, w.last_activity_ms, w.created_ms, w.updated_ms, \
                w.user_notes, w.archived_at_ms, w.reopened_at_ms, w.owner_member_id, \
                COALESCE((SELECT COUNT(*) FROM workstream_emails WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_events WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_notes  WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0) \
         FROM workstreams w \
         WHERE w.status = 'archived' \
         ORDER BY w.archived_at_ms DESC NULLS LAST",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Workstream {
            id: r.get(0)?,
            title: r.get(1)?,
            summary: r.get(2)?,
            status: r.get(3)?,
            last_activity_ms: r.get(4)?,
            created_ms: r.get(5)?,
            updated_ms: r.get(6)?,
            user_notes: r.get(7)?,
            archived_at_ms: r.get(8)?,
            reopened_at_ms: r.get(9)?,
            owner_member_id: r.get(10)?,
            members: Vec::new(),
            email_count: r.get::<_, i64>(11)? as u32,
            event_count: r.get::<_, i64>(12)? as u32,
            note_count: r.get::<_, i64>(13)? as u32,
            open_action_count: r.get::<_, i64>(14)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    attach_members(conn, &mut out)?;
    Ok(out)
}

/// Both active and archived workstreams (NOT snoozed) returned as a
/// single list with an `is_archived` flag, for the synthesizer to
/// partition into separate prompt sections (#78).
///
/// Snoozed workstreams stay hidden from Claude entirely — we treat
/// "snooze" as "remind me later, don't include in synthesis", distinct
/// from archive's "this is done, only resurrect on clear continuation".
pub fn list_workstreams_for_synthesis(
    conn: &Connection,
) -> rusqlite::Result<Vec<(Workstream, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.title, w.summary, w.status, w.last_activity_ms, w.created_ms, w.updated_ms, \
                w.user_notes, w.archived_at_ms, w.reopened_at_ms, w.owner_member_id, \
                COALESCE((SELECT COUNT(*) FROM workstream_emails WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_events WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_notes  WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0) \
         FROM workstreams w \
         WHERE w.status IN ('active', 'archived') \
         ORDER BY \
            CASE w.status WHEN 'active' THEN 0 ELSE 1 END, \
            CASE WHEN w.status = 'active' THEN w.last_activity_ms ELSE w.archived_at_ms END DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let status: String = r.get(3)?;
        let is_archived = status == "archived";
        Ok((
            Workstream {
                id: r.get(0)?,
                title: r.get(1)?,
                summary: r.get(2)?,
                status,
                last_activity_ms: r.get(4)?,
                created_ms: r.get(5)?,
                updated_ms: r.get(6)?,
                user_notes: r.get(7)?,
                archived_at_ms: r.get(8)?,
                reopened_at_ms: r.get(9)?,
                owner_member_id: r.get(10)?,
                members: Vec::new(),
                email_count: r.get::<_, i64>(11)? as u32,
                event_count: r.get::<_, i64>(12)? as u32,
                note_count: r.get::<_, i64>(13)? as u32,
                open_action_count: r.get::<_, i64>(14)? as u32,
            },
            is_archived,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    // Attach members to the workstreams (mutating in place via a
    // throwaway view onto just the Workstream side of the tuple).
    let mut just_ws: Vec<Workstream> = out.iter().map(|(w, _)| w.clone()).collect();
    attach_members(conn, &mut just_ws)?;
    for (i, (w, _)) in out.iter_mut().enumerate() {
        w.members = std::mem::take(&mut just_ws[i].members);
    }
    Ok(out)
}

/// Look up a workstream's status without joining counts. Used by the
/// synthesizer's persist loop to decide whether a Claude-returned id
/// is referencing an existing archived workstream (#78). Returns `None`
/// if the workstream doesn't exist (a fresh ws_<uuid> will be inserted).
pub fn lookup_pre_status(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> rusqlite::Result<Option<String>> {
    let mut stmt = tx.prepare("SELECT status FROM workstreams WHERE id = ?1")?;
    stmt.query_row(params![id], |r| r.get::<_, String>(0))
        .optional()
}

pub fn get_workstream_detail(
    conn: &Connection,
    id: &str,
) -> rusqlite::Result<Option<WorkstreamDetail>> {
    let workstream = match get_workstream_one(conn, id)? {
        Some(w) => w,
        None => return Ok(None),
    };

    // Emails: join through pivot, hydrate via `email::get_message_details`.
    let mut stmt = conn.prepare(
        "SELECT message_id FROM workstream_emails WHERE workstream_id = ?1",
    )?;
    let mut emails = Vec::new();
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    for row in rows {
        if let Some(m) = email::get_message_details(conn, &row?)? {
            emails.push(m);
        }
    }
    // Sort by sent_at_ms desc.
    emails.sort_by(|a, b| b.sent_at_ms.cmp(&a.sent_at_ms));

    // Events: ditto via calendar::get_event_details.
    let mut stmt = conn.prepare(
        "SELECT event_id FROM workstream_events WHERE workstream_id = ?1",
    )?;
    let mut events = Vec::new();
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    for row in rows {
        if let Some(e) = calendar::get_event_details(conn, &row?)? {
            events.push(e);
        }
    }
    events.sort_by(|a, b| b.start_ms.cmp(&a.start_ms));

    // Notes: join workstream_notes against notes table for title.
    let mut stmt = conn.prepare(
        "SELECT wn.note_path, COALESCE(n.title, ''), COALESCE(n.modified_ms, 0) \
         FROM workstream_notes wn \
         LEFT JOIN notes n ON n.note_path = wn.note_path \
         WHERE wn.workstream_id = ?1 \
         ORDER BY n.modified_ms DESC",
    )?;
    let note_rows = stmt.query_map(params![id], |r| {
        Ok(NoteRef {
            note_path: r.get(0)?,
            title: r.get(1)?,
            modified_ms: r.get(2)?,
        })
    })?;
    let notes = note_rows.collect::<Result<Vec<_>, _>>()?;

    let actions = list_actions_for(conn, id)?;

    Ok(Some(WorkstreamDetail {
        workstream,
        emails,
        events,
        notes,
        actions,
    }))
}

fn get_workstream_one(conn: &Connection, id: &str) -> rusqlite::Result<Option<Workstream>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.title, w.summary, w.status, w.last_activity_ms, w.created_ms, w.updated_ms, \
                w.user_notes, w.archived_at_ms, w.reopened_at_ms, w.owner_member_id, \
                COALESCE((SELECT COUNT(*) FROM workstream_emails WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_events WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_notes  WHERE workstream_id = w.id), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0) \
         FROM workstreams w WHERE w.id = ?1",
    )?;
    let mut ws = stmt
        .query_row(params![id], |r| {
            Ok(Workstream {
                id: r.get(0)?,
                title: r.get(1)?,
                summary: r.get(2)?,
                status: r.get(3)?,
                last_activity_ms: r.get(4)?,
                created_ms: r.get(5)?,
                updated_ms: r.get(6)?,
                user_notes: r.get(7)?,
                archived_at_ms: r.get(8)?,
                reopened_at_ms: r.get(9)?,
                owner_member_id: r.get(10)?,
                members: Vec::new(),
                email_count: r.get::<_, i64>(11)? as u32,
                event_count: r.get::<_, i64>(12)? as u32,
                note_count: r.get::<_, i64>(13)? as u32,
                open_action_count: r.get::<_, i64>(14)? as u32,
            })
        })
        .optional()?;
    if let Some(ref mut w) = ws {
        let mut single = vec![std::mem::take(w)];
        attach_members(conn, &mut single)?;
        *w = single.pop().unwrap();
    }
    Ok(ws)
}

/// Bulk-derive members for a slice of workstreams (#81). Members are
/// the team_member ids that resolve from the workstream's email
/// recipients and event attendees. One UNION query covers all rows in
/// the slice — far cheaper than per-workstream fetches in the list
/// view. No-op when the slice is empty.
fn attach_members(
    conn: &Connection,
    workstreams: &mut [Workstream],
) -> rusqlite::Result<()> {
    if workstreams.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?")
        .take(workstreams.len())
        .collect::<Vec<_>>()
        .join(",");
    // Two halves UNION'd then DISTINCT'd at the (workstream, member) level.
    // Each member appears once per workstream regardless of how many
    // emails / events they're on.
    let sql = format!(
        "SELECT DISTINCT workstream_id, member_id FROM ( \
            SELECT we.workstream_id, er.team_member_id AS member_id \
            FROM workstream_emails we \
            JOIN email_recipients er ON er.message_id = we.message_id \
            WHERE we.workstream_id IN ({placeholders}) AND er.team_member_id IS NOT NULL \
            UNION \
            SELECT wev.workstream_id, ca.team_member_id AS member_id \
            FROM workstream_events wev \
            JOIN calendar_attendees ca ON ca.event_id = wev.event_id \
            WHERE wev.workstream_id IN ({placeholders}) AND ca.team_member_id IS NOT NULL \
         ) ORDER BY workstream_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    // The IN list appears twice in the UNION; bind both.
    let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(workstreams.len() * 2);
    for w in workstreams.iter() {
        params_vec.push(&w.id as &dyn rusqlite::ToSql);
    }
    for w in workstreams.iter() {
        params_vec.push(&w.id as &dyn rusqlite::ToSql);
    }
    let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut by_id: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in rows {
        let (ws_id, member_id) = row?;
        by_id.entry(ws_id).or_default().push(member_id);
    }
    for w in workstreams.iter_mut() {
        if let Some(members) = by_id.remove(&w.id) {
            w.members = members;
        }
    }
    Ok(())
}

fn list_actions_for(
    conn: &Connection,
    workstream_id: &str,
) -> rusqlite::Result<Vec<WorkstreamAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, workstream_id, text, due_ms, source_kind, source_id, done, created_ms \
         FROM workstream_actions WHERE workstream_id = ?1 \
         ORDER BY done ASC, created_ms DESC",
    )?;
    let rows = stmt.query_map(params![workstream_id], |r| {
        Ok(WorkstreamAction {
            id: r.get(0)?,
            workstream_id: r.get(1)?,
            text: r.get(2)?,
            due_ms: r.get(3)?,
            source_kind: r.get(4)?,
            source_id: r.get(5)?,
            done: r.get::<_, i64>(6)? != 0,
            created_ms: r.get(7)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
}

// ----- Write helpers -------------------------------------------------------

/// Upsert a workstream + replace its pivot sets + upsert actions in a
/// single transaction. Returns the per-workstream contribution to the
/// outer ClusterReport.
///
/// `record.id` is the existing workstream id when the synthesizer
/// recognized this thread as a continuation; otherwise we generate a
/// fresh `ws_<uuid>`. Action ids are content-hash so re-runs preserve
/// the user's `done` flag.
pub fn write_workstream(
    tx: &rusqlite::Transaction<'_>,
    record: &SynthesizedWorkstream,
    now_ms: i64,
) -> rusqlite::Result<WriteCounts> {
    let mut counts = WriteCounts::default();

    let id = match &record.id {
        Some(s) if !s.is_empty() => s.clone(),
        _ => format!("ws_{}", uuid::Uuid::new_v4()),
    };
    let pre_existed: i64 = tx
        .query_row(
            "SELECT 1 FROM workstreams WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);

    // Last activity = max modified across joined items, fallback now.
    let last_activity = compute_last_activity(tx, record)?.unwrap_or(now_ms);

    if pre_existed == 0 {
        tx.execute(
            "INSERT INTO workstreams(id, title, summary, status, last_activity_ms, created_ms, updated_ms) \
             VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?5)",
            params![id, record.title, record.summary, last_activity, now_ms],
        )?;
        counts.workstream_added = true;
    } else {
        tx.execute(
            "UPDATE workstreams SET title = ?2, summary = ?3, last_activity_ms = ?4, updated_ms = ?5 \
             WHERE id = ?1",
            params![id, record.title, record.summary, last_activity, now_ms],
        )?;
    }

    // Replace pivots wholesale. Smaller than diffing for the typical
    // dozens-of-items per workstream.
    tx.execute(
        "DELETE FROM workstream_emails WHERE workstream_id = ?1",
        params![id],
    )?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO workstream_emails(workstream_id, message_id) VALUES (?1, ?2)",
        )?;
        for mid in &record.member_emails {
            stmt.execute(params![id, mid])?;
        }
    }

    tx.execute(
        "DELETE FROM workstream_events WHERE workstream_id = ?1",
        params![id],
    )?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO workstream_events(workstream_id, event_id) VALUES (?1, ?2)",
        )?;
        for eid in &record.member_events {
            stmt.execute(params![id, eid])?;
        }
    }

    tx.execute(
        "DELETE FROM workstream_notes WHERE workstream_id = ?1",
        params![id],
    )?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO workstream_notes(workstream_id, note_path) VALUES (?1, ?2)",
        )?;
        for np in &record.member_notes {
            stmt.execute(params![id, np])?;
        }
    }

    // Actions: upsert by hashed id. ON CONFLICT preserves `done` and
    // `created_ms` (the user's state); refreshes everything else.
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO workstream_actions(\
                id, workstream_id, text, due_ms, source_kind, source_id, done, created_ms\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7) \
             ON CONFLICT(id) DO UPDATE SET \
                text = excluded.text, \
                due_ms = excluded.due_ms, \
                source_kind = excluded.source_kind, \
                source_id = excluded.source_id",
        )?;
        for a in &record.actions {
            let aid = action_id(&id, &a.text);
            let pre_existed_action: i64 = tx
                .query_row(
                    "SELECT 1 FROM workstream_actions WHERE id = ?1",
                    params![aid],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            stmt.execute(params![
                aid,
                id,
                a.text,
                a.due_ms,
                a.source_kind,
                a.source_id,
                now_ms,
            ])?;
            if pre_existed_action == 0 {
                counts.actions_added += 1;
            } else {
                counts.actions_updated += 1;
            }
        }
    }

    Ok(counts)
}

fn compute_last_activity(
    tx: &rusqlite::Transaction<'_>,
    record: &SynthesizedWorkstream,
) -> rusqlite::Result<Option<i64>> {
    let mut max_ms: Option<i64> = None;

    if !record.member_emails.is_empty() {
        let placeholders = std::iter::repeat("?")
            .take(record.member_emails.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT MAX(sent_at_ms) FROM email_messages WHERE id IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql)?;
        let p: Vec<&dyn rusqlite::ToSql> = record
            .member_emails
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let v: Option<i64> = stmt
            .query_row(rusqlite::params_from_iter(p), |r| r.get(0))
            .optional()?
            .flatten();
        max_ms = max_opt(max_ms, v);
    }

    if !record.member_events.is_empty() {
        let placeholders = std::iter::repeat("?")
            .take(record.member_events.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT MAX(start_ms) FROM calendar_events WHERE id IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql)?;
        let p: Vec<&dyn rusqlite::ToSql> = record
            .member_events
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let v: Option<i64> = stmt
            .query_row(rusqlite::params_from_iter(p), |r| r.get(0))
            .optional()?
            .flatten();
        max_ms = max_opt(max_ms, v);
    }

    if !record.member_notes.is_empty() {
        let placeholders = std::iter::repeat("?")
            .take(record.member_notes.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT MAX(modified_ms) FROM notes WHERE note_path IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql)?;
        let p: Vec<&dyn rusqlite::ToSql> = record
            .member_notes
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let v: Option<i64> = stmt
            .query_row(rusqlite::params_from_iter(p), |r| r.get(0))
            .optional()?
            .flatten();
        max_ms = max_opt(max_ms, v);
    }

    Ok(max_ms)
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

pub fn set_action_done(
    conn: &Connection,
    action_id: &str,
    done: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE workstream_actions SET done = ?2 WHERE id = ?1",
        params![action_id, done as i64],
    )?;
    Ok(())
}

/// Apply a user-driven status change (#78). Stamps the appropriate
/// timestamps for the lifecycle:
/// - `archived`: stamps `archived_at_ms = now`, clears `reopened_at_ms`.
/// - `active`: clears both `archived_at_ms` and `reopened_at_ms` for a
///   clean slate. The synthesizer-driven resurrect path uses
///   `resurrect_if_archived` instead, which preserves `archived_at_ms`
///   as historical record.
/// - `snoozed`: status only, no timestamps touched.
pub fn set_status(conn: &Connection, id: &str, status: &str) -> rusqlite::Result<()> {
    let now = now_ms();
    match status {
        "archived" => {
            conn.execute(
                "UPDATE workstreams SET status = 'archived', archived_at_ms = ?2, \
                                        reopened_at_ms = NULL, updated_ms = ?2 \
                 WHERE id = ?1",
                params![id, now],
            )?;
        }
        "active" => {
            conn.execute(
                "UPDATE workstreams SET status = 'active', archived_at_ms = NULL, \
                                        reopened_at_ms = NULL, updated_ms = ?2 \
                 WHERE id = ?1",
                params![id, now],
            )?;
        }
        _ => {
            // snoozed (or any future state): touch only status + updated.
            conn.execute(
                "UPDATE workstreams SET status = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id, status, now],
            )?;
        }
    }
    Ok(())
}

/// Set or clear a workstream's owner (#81). Pass `None` to unassign.
/// `write_workstream` deliberately doesn't touch this column, so
/// owner survives re-clusters — same pattern as `user_notes` and
/// `linked_note_path`.
pub fn set_owner(
    conn: &Connection,
    id: &str,
    owner_member_id: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE workstreams SET owner_member_id = ?2, updated_ms = ?3 WHERE id = ?1",
        params![id, owner_member_id, now_ms()],
    )?;
    Ok(())
}

/// Synthesizer-driven resurrect: flip an archived workstream back to
/// active, stamp `reopened_at_ms = now`, leave `archived_at_ms` as
/// historical record. No-op if the workstream isn't currently
/// archived (defensive — Claude may emit `status: "active"` for an
/// already-active workstream). Returns `true` when an actual flip
/// happened so the caller can count it for the ClusterReport.
pub fn resurrect_if_archived(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let updated = tx.execute(
        "UPDATE workstreams SET status = 'active', reopened_at_ms = ?2, updated_ms = ?2 \
         WHERE id = ?1 AND status = 'archived'",
        params![id, now_ms],
    )?;
    Ok(updated > 0)
}

/// Clear the `reopened_at_ms` marker. Called from the detail view's
/// unmount cleanup when the user has visited a reopened workstream
/// (#78), so the "Reopened" badge stops showing on subsequent list
/// renders.
pub fn mark_seen(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE workstreams SET reopened_at_ms = NULL, updated_ms = ?2 WHERE id = ?1",
        params![id, now_ms()],
    )?;
    Ok(())
}

/// Update a workstream's user-authored context notes (#77). `None`
/// clears the field. Caller is responsible for trimming whitespace
/// and mapping empty strings to `None` so the prompt-omission logic
/// downstream stays simple.
pub fn set_user_notes(
    conn: &Connection,
    id: &str,
    notes: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE workstreams SET user_notes = ?2, updated_ms = ?3 WHERE id = ?1",
        params![id, notes, now_ms()],
    )?;
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Stable id for a workstream action: sha256 of `workstream_id\ntrim(text)`.
/// Re-runs that produce the same workstream + same action text generate
/// the same id, so the upsert preserves any user-set `done`.
pub fn action_id(workstream_id: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(workstream_id.as_bytes());
    h.update(b"\n");
    h.update(text.trim().as_bytes());
    format!("wsa_{:x}", h.finalize())
}

// ----- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Minimal schema replica needed for the workstreams tests.
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
        conn.execute_batch(include_str!("../migrations/014_workstream_archive_resurface.sql"))
            .unwrap();
        // 015 references team_members; the test setup already has the
        // table created above, so the FK reference resolves at migrate time.
        conn.execute_batch(include_str!("../migrations/015_workstream_owner.sql"))
            .unwrap();
        conn
    }

    fn seed_email(conn: &Connection, id: &str, sent_at: i64) {
        conn.execute(
            "INSERT INTO email_messages(\
                id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 't1', 'Sub', 'a@e', NULL, ?2, NULL, NULL, 0, 0, NULL, ?2)",
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

    fn make_ws(id: Option<&str>, title: &str, emails: &[&str], events: &[&str], notes: &[&str], actions: Vec<SynthesizedAction>) -> SynthesizedWorkstream {
        SynthesizedWorkstream {
            id: id.map(|s| s.to_string()),
            title: title.to_string(),
            summary: format!("Summary of {title}"),
            member_emails: emails.iter().map(|s| s.to_string()).collect(),
            member_events: events.iter().map(|s| s.to_string()).collect(),
            member_notes: notes.iter().map(|s| s.to_string()).collect(),
            actions,
            status: None,
        }
    }

    fn make_action(text: &str, source_kind: &str, source_id: &str) -> SynthesizedAction {
        SynthesizedAction {
            text: text.to_string(),
            due_ms: None,
            source_kind: source_kind.to_string(),
            source_id: source_id.to_string(),
        }
    }

    #[test]
    fn last_clustered_ms_default_is_zero_then_round_trips() {
        let conn = open_test_db();
        assert_eq!(last_clustered_ms(&conn).unwrap(), 0);
        set_last_clustered_ms(&conn, 1_700_000_000_000).unwrap();
        assert_eq!(last_clustered_ms(&conn).unwrap(), 1_700_000_000_000);
    }

    #[test]
    fn write_workstream_inserts_new_record() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_event(&conn, "mg:test::e1", 2_000);
        seed_note(&conn, "/n/a.md", 3_000);

        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            None,
            "Hyundai POC",
            &["mg:test::m1"],
            &["mg:test::e1"],
            &["/n/a.md"],
            vec![make_action("Send invoice", "email", "mg:test::m1")],
        );
        let counts = write_workstream(&tx, &ws, 5_000).unwrap();
        tx.commit().unwrap();

        assert!(counts.workstream_added);
        assert_eq!(counts.actions_added, 1);

        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Hyundai POC");
        assert_eq!(active[0].email_count, 1);
        assert_eq!(active[0].event_count, 1);
        assert_eq!(active[0].note_count, 1);
        assert_eq!(active[0].open_action_count, 1);
        // last_activity = max(1000, 2000, 3000) = 3000
        assert_eq!(active[0].last_activity_ms, 3_000);
    }

    #[test]
    fn write_workstream_updates_existing_and_replaces_pivots() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_email(&conn, "mg:test::m2", 4_000);
        seed_event(&conn, "mg:test::e1", 2_000);

        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            Some("ws_existing"),
            "Workstream A",
            &["mg:test::m1"],
            &["mg:test::e1"],
            &[],
            vec![],
        );
        write_workstream(&tx, &ws, 1_000).unwrap();
        tx.commit().unwrap();

        // Re-sync: replace m1 with m2, drop the event entirely, change title.
        let tx = conn.transaction().unwrap();
        let ws2 = make_ws(
            Some("ws_existing"),
            "Workstream A (renamed)",
            &["mg:test::m2"],
            &[],
            &[],
            vec![],
        );
        let counts = write_workstream(&tx, &ws2, 2_000).unwrap();
        tx.commit().unwrap();
        assert!(!counts.workstream_added);

        let detail = get_workstream_detail(&conn, "ws_existing").unwrap().unwrap();
        assert_eq!(detail.workstream.title, "Workstream A (renamed)");
        assert_eq!(detail.emails.len(), 1);
        assert_eq!(detail.emails[0].id, "mg:test::m2");
        assert_eq!(detail.events.len(), 0);
    }

    #[test]
    fn write_workstream_preserves_action_done_on_resync() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);

        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws1"),
                "WS",
                &["mg:test::m1"],
                &[],
                &[],
                vec![make_action("Reply to invoice", "email", "mg:test::m1")],
            ),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        // User checks the action.
        let aid = action_id("ws1", "Reply to invoice");
        set_action_done(&conn, &aid, true).unwrap();
        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert_eq!(detail.actions.len(), 1);
        assert!(detail.actions[0].done);

        // Re-sync emits the same action text.
        let tx = conn.transaction().unwrap();
        let counts = write_workstream(
            &tx,
            &make_ws(
                Some("ws1"),
                "WS",
                &["mg:test::m1"],
                &[],
                &[],
                vec![make_action("Reply to invoice", "email", "mg:test::m1")],
            ),
            2_000,
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(counts.actions_added, 0);
        assert_eq!(counts.actions_updated, 1);

        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert!(
            detail.actions[0].done,
            "done flag must survive a re-sync of the same action text"
        );
    }

    #[test]
    fn list_workstreams_active_excludes_archived() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_a"), "Active", &[], &[], &[], vec![]),
            1_000,
        )
        .unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_b"), "Archived", &[], &[], &[], vec![]),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();
        set_status(&conn, "ws_b", "archived").unwrap();

        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "ws_a");
    }

    #[test]
    fn set_action_done_round_trips() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);

        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws1"),
                "WS",
                &["mg:test::m1"],
                &[],
                &[],
                vec![make_action("Do thing", "email", "mg:test::m1")],
            ),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let aid = action_id("ws1", "Do thing");
        set_action_done(&conn, &aid, true).unwrap();
        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert!(detail.actions[0].done);
        set_action_done(&conn, &aid, false).unwrap();
        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert!(!detail.actions[0].done);
    }

    #[test]
    fn get_workstream_detail_returns_joined_emails_events_notes_and_actions() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_email(&conn, "mg:test::m2", 5_000);
        seed_event(&conn, "mg:test::e1", 2_000);
        seed_note(&conn, "/n/a.md", 3_000);

        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws_x"),
                "X",
                &["mg:test::m1", "mg:test::m2"],
                &["mg:test::e1"],
                &["/n/a.md"],
                vec![
                    make_action("a1", "email", "mg:test::m1"),
                    make_action("a2", "note", "/n/a.md"),
                ],
            ),
            10_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let d = get_workstream_detail(&conn, "ws_x").unwrap().unwrap();
        assert_eq!(d.emails.len(), 2);
        // Sorted desc by sent_at_ms: m2 (5000), m1 (1000).
        assert_eq!(d.emails[0].id, "mg:test::m2");
        assert_eq!(d.emails[1].id, "mg:test::m1");
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.notes.len(), 1);
        assert_eq!(d.notes[0].note_path, "/n/a.md");
        assert_eq!(d.actions.len(), 2);
    }

    #[test]
    fn action_id_is_stable_for_same_inputs() {
        let id1 = action_id("ws1", "Do thing");
        let id2 = action_id("ws1", "Do thing");
        assert_eq!(id1, id2);

        let id3 = action_id("ws2", "Do thing");
        assert_ne!(id1, id3);
        let id4 = action_id("ws1", "Do other thing");
        assert_ne!(id1, id4);
    }

    #[test]
    fn action_id_is_trim_normalized() {
        // Trimming the text means whitespace drift in Claude output
        // doesn't fragment action history.
        let id1 = action_id("ws1", "Do thing");
        let id2 = action_id("ws1", "  Do thing  ");
        assert_eq!(id1, id2);
        // But internal whitespace still matters (different action).
        let id3 = action_id("ws1", "Do  thing");
        assert_ne!(id1, id3);
    }

    // ----- user_notes (#77) ------------------------------------------------

    #[test]
    fn set_user_notes_round_trips() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(&tx, &make_ws(Some("ws_n"), "WS", &[], &[], &[], vec![]), 1_000)
            .unwrap();
        tx.commit().unwrap();

        // Default: no notes.
        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active[0].user_notes, None);

        // Set, read back.
        set_user_notes(&conn, "ws_n", Some("Real deadline May 30")).unwrap();
        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active[0].user_notes.as_deref(), Some("Real deadline May 30"));

        // Detail view returns it too.
        let detail = get_workstream_detail(&conn, "ws_n").unwrap().unwrap();
        assert_eq!(
            detail.workstream.user_notes.as_deref(),
            Some("Real deadline May 30")
        );
    }

    #[test]
    fn set_user_notes_with_none_clears_field() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(&tx, &make_ws(Some("ws_c"), "WS", &[], &[], &[], vec![]), 1_000)
            .unwrap();
        tx.commit().unwrap();
        set_user_notes(&conn, "ws_c", Some("placeholder")).unwrap();
        set_user_notes(&conn, "ws_c", None).unwrap();
        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active[0].user_notes, None);
    }

    #[test]
    fn write_workstream_does_not_overwrite_user_notes_on_resync() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(&tx, &make_ws(Some("ws_r"), "Hyundai", &[], &[], &[], vec![]), 1_000)
            .unwrap();
        tx.commit().unwrap();
        set_user_notes(&conn, "ws_r", Some("This is the new POC.")).unwrap();

        // Re-sync the same workstream (fresh title, no carry-over of
        // user_notes from the synthesizer side — the SynthesizedWorkstream
        // shape doesn't have user_notes at all).
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_r"), "Hyundai (renamed)", &[], &[], &[], vec![]),
            2_000,
        )
        .unwrap();
        tx.commit().unwrap();

        // Title updated, but user_notes survived.
        let detail = get_workstream_detail(&conn, "ws_r").unwrap().unwrap();
        assert_eq!(detail.workstream.title, "Hyundai (renamed)");
        assert_eq!(
            detail.workstream.user_notes.as_deref(),
            Some("This is the new POC."),
            "user_notes must survive a re-sync that doesn't carry it"
        );
    }

    // ----- archive + resurface (#78) ---------------------------------------

    fn seed_workstream(conn: &mut Connection, id: &str) {
        let tx = conn.transaction().unwrap();
        write_workstream(&tx, &make_ws(Some(id), "WS", &[], &[], &[], vec![]), 1_000)
            .unwrap();
        tx.commit().unwrap();
    }

    fn fetch_one_raw(conn: &Connection, id: &str) -> (String, Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT status, archived_at_ms, reopened_at_ms FROM workstreams WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn set_status_to_archived_stamps_archived_at_ms() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_a");
        set_status(&conn, "ws_a", "archived").unwrap();
        let (status, archived, reopened) = fetch_one_raw(&conn, "ws_a");
        assert_eq!(status, "archived");
        assert!(archived.is_some());
        assert!(reopened.is_none());
    }

    #[test]
    fn set_status_to_active_clears_both_timestamps() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_b");
        set_status(&conn, "ws_b", "archived").unwrap();
        // Simulate a synthesizer-driven resurrect setting reopened_at_ms.
        let tx = conn.transaction().unwrap();
        resurrect_if_archived(&tx, "ws_b", 5_000).unwrap();
        tx.commit().unwrap();
        // Now the user manually unarchives via the status pill.
        set_status(&conn, "ws_b", "active").unwrap();
        let (status, archived, reopened) = fetch_one_raw(&conn, "ws_b");
        assert_eq!(status, "active");
        assert_eq!(archived, None, "manual unarchive should clear archived_at_ms");
        assert_eq!(reopened, None, "manual unarchive should clear reopened_at_ms");
    }

    #[test]
    fn resurrect_if_archived_flips_status_and_stamps_reopened() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_c");
        set_status(&conn, "ws_c", "archived").unwrap();
        let (_, archived_before, _) = fetch_one_raw(&conn, "ws_c");

        let tx = conn.transaction().unwrap();
        let flipped = resurrect_if_archived(&tx, "ws_c", 9_999).unwrap();
        tx.commit().unwrap();
        assert!(flipped);

        let (status, archived_after, reopened) = fetch_one_raw(&conn, "ws_c");
        assert_eq!(status, "active");
        assert_eq!(reopened, Some(9_999));
        assert_eq!(
            archived_after, archived_before,
            "synthesizer-driven resurrect should preserve archived_at_ms as history"
        );
    }

    #[test]
    fn resurrect_if_archived_is_no_op_when_already_active() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_d");
        let tx = conn.transaction().unwrap();
        let flipped = resurrect_if_archived(&tx, "ws_d", 1_111).unwrap();
        tx.commit().unwrap();
        assert!(!flipped);
        let (status, _, reopened) = fetch_one_raw(&conn, "ws_d");
        assert_eq!(status, "active");
        assert!(reopened.is_none());
    }

    #[test]
    fn mark_seen_clears_reopened_at_ms() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_e");
        set_status(&conn, "ws_e", "archived").unwrap();
        let tx = conn.transaction().unwrap();
        resurrect_if_archived(&tx, "ws_e", 2_000).unwrap();
        tx.commit().unwrap();
        assert_eq!(fetch_one_raw(&conn, "ws_e").2, Some(2_000));

        mark_seen(&conn, "ws_e").unwrap();
        assert_eq!(fetch_one_raw(&conn, "ws_e").2, None);
    }

    #[test]
    fn list_workstreams_for_synthesis_returns_active_and_archived_only() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_active");
        seed_workstream(&mut conn, "ws_archived");
        seed_workstream(&mut conn, "ws_snoozed");
        set_status(&conn, "ws_archived", "archived").unwrap();
        set_status(&conn, "ws_snoozed", "snoozed").unwrap();

        let rows = list_workstreams_for_synthesis(&conn).unwrap();
        let ids: Vec<&str> = rows.iter().map(|(w, _)| w.id.as_str()).collect();
        assert!(ids.contains(&"ws_active"));
        assert!(ids.contains(&"ws_archived"));
        assert!(!ids.contains(&"ws_snoozed"), "snoozed workstreams must not surface to the synthesizer");

        // is_archived flag matches status.
        for (w, is_archived) in &rows {
            assert_eq!(*is_archived, w.status == "archived");
        }
    }

    #[test]
    fn list_workstreams_archived_orders_by_archived_at_desc() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_old");
        seed_workstream(&mut conn, "ws_new");
        set_status(&conn, "ws_old", "archived").unwrap();
        // Force a later archived_at_ms by sleeping briefly via direct SQL stamp
        // (now_ms() resolution is millis but tests can run within the same ms).
        std::thread::sleep(std::time::Duration::from_millis(2));
        set_status(&conn, "ws_new", "archived").unwrap();

        let archived = list_workstreams_archived(&conn).unwrap();
        let ids: Vec<&str> = archived.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, vec!["ws_new", "ws_old"]);
    }

    #[test]
    fn lookup_pre_status_returns_none_for_missing_workstream() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        let res = lookup_pre_status(&tx, "ws_nope").unwrap();
        tx.commit().unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn lookup_pre_status_returns_status_for_existing() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_x");
        set_status(&conn, "ws_x", "archived").unwrap();
        let tx = conn.transaction().unwrap();
        let res = lookup_pre_status(&tx, "ws_x").unwrap();
        tx.commit().unwrap();
        assert_eq!(res.as_deref(), Some("archived"));
    }

    // ----- owner + members (#81) -------------------------------------------

    #[test]
    fn set_owner_round_trips() {
        let mut conn = open_test_db();
        conn.execute("INSERT INTO team_members(id) VALUES ('tm:tj')", [])
            .unwrap();
        seed_workstream(&mut conn, "ws_o");
        // Default: no owner.
        assert_eq!(
            list_workstreams_active(&conn).unwrap()[0].owner_member_id,
            None
        );

        set_owner(&conn, "ws_o", Some("tm:tj")).unwrap();
        assert_eq!(
            list_workstreams_active(&conn).unwrap()[0].owner_member_id.as_deref(),
            Some("tm:tj")
        );

        set_owner(&conn, "ws_o", None).unwrap();
        assert_eq!(
            list_workstreams_active(&conn).unwrap()[0].owner_member_id,
            None
        );
    }

    #[test]
    fn write_workstream_does_not_overwrite_owner_on_resync() {
        let mut conn = open_test_db();
        conn.execute("INSERT INTO team_members(id) VALUES ('tm:tj')", [])
            .unwrap();
        seed_workstream(&mut conn, "ws_oo");
        set_owner(&conn, "ws_oo", Some("tm:tj")).unwrap();

        // Re-cluster pass refreshes title/summary; owner must survive.
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_oo"), "Renamed", &[], &[], &[], vec![]),
            2_000,
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            list_workstreams_active(&conn).unwrap()[0].owner_member_id.as_deref(),
            Some("tm:tj")
        );
    }

    #[test]
    fn members_derived_from_email_recipients_and_event_attendees() {
        let mut conn = open_test_db();
        // Seed two team members.
        conn.execute("INSERT INTO team_members(id) VALUES ('tm:heike'), ('tm:tj')", [])
            .unwrap();

        // An email + event referencing both. seed_email/seed_event from
        // the calendar tests above set up the rows; recipients/attendees
        // we add manually.
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_event(&conn, "mg:test::e1", 2_000);
        conn.execute(
            "INSERT INTO email_recipients(message_id, email, recipient_type, team_member_id) \
             VALUES ('mg:test::m1', 'heike@e.com', 'to', 'tm:heike')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_attendees(event_id, email, team_member_id) \
             VALUES ('mg:test::e1', 'tj@e.com', 'tm:tj')",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws_p"),
                "WS",
                &["mg:test::m1"],
                &["mg:test::e1"],
                &[],
                vec![],
            ),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let ws = &list_workstreams_active(&conn).unwrap()[0];
        let mut members = ws.members.clone();
        members.sort();
        assert_eq!(members, vec!["tm:heike".to_string(), "tm:tj".to_string()]);
    }

    #[test]
    fn members_excludes_unresolved_email_recipients() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        // Recipient with no team_member_id (NULL) — must not show up.
        conn.execute(
            "INSERT INTO email_recipients(message_id, email, recipient_type, team_member_id) \
             VALUES ('mg:test::m1', 'unknown@e.com', 'to', NULL)",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_q"), "WS", &["mg:test::m1"], &[], &[], vec![]),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        assert!(list_workstreams_active(&conn).unwrap()[0].members.is_empty());
    }
}
