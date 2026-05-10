//! Storage layer for workstreams + their pivots and actions.
//!
//! Mirrors the per-domain pattern from `connectors/calendar.rs` /
//! `connectors/email.rs`: small, transparent functions that take a
//! `Connection` (or a `Transaction` on the write side), no hidden
//! state, no caching. The synthesizer composes these into the
//! end-to-end cluster pass.

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::{
    ExternalParticipant, NoteRef, Workstream, WorkstreamAction, WorkstreamDetail, WorkstreamLink,
    WriteCounts,
};
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email'), 0) AS ec, \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event'), 0) AS evc, \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note'), 0) AS nc, \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0) AS ac, \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0) AS lc \
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
            link_count: r.get::<_, i64>(15)? as u32,
            external_participants: Vec::new(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    attach_members(conn, &mut out)?;
    attach_external_participants(conn, &mut out)?;
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0) \
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
            link_count: r.get::<_, i64>(15)? as u32,
            external_participants: Vec::new(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    attach_members(conn, &mut out)?;
    attach_external_participants(conn, &mut out)?;
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0) \
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
                link_count: r.get::<_, i64>(15)? as u32,
                external_participants: Vec::new(),
            },
            is_archived,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    // Attach members + external participants (mutating in place via a
    // throwaway view onto just the Workstream side of the tuple).
    let mut just_ws: Vec<Workstream> = out.iter().map(|(w, _)| w.clone()).collect();
    attach_members(conn, &mut just_ws)?;
    attach_external_participants(conn, &mut just_ws)?;
    for (i, (w, _)) in out.iter_mut().enumerate() {
        w.members = std::mem::take(&mut just_ws[i].members);
        w.external_participants =
            std::mem::take(&mut just_ws[i].external_participants);
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

    // Hydrate per-kind through the Signal registry (#85). Each
    // registered hydrator returns recency-desc; the registry routes
    // unknown kinds to an empty result so a stale signal pivot row
    // doesn't crash the detail view.
    let mut emails: Vec<email::EmailMessage> = Vec::new();
    let mut events: Vec<calendar::CalendarEvent> = Vec::new();
    let mut notes: Vec<NoteRef> = Vec::new();
    let by_kind = super::signals::load_and_hydrate_for_workstream(conn, id)?;
    for (_kind, hydrated) in by_kind {
        for h in hydrated {
            match h {
                super::signals::HydratedSignal::Email(m) => emails.push(m),
                super::signals::HydratedSignal::Event(e) => events.push(e),
                super::signals::HydratedSignal::Note(n) => notes.push(n),
            }
        }
    }

    let actions = list_actions_for(conn, id)?;
    let links = list_workstream_links(conn, id)?;

    Ok(Some(WorkstreamDetail {
        workstream,
        emails,
        events,
        notes,
        actions,
        links,
    }))
}

// ----- User-curated links (#88) -------------------------------------------

/// All links for a workstream, ordered by `(position, created_ms)` so
/// insertion order is preserved when `position` is left at the default.
pub fn list_workstream_links(
    conn: &Connection,
    workstream_id: &str,
) -> rusqlite::Result<Vec<WorkstreamLink>> {
    let mut stmt = conn.prepare(
        "SELECT id, workstream_id, label, url, kind, position, created_ms \
         FROM workstream_links \
         WHERE workstream_id = ?1 \
         ORDER BY position ASC, created_ms ASC",
    )?;
    let rows = stmt.query_map(params![workstream_id], |r| {
        Ok(WorkstreamLink {
            id: r.get(0)?,
            workstream_id: r.get(1)?,
            label: r.get(2)?,
            url: r.get(3)?,
            kind: r.get(4)?,
            position: r.get(5)?,
            created_ms: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Insert a new link at the end of the list. `position` is set to
/// `MAX(position) + 1` for the workstream so insertion order survives
/// even when the caller skips it. Trims label / url / kind; rejects
/// empty label or url.
pub fn add_workstream_link(
    conn: &Connection,
    workstream_id: &str,
    label: &str,
    url: &str,
    kind: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<WorkstreamLink> {
    let label = label.trim();
    let url = url.trim();
    if label.is_empty() || url.is_empty() {
        return Err(rusqlite::Error::InvalidParameterName(
            "label and url are required".to_string(),
        ));
    }
    let kind_owned = kind
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let next_position: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM workstream_links \
             WHERE workstream_id = ?1",
            params![workstream_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let id = format!("wsl_{}", uuid::Uuid::new_v4());
    conn.execute(
        "INSERT INTO workstream_links(id, workstream_id, label, url, kind, position, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, workstream_id, label, url, kind_owned, next_position, now_ms],
    )?;
    Ok(WorkstreamLink {
        id,
        workstream_id: workstream_id.to_string(),
        label: label.to_string(),
        url: url.to_string(),
        kind: kind_owned,
        position: next_position,
        created_ms: now_ms,
    })
}

/// Delete a single link by id. Returns whether a row was actually
/// removed; callers that pass a stale id can choose how to surface that.
pub fn remove_workstream_link(conn: &Connection, link_id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM workstream_links WHERE id = ?1",
        params![link_id],
    )?;
    Ok(n > 0)
}

fn get_workstream_one(conn: &Connection, id: &str) -> rusqlite::Result<Option<Workstream>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.title, w.summary, w.status, w.last_activity_ms, w.created_ms, w.updated_ms, \
                w.user_notes, w.archived_at_ms, w.reopened_at_ms, w.owner_member_id, \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_actions WHERE workstream_id = w.id AND done = 0), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0) \
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
                link_count: r.get::<_, i64>(15)? as u32,
                external_participants: Vec::new(),
            })
        })
        .optional()?;
    if let Some(ref mut w) = ws {
        let mut single = vec![std::mem::take(w)];
        attach_members(conn, &mut single)?;
        attach_external_participants(conn, &mut single)?;
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
            SELECT ws.workstream_id, er.team_member_id AS member_id \
            FROM workstream_signals ws \
            JOIN email_recipients er ON er.message_id = ws.item_id \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) AND er.team_member_id IS NOT NULL \
            UNION \
            SELECT ws.workstream_id, ca.team_member_id AS member_id \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) AND ca.team_member_id IS NOT NULL \
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

/// Cap on per-workstream external participants. Bounds the chip strip
/// + the AI-ask `External:` line — most workstreams have a handful of
/// counterparties; long mailing-list threads can produce hundreds, and
/// we want to surface the top recurring ones, not all of them.
const EXTERNAL_PARTICIPANT_CAP: usize = 12;

/// Bulk-derive external participants for a slice of workstreams (#?). An
/// "external participant" is an email address that appears in the
/// workstream's emails (recipients OR senders) or events (attendees)
/// but does NOT resolve to a `team_member`. Resolution rules:
///
///   * recipient/attendee rows are excluded when `team_member_id IS
///     NOT NULL` (sync-time resolution already mapped them).
///   * sender rows on `email_messages.from_email` are excluded when a
///     `team_member_aliases` row of kind 'email' matches the lowercased
///     address. (The senders path is the only place we re-check by
///     alias; the existing `attach_members` does not surface senders
///     at all today, which is a pre-existing gap not addressed here.)
///
/// The result is deduplicated case-insensitively by email; display
/// name is the first non-null encountered. Per-workstream count =
/// number of signal rows the email appeared on; ordered count desc,
/// email asc, capped at `EXTERNAL_PARTICIPANT_CAP`.
fn attach_external_participants(
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
    // Three halves: recipients (no team_member), senders (no matching
    // email-kind alias), attendees (no team_member). Each emits one row
    // per signal occurrence so the outer GROUP BY can count.
    let sql = format!(
        "SELECT workstream_id, email_lc, MAX(display_name) AS display_name, COUNT(*) AS cnt FROM ( \
            SELECT ws.workstream_id, LOWER(er.email) AS email_lc, er.display_name \
            FROM workstream_signals ws \
            JOIN email_recipients er ON er.message_id = ws.item_id \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND er.team_member_id IS NULL \
            UNION ALL \
            SELECT ws.workstream_id, LOWER(em.from_email) AS email_lc, em.from_name AS display_name \
            FROM workstream_signals ws \
            JOIN email_messages em ON em.id = ws.item_id \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND NOT EXISTS ( \
                SELECT 1 FROM team_member_aliases tma \
                WHERE tma.kind = 'email' AND LOWER(tma.value) = LOWER(em.from_email) \
              ) \
            UNION ALL \
            SELECT ws.workstream_id, LOWER(ca.email) AS email_lc, ca.display_name \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) \
              AND ca.team_member_id IS NULL \
         ) \
         WHERE email_lc IS NOT NULL AND email_lc <> '' \
         GROUP BY workstream_id, email_lc \
         ORDER BY workstream_id, cnt DESC, email_lc ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(workstreams.len() * 3);
    for _ in 0..3 {
        for w in workstreams.iter() {
            params_vec.push(&w.id as &dyn rusqlite::ToSql);
        }
    }
    let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |r| {
        Ok((
            r.get::<_, String>(0)?,        // workstream_id
            r.get::<_, String>(1)?,        // email_lc
            r.get::<_, Option<String>>(2)?, // display_name
            r.get::<_, i64>(3)?,           // count
        ))
    })?;
    let mut by_id: std::collections::HashMap<String, Vec<ExternalParticipant>> =
        std::collections::HashMap::new();
    for row in rows {
        let (ws_id, email, display_name, count) = row?;
        let bucket = by_id.entry(ws_id).or_default();
        if bucket.len() >= EXTERNAL_PARTICIPANT_CAP {
            continue;
        }
        bucket.push(ExternalParticipant {
            email,
            display_name: display_name.filter(|s| !s.trim().is_empty()),
            count: count as u32,
        });
    }
    for w in workstreams.iter_mut() {
        if let Some(externals) = by_id.remove(&w.id) {
            w.external_participants = externals;
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

    // Replace signals wholesale (#85). One DELETE + one INSERT loop
    // covers every kind. Smaller than diffing for the typical
    // dozens-of-items per workstream.
    tx.execute(
        "DELETE FROM workstream_signals WHERE workstream_id = ?1",
        params![id],
    )?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for mid in &record.member_emails {
            stmt.execute(params![id, "email", mid, now_ms])?;
        }
        for eid in &record.member_events {
            stmt.execute(params![id, "event", eid, now_ms])?;
        }
        for np in &record.member_notes {
            stmt.execute(params![id, "note", np, now_ms])?;
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

/// Drop signal pivot rows that point at items no longer present in
/// their domain table (#85). Soft FKs mean the cascade-on-delete the
/// old per-source pivots had via `ON DELETE CASCADE` is gone — this
/// runs once per cluster pass to keep the pivot tidy.
///
/// Cheap thanks to the `idx_signals_kind_item` index. Safe to run
/// concurrently with anything else inside the same Mutex<Connection>;
/// the synthesizer always serializes through that lock anyway.
pub fn cleanup_orphan_signals(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM workstream_signals \
         WHERE kind = 'email' AND item_id NOT IN (SELECT id FROM email_messages)",
        [],
    )?;
    conn.execute(
        "DELETE FROM workstream_signals \
         WHERE kind = 'event' AND item_id NOT IN (SELECT id FROM calendar_events)",
        [],
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
             CREATE TABLE team_members (
                 id      TEXT PRIMARY KEY,
                 aliases TEXT NOT NULL DEFAULT '[]'
             );
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
        // 016 collapses the per-source pivots into workstream_signals;
        // the migration backfills from the now-deleted pivots so it
        // must run before any test seeds workstreams.
        conn.execute_batch(include_str!("../migrations/016_workstream_signals.sql"))
            .unwrap();
        // 017 reshapes team_member_aliases — independent of workstreams
        // but the persist layer's count subqueries (#88) reference
        // tables created in 018 below, so we keep the migration order
        // intact.
        conn.execute_batch(include_str!("../migrations/017_typed_aliases.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/018_workstream_links.sql"))
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
    fn cleanup_orphan_signals_removes_dangling_emails_and_events() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_event(&conn, "mg:test::e1", 2_000);
        seed_workstream(&mut conn, "ws_orph");

        // Attach two real + two orphan signals.
        for (kind, item) in [
            ("email", "mg:test::m1"),
            ("email", "mg:test::missing"),
            ("event", "mg:test::e1"),
            ("event", "mg:test::also_missing"),
        ] {
            conn.execute(
                "INSERT INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
                 VALUES ('ws_orph', ?1, ?2, 0)",
                params![kind, item],
            )
            .unwrap();
        }
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM workstream_signals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 4);

        cleanup_orphan_signals(&conn).unwrap();

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM workstream_signals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 2, "orphans deleted, real ones kept");
        let orphan_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workstream_signals \
                 WHERE item_id IN ('mg:test::missing', 'mg:test::also_missing')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_left, 0);
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

    // ----- User-curated links (#88) ----------------------------------------
    //
    // These tests reuse the `seed_workstream` helper above so the FK on
    // `workstream_links` resolves through a real `write_workstream` call.

    #[test]
    fn add_workstream_link_assigns_monotonic_position() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");

        let a = add_workstream_link(&conn, "ws_l", "Repo", "https://x/y", Some("github"), 100).unwrap();
        let b = add_workstream_link(&conn, "ws_l", "Linear", "https://lin/p", Some("linear"), 110).unwrap();
        let c = add_workstream_link(&conn, "ws_l", "Notes", "https://n/d", None, 120).unwrap();
        assert_eq!(a.position, 0);
        assert_eq!(b.position, 1);
        assert_eq!(c.position, 2);
        assert_eq!(c.kind, None, "kind=None survives round-trip");
    }

    #[test]
    fn list_workstream_links_orders_by_position_then_created_ms() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        let _ = add_workstream_link(&conn, "ws_l", "A", "https://a", None, 100).unwrap();
        let _ = add_workstream_link(&conn, "ws_l", "B", "https://b", None, 110).unwrap();
        let _ = add_workstream_link(&conn, "ws_l", "C", "https://c", None, 120).unwrap();

        let listed = list_workstream_links(&conn, "ws_l").unwrap();
        let labels: Vec<&str> = listed.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, vec!["A", "B", "C"]);
    }

    #[test]
    fn add_workstream_link_rejects_empty_label_or_url() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        assert!(add_workstream_link(&conn, "ws_l", "  ", "https://x", None, 100).is_err());
        assert!(add_workstream_link(&conn, "ws_l", "Ok", "  ", None, 100).is_err());
    }

    #[test]
    fn remove_workstream_link_targets_only_the_named_id() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        let a = add_workstream_link(&conn, "ws_l", "A", "https://a", None, 100).unwrap();
        let b = add_workstream_link(&conn, "ws_l", "B", "https://b", None, 110).unwrap();

        let removed = remove_workstream_link(&conn, &a.id).unwrap();
        assert!(removed);
        let listed = list_workstream_links(&conn, "ws_l").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, b.id);

        // Re-removing the same id is a no-op (returns false).
        let removed_again = remove_workstream_link(&conn, &a.id).unwrap();
        assert!(!removed_again);
    }

    #[test]
    fn delete_workstream_cascades_links() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        let _ = add_workstream_link(&conn, "ws_l", "A", "https://a", None, 100).unwrap();
        let _ = add_workstream_link(&conn, "ws_l", "B", "https://b", None, 110).unwrap();
        assert_eq!(list_workstream_links(&conn, "ws_l").unwrap().len(), 2);

        conn.execute("DELETE FROM workstreams WHERE id = 'ws_l'", []).unwrap();
        assert!(list_workstream_links(&conn, "ws_l").unwrap().is_empty());
    }

    #[test]
    fn workstream_link_count_reflects_pivot_rows() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        let _ = add_workstream_link(&conn, "ws_l", "A", "https://a", None, 100).unwrap();
        let _ = add_workstream_link(&conn, "ws_l", "B", "https://b", None, 110).unwrap();

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_l").expect("present");
        assert_eq!(row.link_count, 2);
    }

    // ----- External participants ----------------------------------------

    /// Seed a workstream + an email-kind signal so the per-pivot
    /// queries have something to join against. Returns the workstream
    /// id for chaining test asserts.
    fn seed_workstream_with_email_signal(
        conn: &mut Connection,
        ws_id: &str,
        email_id: &str,
    ) {
        conn.execute(
            "INSERT INTO workstreams(id, title, summary, status, last_activity_ms, created_ms, updated_ms) \
             VALUES (?1, 'WS', '', 'active', 0, 0, 0)",
            params![ws_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
             VALUES (?1, 'email', ?2, 0)",
            params![ws_id, email_id],
        )
        .unwrap();
    }

    fn add_recipient(
        conn: &Connection,
        message_id: &str,
        email: &str,
        display_name: Option<&str>,
        team_member_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO email_recipients(message_id, email, display_name, recipient_type, team_member_id) \
             VALUES (?1, ?2, ?3, 'to', ?4)",
            params![message_id, email, display_name, team_member_id],
        )
        .unwrap();
    }

    /// Seed a member into the (test) team_members table so FKs from
    /// `email_recipients.team_member_id` resolve. Tests can then mark
    /// recipients as resolved by passing this id.
    fn seed_team_member(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO team_members(id) VALUES (?1)",
            params![id],
        )
        .unwrap();
    }

    /// Seed an email-kind alias on a team member so the externals
    /// query's NOT EXISTS check excludes that address from senders.
    fn seed_email_alias(conn: &Connection, member_id: &str, email: &str) {
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) \
             VALUES (?1, 'email', ?2)",
            params![member_id, email],
        )
        .unwrap();
    }

    /// Replace the auto-seeded sender on a previously-`seed_email`-ed row.
    /// Lets tests vary `from_email` per case without re-implementing
    /// the seed helper.
    fn set_email_sender(conn: &Connection, message_id: &str, from_email: &str) {
        conn.execute(
            "UPDATE email_messages SET from_email = ?2 WHERE id = ?1",
            params![message_id, from_email],
        )
        .unwrap();
    }

    #[test]
    fn external_participants_lists_recipients_with_null_team_id() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        set_email_sender(&conn, "mg:test::m1", "internal-noreply@x.io");
        // Two external recipients (no team_member_id) and one resolved.
        seed_team_member(&conn, "tm_tom");
        add_recipient(&conn, "mg:test::m1", "alice@example.com", Some("Alice"), None);
        add_recipient(&conn, "mg:test::m1", "bob@example.com", None, None);
        add_recipient(&conn, "mg:test::m1", "tom@x.io", Some("Tom"), Some("tm_tom"));
        // Mark the sender as a known team member by alias so it doesn't
        // pollute the externals.
        seed_team_member(&conn, "tm_noreply");
        seed_email_alias(&conn, "tm_noreply", "internal-noreply@x.io");

        seed_workstream_with_email_signal(&mut conn, "ws_x", "mg:test::m1");

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        let emails: Vec<&str> = row
            .external_participants
            .iter()
            .map(|p| p.email.as_str())
            .collect();
        assert_eq!(
            emails,
            vec!["alice@example.com", "bob@example.com"],
            "only the unresolved recipients surface; team_member rows are filtered"
        );
        assert_eq!(
            row.external_participants[0].display_name.as_deref(),
            Some("Alice")
        );
        assert!(row.external_participants[1].display_name.is_none());
    }

    #[test]
    fn external_participants_includes_senders_with_no_team_alias() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        // Sender has no matching team_member_aliases entry.
        set_email_sender(&conn, "mg:test::m1", "Outsider@External.com");
        // Recipients are all team members so they don't dominate the
        // signal — we want to verify the sender path on its own.
        seed_team_member(&conn, "tm_tom");
        add_recipient(&conn, "mg:test::m1", "tom@x.io", Some("Tom"), Some("tm_tom"));

        seed_workstream_with_email_signal(&mut conn, "ws_x", "mg:test::m1");

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        let emails: Vec<&str> = row
            .external_participants
            .iter()
            .map(|p| p.email.as_str())
            .collect();
        assert_eq!(
            emails,
            vec!["outsider@external.com"],
            "sender lowercased + included when no email-alias matches"
        );
    }

    #[test]
    fn external_participants_excludes_team_member_senders() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        set_email_sender(&conn, "mg:test::m1", "Tom@X.io");
        seed_team_member(&conn, "tm_tom");
        seed_email_alias(&conn, "tm_tom", "tom@x.io");
        // Add an external recipient so the helper has SOMETHING to
        // surface — keeps the assertion clear.
        add_recipient(&conn, "mg:test::m1", "alice@example.com", None, None);

        seed_workstream_with_email_signal(&mut conn, "ws_x", "mg:test::m1");

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        let emails: Vec<&str> = row
            .external_participants
            .iter()
            .map(|p| p.email.as_str())
            .collect();
        assert_eq!(
            emails,
            vec!["alice@example.com"],
            "team-member senders excluded by NOT EXISTS alias check"
        );
    }

    #[test]
    fn external_participants_dedupes_case_insensitively_and_picks_a_display_name() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_email(&conn, "mg:test::m2", 2_000);
        set_email_sender(&conn, "mg:test::m1", "system@noreply.io");
        set_email_sender(&conn, "mg:test::m2", "system@noreply.io");
        seed_team_member(&conn, "tm_noreply");
        seed_email_alias(&conn, "tm_noreply", "system@noreply.io");
        // Same external email across two messages, varying case + null name.
        add_recipient(&conn, "mg:test::m1", "Alice@Example.com", Some("Alice"), None);
        add_recipient(&conn, "mg:test::m2", "alice@example.com", None, None);

        conn.execute(
            "INSERT INTO workstreams(id, title, summary, status, last_activity_ms, created_ms, updated_ms) \
             VALUES ('ws_x', 'WS', '', 'active', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
             VALUES ('ws_x', 'email', 'mg:test::m1', 0), \
                    ('ws_x', 'email', 'mg:test::m2', 0)",
            [],
        )
        .unwrap();

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        assert_eq!(row.external_participants.len(), 1, "deduped case-insensitively");
        let p = &row.external_participants[0];
        assert_eq!(p.email, "alice@example.com");
        assert_eq!(p.display_name.as_deref(), Some("Alice"));
        assert_eq!(p.count, 2);
    }

    #[test]
    fn external_participants_orders_by_count_desc() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_email(&conn, "mg:test::m2", 2_000);
        seed_email(&conn, "mg:test::m3", 3_000);
        for id in ["mg:test::m1", "mg:test::m2", "mg:test::m3"] {
            set_email_sender(&conn, id, "noreply@x.io");
        }
        seed_team_member(&conn, "tm_noreply");
        seed_email_alias(&conn, "tm_noreply", "noreply@x.io");
        // bob shows up on 1 message, alice on 3.
        add_recipient(&conn, "mg:test::m1", "alice@x.io", None, None);
        add_recipient(&conn, "mg:test::m2", "alice@x.io", None, None);
        add_recipient(&conn, "mg:test::m3", "alice@x.io", None, None);
        add_recipient(&conn, "mg:test::m1", "bob@x.io", None, None);

        conn.execute(
            "INSERT INTO workstreams(id, title, summary, status, last_activity_ms, created_ms, updated_ms) \
             VALUES ('ws_x', 'WS', '', 'active', 0, 0, 0)",
            [],
        )
        .unwrap();
        for id in ["mg:test::m1", "mg:test::m2", "mg:test::m3"] {
            conn.execute(
                "INSERT INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
                 VALUES ('ws_x', 'email', ?1, 0)",
                params![id],
            )
            .unwrap();
        }

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        let pairs: Vec<(&str, u32)> = row
            .external_participants
            .iter()
            .map(|p| (p.email.as_str(), p.count))
            .collect();
        assert_eq!(pairs, vec![("alice@x.io", 3), ("bob@x.io", 1)]);
    }
}
