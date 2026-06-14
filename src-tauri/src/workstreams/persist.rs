//! Storage layer for workstreams + their pivots.
//!
//! Mirrors the per-domain pattern from `connectors/calendar.rs` /
//! `connectors/email.rs`: small, transparent functions that take a
//! `Connection` (or a `Transaction` on the write side), no hidden
//! state, no caching. The synthesizer composes these into the
//! end-to-end cluster pass.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::{
    ExternalParticipant, NoteRef, Workstream, WorkstreamDetail, WorkstreamLink,
    WriteCounts,
};
use crate::connectors::calendar::{self, CalendarEvent};
use crate::connectors::email::{self, EmailMessage};
use crate::connectors::teams::TeamsMessage;

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
    /// Optional status hint from Claude (#78). When set to `"active"`
    /// for a workstream that's currently archived, the synthesizer
    /// runs `resurrect_if_archived`. Other values are ignored — archive
    /// flow is user-driven only.
    pub status: Option<String>,
    /// Optional parent workstream id from Claude (#89). When set, the
    /// synthesizer's write path validates against the 2-level cap +
    /// self-parent / unknown-id rules; invalid values are silently
    /// dropped to NULL with a log line.
    pub parent_id: Option<String>,
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email' AND manual_detached_ms IS NULL), 0) AS ec, \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event' AND manual_detached_ms IS NULL), 0) AS evc, \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note' AND manual_detached_ms IS NULL), 0) AS nc, \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0) AS lc, \
                w.parent_workstream_id \
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
            link_count: r.get::<_, i64>(14)? as u32,
            parent_workstream_id: r.get(15)?,
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0), \
                w.parent_workstream_id \
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
            link_count: r.get::<_, i64>(14)? as u32,
            parent_workstream_id: r.get(15)?,
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0), \
                w.parent_workstream_id \
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
                link_count: r.get::<_, i64>(14)? as u32,
                parent_workstream_id: r.get(15)?,
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
    let mut teams_messages: Vec<crate::connectors::teams::TeamsMessage> = Vec::new();
    let by_kind = super::signals::load_and_hydrate_for_workstream(conn, id)?;
    for (_kind, hydrated) in by_kind {
        for h in hydrated {
            match h {
                super::signals::HydratedSignal::Email(m) => emails.push(m),
                super::signals::HydratedSignal::Event(e) => events.push(e),
                super::signals::HydratedSignal::Note(n) => notes.push(n),
                super::signals::HydratedSignal::TeamsMessage(t) => teams_messages.push(t),
            }
        }
    }

    let links = list_workstream_links(conn, id)?;
    let children = list_children_of(conn, id)?;

    Ok(Some(WorkstreamDetail {
        workstream,
        emails,
        events,
        notes,
        links,
        teams_messages,
        children,
    }))
}

/// All direct children of `parent_id`, ordered by recency (#89).
/// Reuses the same column shape as the list builders so `attach_members`
/// applies. Empty for leaves and standalones.
pub fn list_children_of(
    conn: &Connection,
    parent_id: &str,
) -> rusqlite::Result<Vec<Workstream>> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.title, w.summary, w.status, w.last_activity_ms, w.created_ms, w.updated_ms, \
                w.user_notes, w.archived_at_ms, w.reopened_at_ms, w.owner_member_id, \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0), \
                w.parent_workstream_id \
         FROM workstreams w \
         WHERE w.parent_workstream_id = ?1 \
         ORDER BY w.last_activity_ms DESC",
    )?;
    let rows = stmt.query_map(params![parent_id], |r| {
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
            link_count: r.get::<_, i64>(14)? as u32,
            parent_workstream_id: r.get(15)?,
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

// ----- User-curated links (#88) -------------------------------------------

/// All links for a workstream, ordered by `(position, created_ms)` so
/// insertion order is preserved when `position` is left at the default.
pub fn list_workstream_links(
    conn: &Connection,
    workstream_id: &str,
) -> rusqlite::Result<Vec<WorkstreamLink>> {
    let mut stmt = conn.prepare(
        "SELECT id, workstream_id, label, url, kind, position, created_ms, summary \
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
            summary: r.get(7)?,
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
/// Canonical kind strings accepted by the link writers. Mirrors the
/// soft enum in [`super::link_kinds`]; gives those constants their
/// only Rust caller and lets the writer reject typos before hitting
/// the schema (which itself stores `kind` as opaque TEXT).
const ALLOWED_LINK_KINDS: &[&str] = &[
    super::link_kinds::GITHUB,
    super::link_kinds::LINEAR,
    super::link_kinds::NOTION,
    super::link_kinds::FIGMA,
    super::link_kinds::OTHER,
];

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
    if let Some(k) = kind_owned.as_deref() {
        if !ALLOWED_LINK_KINDS.contains(&k) {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "unknown link kind: {k}"
            )));
        }
    }
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
        summary: None,
    })
}

/// Update a link's AI-generated summary. Used by the background
/// summarization task once Firecrawl + Haiku finish. Returns whether
/// the update touched a row — false means the link was removed
/// (or the workstream deleted) while the task was in flight; the
/// caller treats that as a harmless no-op.
pub fn set_workstream_link_summary(
    conn: &Connection,
    link_id: &str,
    summary: Option<&str>,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE workstream_links SET summary = ?2 WHERE id = ?1",
        params![link_id, summary],
    )?;
    Ok(n > 0)
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note' AND manual_detached_ms IS NULL), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_links WHERE workstream_id = w.id), 0), \
                w.parent_workstream_id \
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
                link_count: r.get::<_, i64>(14)? as u32,
                parent_workstream_id: r.get(15)?,
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
/// recipients, email senders, and event attendees. One UNION query
/// covers all rows in the slice — far cheaper than per-workstream
/// fetches in the list view. No-op when the slice is empty.
///
/// Resolution paths (each becomes a UNION branch):
///   1. recipient row with `team_member_id` already cached at sync time
///   2. recipient row with NULL `team_member_id` but matching a
///      `team_member_aliases` (kind='email') row added later — closes
///      the "I added my email today, but old recipient rows haven't
///      been re-synced" gap
///   3. email sender (`em.from_email`) matched via aliases — senders
///      were never recorded on `email_recipients`
///   4. attendee row with `team_member_id` already cached
///   5. attendee row with NULL `team_member_id` but matching an alias
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
    // Five UNION branches (see fn doc): cached recipient, alias-matched
    // recipient, alias-matched sender, cached attendee, alias-matched
    // attendee. SQL `--` comments are deliberately omitted because the
    // Rust `\`-line-continuation collapses newlines, leaving the
    // comment running to the next `\n` we never emit.
    let sql = format!(
        "SELECT DISTINCT workstream_id, member_id FROM ( \
            SELECT ws.workstream_id, er.team_member_id AS member_id \
            FROM workstream_signals ws \
            JOIN email_recipients er ON er.message_id = ws.item_id \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND er.team_member_id IS NOT NULL \
            UNION \
            SELECT ws.workstream_id, tma.member_id AS member_id \
            FROM workstream_signals ws \
            JOIN email_recipients er ON er.message_id = ws.item_id \
            JOIN team_member_aliases tma \
              ON tma.kind = 'email' AND LOWER(tma.value) = LOWER(er.email) \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND er.team_member_id IS NULL \
            UNION \
            SELECT ws.workstream_id, tma.member_id AS member_id \
            FROM workstream_signals ws \
            JOIN email_messages em ON em.id = ws.item_id \
            JOIN team_member_aliases tma \
              ON tma.kind = 'email' AND LOWER(tma.value) = LOWER(em.from_email) \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
            UNION \
            SELECT ws.workstream_id, ca.team_member_id AS member_id \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND ca.team_member_id IS NOT NULL \
            UNION \
            SELECT ws.workstream_id, tma.member_id AS member_id \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            JOIN team_member_aliases tma \
              ON tma.kind = 'email' AND LOWER(tma.value) = LOWER(ca.email) \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND ca.team_member_id IS NULL \
         ) ORDER BY workstream_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    // The IN list appears five times in the UNION; bind each.
    let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(workstreams.len() * 5);
    for _ in 0..5 {
        for w in workstreams.iter() {
            params_vec.push(&w.id as &dyn rusqlite::ToSql);
        }
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
    // For all three sources: a row is "external" only if the email
    // doesn't match a team_member by alias. The sync-time
    // `team_member_id` column on recipients/attendees is treated as a
    // fast-path hint, but typed aliases added AFTER sync (#87) are not
    // backfilled into those rows — so the alias NOT EXISTS check is
    // the source of truth. Keep the `team_member_id IS NULL` filter
    // too: it short-circuits before the correlated subquery for the
    // common case.
    let sql = format!(
        "SELECT workstream_id, email_lc, MAX(display_name) AS display_name, COUNT(*) AS cnt FROM ( \
            SELECT ws.workstream_id, LOWER(er.email) AS email_lc, er.display_name \
            FROM workstream_signals ws \
            JOIN email_recipients er ON er.message_id = ws.item_id \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND er.team_member_id IS NULL \
              AND NOT EXISTS ( \
                SELECT 1 FROM team_member_aliases tma \
                WHERE tma.kind = 'email' AND LOWER(tma.value) = LOWER(er.email) \
              ) \
            UNION ALL \
            SELECT ws.workstream_id, LOWER(em.from_email) AS email_lc, em.from_name AS display_name \
            FROM workstream_signals ws \
            JOIN email_messages em ON em.id = ws.item_id \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND NOT EXISTS ( \
                SELECT 1 FROM team_member_aliases tma \
                WHERE tma.kind = 'email' AND LOWER(tma.value) = LOWER(em.from_email) \
              ) \
            UNION ALL \
            SELECT ws.workstream_id, LOWER(ca.email) AS email_lc, ca.display_name \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) \
              AND ws.manual_detached_ms IS NULL \
              AND ca.team_member_id IS NULL \
              AND NOT EXISTS ( \
                SELECT 1 FROM team_member_aliases tma \
                WHERE tma.kind = 'email' AND LOWER(tma.value) = LOWER(ca.email) \
              ) \
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

// ----- Write helpers -------------------------------------------------------

/// Upsert a workstream + replace its pivot sets in a single
/// transaction. Returns the per-workstream contribution to the outer
/// ClusterReport.
///
/// `record.id` is the existing workstream id when the synthesizer
/// recognized this thread as a continuation; otherwise we generate a
/// fresh `ws_<uuid>`.
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

        // Synthesizer-proposed parent (#89) only applies on insert. For
        // updates we preserve user-set parent — same authority pattern
        // as `owner_member_id` and `user_notes`. Validation drops bad
        // values (would-be-grandparent, self-parent, unknown id) with a
        // log line; the new row stays standalone in those cases.
        if let Some(parent_id) = record
            .parent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "null")
        {
            match validate_proposed_parent(tx, &id, parent_id)? {
                Ok(()) => {
                    tx.execute(
                        "UPDATE workstreams SET parent_workstream_id = ?2 WHERE id = ?1",
                        params![id, parent_id],
                    )?;
                }
                Err(reason) => {
                    eprintln!(
                        "[workstreams] dropping parent_id {parent_id} for new workstream {id}: {reason}"
                    );
                }
            }
        }
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
    //
    // #129: the DELETE explicitly skips tombstoned rows
    // (`manual_detached_ms IS NOT NULL`). When Claude re-clusters a
    // user-detached item into the same workstream, INSERT OR IGNORE
    // below collides on the PK and silently leaves the tombstone in
    // place — the synth has no path to revive a manual detachment.
    tx.execute(
        "DELETE FROM workstream_signals \
          WHERE workstream_id = ?1 AND manual_detached_ms IS NULL",
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
            "SELECT MAX(modified_ms) FROM notes WHERE id IN ({placeholders})"
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

/// Insert a user-created workstream (#101). Mirrors the synthesizer
/// insert path but takes only the fields the user can author at create
/// time. Title is required; summary defaults to empty. When `parent_id`
/// is supplied, it goes through the same `validate_proposed_parent`
/// check as `set_workstream_parent` — invalid edges come back as a
/// user-facing error string so the composer can surface them inline.
/// Returns the new workstream id on success.
pub fn create_workstream(
    conn: &Connection,
    title: &str,
    summary: &str,
    parent_id: Option<&str>,
) -> rusqlite::Result<Result<String, String>> {
    let title = title.trim();
    if title.is_empty() {
        return Ok(Err("title is required".to_string()));
    }
    let id = format!("ws_{}", uuid::Uuid::new_v4());
    if let Some(p) = parent_id {
        match validate_proposed_parent(conn, &id, p)? {
            Ok(()) => {}
            Err(reason) => return Ok(Err(reason)),
        }
    }
    let now = now_ms();
    conn.execute(
        "INSERT INTO workstreams(\
            id, title, summary, status, last_activity_ms, created_ms, updated_ms, \
            parent_workstream_id\
         ) VALUES (?1, ?2, ?3, 'active', ?4, ?4, ?4, ?5)",
        params![id, title, summary, now, parent_id],
    )?;
    Ok(Ok(id))
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
/// `linked_note_id`.
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

/// Validate a proposed `(child, parent)` edge for #89's flat 2-level
/// hierarchy. Returns `Ok(())` when the edge is allowed, `Err(reason)`
/// otherwise. Cheap — at most three single-row lookups.
///
/// Rules:
///   1. `parent_id == child_id` is forbidden (self-parent).
///   2. `parent_id` must resolve to an existing workstream.
///   3. The resolved parent must itself have `parent_workstream_id IS NULL`
///      (otherwise the chain is 3 levels).
///   4. `child_id` must not already have children (would push them to a
///      grandchild slot).
pub fn validate_proposed_parent(
    conn: &Connection,
    child_id: &str,
    parent_id: &str,
) -> rusqlite::Result<Result<(), String>> {
    if parent_id == child_id {
        return Ok(Err("a workstream can't be its own parent".to_string()));
    }
    let parent_grandparent: Option<Option<String>> = conn
        .query_row(
            "SELECT parent_workstream_id FROM workstreams WHERE id = ?1",
            params![parent_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(parent_grandparent) = parent_grandparent else {
        return Ok(Err(format!("parent {parent_id} not found")));
    };
    if parent_grandparent.is_some() {
        return Ok(Err(
            "the proposed parent is itself a child — hierarchy is capped at 2 levels".to_string(),
        ));
    }
    let child_has_children: i64 = conn.query_row(
        "SELECT COUNT(*) FROM workstreams WHERE parent_workstream_id = ?1",
        params![child_id],
        |r| r.get(0),
    )?;
    if child_has_children > 0 {
        return Ok(Err(
            "this workstream already has children — unparent them before assigning a parent here"
                .to_string(),
        ));
    }
    Ok(Ok(()))
}

/// Set or clear a workstream's parent (#89). Pass `None` to make it a
/// top-level standalone. Validates via `validate_proposed_parent` when
/// `parent_id` is `Some`; rejects with a user-facing error string the
/// UI surfaces directly.
pub fn set_workstream_parent(
    conn: &Connection,
    child_id: &str,
    parent_id: Option<&str>,
) -> rusqlite::Result<Result<(), String>> {
    if let Some(p) = parent_id {
        match validate_proposed_parent(conn, child_id, p)? {
            Ok(()) => {}
            Err(e) => return Ok(Err(e)),
        }
    }
    let n = conn.execute(
        "UPDATE workstreams SET parent_workstream_id = ?2, updated_ms = ?3 WHERE id = ?1",
        params![child_id, parent_id, now_ms()],
    )?;
    if n == 0 {
        return Ok(Err(format!("workstream {child_id} not found")));
    }
    Ok(Ok(()))
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

/// Manual attach (#108). UPSERT: on a fresh attach this is the usual
/// INSERT; on a re-attach after manual detach (#129) it clears the
/// tombstone and bumps `added_ms`. Either way the row ends up
/// attached (`manual_detached_ms IS NULL`).
pub fn attach_signal(
    conn: &Connection,
    workstream_id: &str,
    kind: &str,
    item_id: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO workstream_signals \
            (workstream_id, kind, item_id, added_ms, manual_detached_ms) \
         VALUES (?1, ?2, ?3, ?4, NULL) \
         ON CONFLICT(workstream_id, kind, item_id) DO UPDATE SET \
            added_ms = excluded.added_ms, \
            manual_detached_ms = NULL",
        params![workstream_id, kind, item_id, now_ms],
    )?;
    Ok(())
}

/// Manual detach (#108 + #129). Tombstones the row rather than
/// deleting it: `manual_detached_ms = now`. Reads filter tombstoned
/// rows out (so the item moves back to Unassigned and disappears
/// from the workstream's detail), and the synth's `save_workstream`
/// preserves tombstones across its wholesale-replace pass — so the
/// next cluster cycle cannot revive the user-rejected attachment.
/// The `manual_detached_ms IS NULL` guard makes double-detach a
/// no-op and preserves the original tombstone timestamp.
pub fn detach_signal(
    conn: &Connection,
    workstream_id: &str,
    kind: &str,
    item_id: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE workstream_signals \
            SET manual_detached_ms = ?4 \
          WHERE workstream_id = ?1 AND kind = ?2 AND item_id = ?3 \
            AND manual_detached_ms IS NULL",
        params![workstream_id, kind, item_id, now_ms],
    )?;
    Ok(())
}

/// One row in the unassigned-content feed (#108). Tagged-enum serde
/// shape lands on the frontend as `{kind: "email" | …, item: {…}}`
/// — the four payload variants reuse the same hydrated structs the
/// workstream detail page consumes, so row chrome stays consistent
/// across the two surfaces.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "item", rename_all = "snake_case")]
pub enum UnassignedItem {
    Email(EmailMessage),
    Event(CalendarEvent),
    Note(NoteRef),
    TeamsMessage(TeamsMessage),
}

/// Recent entities (across email, event, note, teams_message) whose
/// id is NOT in `workstream_signals.item_id` — the "Unassigned" pill
/// on the Workstreams view (#108). One UNION ALL query picks the
/// candidates + their sort_ms; per-kind hydration through
/// `signals::registry` produces the rich rows.
pub fn list_unassigned(
    conn: &Connection,
    limit: usize,
) -> rusqlite::Result<Vec<UnassignedItem>> {
    use crate::workstreams::signals::{registry, HydratedSignal};
    use std::collections::HashMap;

    // Candidate ids in recency-desc order. The recurring-series
    // filter on `event` mirrors the embeddings rule (#109): one
    // canonical occurrence per series, not N.
    let mut stmt = conn.prepare(
        "SELECT 'email' AS kind, em.id AS item_id, em.sent_at_ms AS sort_ms \
           FROM email_messages em \
          WHERE NOT EXISTS ( \
                SELECT 1 FROM workstream_signals ws \
                 WHERE ws.kind = 'email' AND ws.item_id = em.id \
                   AND ws.manual_detached_ms IS NULL) \
         UNION ALL \
         SELECT 'event', ce.id, ce.start_ms \
           FROM calendar_events ce \
          WHERE NOT EXISTS ( \
                SELECT 1 FROM workstream_signals ws \
                 WHERE ws.kind = 'event' AND ws.item_id = ce.id \
                   AND ws.manual_detached_ms IS NULL) \
            AND ( \
                ce.series_master_id IS NULL \
             OR ce.start_ms = ( \
                    SELECT MIN(c2.start_ms) FROM calendar_events c2 \
                     WHERE c2.series_master_id = ce.series_master_id \
                ) \
           ) \
         UNION ALL \
         SELECT 'note', n.id, n.modified_ms \
           FROM notes n \
          WHERE n.archived = 0 AND NOT EXISTS ( \
                SELECT 1 FROM workstream_signals ws \
                 WHERE ws.kind = 'note' AND ws.item_id = n.id \
                   AND ws.manual_detached_ms IS NULL) \
         UNION ALL \
         SELECT 'teams_message', tm.id, tm.sent_at_ms \
           FROM teams_messages tm \
          WHERE NOT EXISTS ( \
                SELECT 1 FROM workstream_signals ws \
                 WHERE ws.kind = 'teams_message' AND ws.item_id = tm.id \
                   AND ws.manual_detached_ms IS NULL) \
         ORDER BY sort_ms DESC \
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let candidates: Vec<(String, String, i64)> = rows.filter_map(Result::ok).collect();

    // Group ids by kind; hydrate each kind once via registry.
    let mut ids_by_kind: HashMap<String, Vec<String>> = HashMap::new();
    for (kind, id, _) in &candidates {
        ids_by_kind.entry(kind.clone()).or_default().push(id.clone());
    }
    let reg = registry();
    let mut hydrated_by_kind: HashMap<String, HashMap<String, HydratedSignal>> =
        HashMap::new();
    for (kind, ids) in &ids_by_kind {
        let rows = reg.hydrate(conn, kind, ids)?;
        let mut by_id: HashMap<String, HydratedSignal> = HashMap::new();
        for row in rows {
            let id = match &row {
                HydratedSignal::Email(m) => m.id.clone(),
                HydratedSignal::Event(e) => e.id.clone(),
                HydratedSignal::Note(n) => n.note_path.clone(),
                HydratedSignal::TeamsMessage(t) => t.id.clone(),
            };
            by_id.insert(id, row);
        }
        hydrated_by_kind.insert(kind.clone(), by_id);
    }

    // Reassemble in the original sort_ms-desc order. Missing rows
    // (hydrate dropped them silently because the upstream entity
    // vanished between query + hydrate) skip gracefully.
    let mut out: Vec<UnassignedItem> = Vec::with_capacity(candidates.len());
    for (kind, id, _) in candidates {
        let Some(by_id) = hydrated_by_kind.get_mut(&kind) else {
            continue;
        };
        let Some(row) = by_id.remove(&id) else {
            continue;
        };
        let item = match row {
            HydratedSignal::Email(m) => UnassignedItem::Email(m),
            HydratedSignal::Event(e) => UnassignedItem::Event(e),
            HydratedSignal::Note(n) => UnassignedItem::Note(n),
            HydratedSignal::TeamsMessage(t) => UnassignedItem::TeamsMessage(t),
        };
        out.push(item);
    }
    Ok(out)
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
                 id           TEXT PRIMARY KEY,
                 display_name TEXT NOT NULL DEFAULT '',
                 aliases      TEXT NOT NULL DEFAULT '[]',
                 is_self      INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE connectors (id TEXT PRIMARY KEY);
             INSERT INTO connectors(id) VALUES ('mg:test');
             CREATE TABLE notes (
                 note_path    TEXT PRIMARY KEY,
                 bundle_id    TEXT NOT NULL DEFAULT '',
                 title        TEXT NOT NULL,
                 modified_ms  INTEGER NOT NULL,
                 duration_ms  INTEGER,
                 preview      TEXT NOT NULL DEFAULT '',
                 body_size    INTEGER NOT NULL DEFAULT 0,
                 archived     INTEGER NOT NULL DEFAULT 0,
                 favorite     INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE tags (
                 note_path TEXT NOT NULL REFERENCES notes(note_path) ON DELETE CASCADE,
                 tag       TEXT NOT NULL,
                 PRIMARY KEY (note_path, tag)
             );
             CREATE TABLE meeting_attendees (
                 note_path     TEXT NOT NULL REFERENCES notes(note_path) ON DELETE CASCADE,
                 member_id     TEXT NOT NULL REFERENCES team_members(id) ON DELETE CASCADE,
                 speaker_index INTEGER,
                 PRIMARY KEY (note_path, member_id)
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
        conn.execute_batch(include_str!("../migrations/019_workstream_parent.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/020_workstream_link_summary.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/021_workstream_action_assignee.sql"))
            .unwrap();
        // The 022 backfill references the `actions` table (note-backed
        // todos). The persist test fixture skips the notes-side ladder
        // and lacks that table — seed a minimal stub before running 022
        // so its `SELECT FROM actions` produces zero rows instead of
        // erroring.
        conn.execute_batch(
            "CREATE TABLE actions (
                 id                TEXT PRIMARY KEY,
                 note_path         TEXT NOT NULL,
                 line              INTEGER NOT NULL,
                 text              TEXT NOT NULL,
                 done              INTEGER NOT NULL DEFAULT 0,
                 created_ms        INTEGER NOT NULL,
                 due_ms            INTEGER,
                 reminder_sent_ms  INTEGER,
                 assignee_id       TEXT,
                 subject_member_id TEXT,
                 manual_override   INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
        // #106 events table — needed by live emission in write_workstream.
        // Backfill block produces zero rows against this fresh fixture
        // (no source data); safe.
        conn.execute_batch(include_str!("../migrations/022_events_edges.sql"))
            .unwrap();
        // 023 (embeddings) references the sqlite-vec extension which
        // can't load in this fixture. Stub the embeddings table so the
        // v26 UPDATE embeddings WHERE ref_kind = 'note' clause doesn't
        // fail with "no such table".
        conn.execute_batch(
            "CREATE TABLE embeddings (
                rowid       INTEGER PRIMARY KEY AUTOINCREMENT,
                ref_kind    TEXT NOT NULL,
                ref_id      TEXT NOT NULL,
                model       TEXT NOT NULL,
                source_hash TEXT NOT NULL,
                indexed_ms  INTEGER NOT NULL,
                UNIQUE (ref_kind, ref_id, model)
            );",
        )
        .unwrap();
        // 024 (teams_messages) — needed by list_unassigned (#108)
        // which UNION ALLs across the four kinds.
        conn.execute_batch(include_str!("../migrations/024_teams.sql"))
            .unwrap();
        // 025 (#111) collapses workstream_actions into the unified
        // actions table. Without it, the persist write path's
        // INSERT INTO actions blows up.
        conn.execute_batch(include_str!("../migrations/025_unify_actions.sql"))
            .unwrap();
        // The notes_fts virtual table is created in 001 normally; we
        // bypass 001 here, so seed a minimal one matching the v26
        // shape so migration 026's DROP/CREATE cycle works.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE notes_fts USING fts5(\
                note_path UNINDEXED, title, body, \
                tokenize = 'porter unicode61'\
             );",
        )
        .unwrap();
        // 026 (#112) moves notes into the DB. Renames column FKs from
        // note_path → note_id; required for persist tests that exercise
        // get_workstream_detail's JOIN onto notes.
        conn.execute_batch(include_str!("../migrations/026_notes_to_db.sql"))
            .unwrap();
        // 030 adds `subject_member_id` and `manual_override` columns
        // to `actions`, plus the `dismissed_action_sources` table —
        // required by the waiting-action upsert path (#120 follow-up).
        conn.execute_batch(include_str!("../migrations/030_action_waiting.sql"))
            .unwrap();
        // 031 adds the auto-resolve hysteresis columns (#124).
        conn.execute_batch(include_str!("../migrations/031_auto_resolve_hysteresis.sql"))
            .unwrap();
        // 033 adds calendar_events.series_master_id (#109).
        conn.execute_batch(include_str!(
            "../migrations/033_calendar_series_master_id.sql"
        ))
        .unwrap();
        // 034 adds workstream_signals.manual_detached_ms (#129).
        conn.execute_batch(include_str!(
            "../migrations/034_workstream_signal_tombstone.sql"
        ))
        .unwrap();
        // 041 adds the universal action_deletions log (#147). Required
        // by delete_action and dismiss_waiting_action, which both write
        // a snapshot row before deleting from `actions`.
        conn.execute_batch(include_str!("../migrations/041_action_deletions.sql"))
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
        // After #112 the `note_path` parameter holds a note id.
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms) VALUES (?1, ?1, ?2, ?3)",
            params![path, "Note", modified],
        )
        .unwrap();
    }

    fn make_ws(id: Option<&str>, title: &str, emails: &[&str], events: &[&str], notes: &[&str]) -> SynthesizedWorkstream {
        SynthesizedWorkstream {
            id: id.map(|s| s.to_string()),
            title: title.to_string(),
            summary: format!("Summary of {title}"),
            member_emails: emails.iter().map(|s| s.to_string()).collect(),
            member_events: events.iter().map(|s| s.to_string()).collect(),
            member_notes: notes.iter().map(|s| s.to_string()).collect(),
            status: None,
            parent_id: None,
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
        );
        let counts = write_workstream(&tx, &ws, 5_000).unwrap();
        tx.commit().unwrap();

        assert!(counts.workstream_added);

        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Hyundai POC");
        assert_eq!(active[0].email_count, 1);
        assert_eq!(active[0].event_count, 1);
        assert_eq!(active[0].note_count, 1);
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
    fn create_workstream_inserts_active_row() {
        let conn = open_test_db();
        let id = create_workstream(&conn, "  Q1 hiring  ", "scope: backend", None)
            .unwrap()
            .unwrap();
        assert!(id.starts_with("ws_"));

        let active = list_workstreams_active(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, id);
        // Title gets trimmed; summary preserved verbatim.
        assert_eq!(active[0].title, "Q1 hiring");
        assert_eq!(active[0].summary, "scope: backend");
        assert!(active[0].parent_workstream_id.is_none());
    }

    #[test]
    fn create_workstream_rejects_empty_title() {
        let conn = open_test_db();
        let r = create_workstream(&conn, "   ", "", None).unwrap();
        assert!(r.is_err(), "empty title must be rejected");
    }

    #[test]
    fn create_workstream_with_valid_parent() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_umbrella"), "Umbrella", &[], &[], &[]),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let child = create_workstream(&conn, "Sub", "", Some("ws_umbrella"))
            .unwrap()
            .unwrap();
        let active = list_workstreams_active(&conn).unwrap();
        let child_row = active.iter().find(|w| w.id == child).unwrap();
        assert_eq!(child_row.parent_workstream_id.as_deref(), Some("ws_umbrella"));
    }

    #[test]
    fn create_workstream_rejects_unknown_parent() {
        let conn = open_test_db();
        let r = create_workstream(&conn, "Sub", "", Some("ws_ghost")).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn create_workstream_rejects_grandparent_chain() {
        // A 2-level cap: child parented to a parent that already has a
        // parent must fail.
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_g"), "Grand", &[], &[], &[]),
            1_000,
        )
        .unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_p"), "Parent", &[], &[], &[]),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();
        // Make ws_p a child of ws_g.
        set_workstream_parent(&conn, "ws_p", Some("ws_g")).unwrap().unwrap();

        // Now try to create a workstream under ws_p — should fail
        // because ws_p is itself a child.
        let r = create_workstream(&conn, "Leaf", "", Some("ws_p")).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn list_workstreams_active_excludes_archived() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_a"), "Active", &[], &[], &[]),
            1_000,
        )
        .unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_b"), "Archived", &[], &[], &[]),
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
    fn get_workstream_detail_returns_joined_emails_events_notes() {
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
    }

    // ----- user_notes (#77) ------------------------------------------------

    #[test]
    fn set_user_notes_round_trips() {
        let mut conn = open_test_db();
        let tx = conn.transaction().unwrap();
        write_workstream(&tx, &make_ws(Some("ws_n"), "WS", &[], &[], &[]), 1_000)
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
        write_workstream(&tx, &make_ws(Some("ws_c"), "WS", &[], &[], &[]), 1_000)
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
        write_workstream(&tx, &make_ws(Some("ws_r"), "Hyundai", &[], &[], &[]), 1_000)
            .unwrap();
        tx.commit().unwrap();
        set_user_notes(&conn, "ws_r", Some("This is the new POC.")).unwrap();

        // Re-sync the same workstream (fresh title, no carry-over of
        // user_notes from the synthesizer side — the SynthesizedWorkstream
        // shape doesn't have user_notes at all).
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_r"), "Hyundai (renamed)", &[], &[], &[]),
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
        write_workstream(&tx, &make_ws(Some(id), "WS", &[], &[], &[]), 1_000)
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
            &make_ws(Some("ws_oo"), "Renamed", &[], &[], &[]),
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
            &make_ws(Some("ws_q"), "WS", &["mg:test::m1"], &[], &[]),
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
    fn add_workstream_link_accepts_each_canonical_kind() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        for kind in [
            super::super::link_kinds::GITHUB,
            super::super::link_kinds::LINEAR,
            super::super::link_kinds::NOTION,
            super::super::link_kinds::FIGMA,
            super::super::link_kinds::OTHER,
        ] {
            let row = add_workstream_link(
                &conn,
                "ws_l",
                "Label",
                &format!("https://example.com/{kind}"),
                Some(kind),
                100,
            )
            .expect("canonical kind accepted");
            assert_eq!(row.kind.as_deref(), Some(kind));
        }
    }

    #[test]
    fn add_workstream_link_rejects_unknown_kind() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        let res = add_workstream_link(
            &conn,
            "ws_l",
            "Label",
            "https://x",
            Some("slack"),
            100,
        );
        assert!(res.is_err());
        let listed = list_workstream_links(&conn, "ws_l").unwrap();
        assert!(listed.is_empty(), "no row inserted for invalid kind");
    }

    #[test]
    fn set_workstream_link_summary_round_trips() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_l");
        let added =
            add_workstream_link(&conn, "ws_l", "Repo", "https://x", Some("github"), 100)
                .unwrap();
        assert!(added.summary.is_none());

        let updated =
            set_workstream_link_summary(&conn, &added.id, Some("It's a repo.")).unwrap();
        assert!(updated);
        let listed = list_workstream_links(&conn, "ws_l").unwrap();
        assert_eq!(listed[0].summary.as_deref(), Some("It's a repo."));
    }

    #[test]
    fn set_workstream_link_summary_no_op_when_link_missing() {
        let conn = open_test_db();
        let updated =
            set_workstream_link_summary(&conn, "wsl_nonexistent", Some("anything")).unwrap();
        assert!(!updated, "no row touched, no error");
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

    /// Regression: `email_recipients.team_member_id` is set at sync time,
    /// so when the user adds an email alias AFTER the email was synced,
    /// old recipient rows still have NULL team_member_id. `attach_members`
    /// must also resolve via the alias table; otherwise the team_member
    /// drops out of the workstream's member list (and the user sees no
    /// chip even though they're aliased).
    #[test]
    fn members_resolves_recipients_via_alias_when_team_member_id_is_stale() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_team_member(&conn, "tm_tj");
        // Recipient row has NULL team_member_id (sync was before the
        // user added their alias) but the alias now resolves it.
        add_recipient(&conn, "mg:test::m1", "tj@example.com", Some("TJ"), None);
        seed_email_alias(&conn, "tm_tj", "tj@example.com");

        seed_workstream_with_email_signal(&mut conn, "ws_x", "mg:test::m1");

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        assert!(
            row.members.iter().any(|m| m == "tm_tj"),
            "alias-resolved team member surfaces despite stale team_member_id; got members={:?}",
            row.members
        );
    }

    /// Mirror: senders never had a `team_member_id` column in the first
    /// place. They should resolve via the alias table.
    #[test]
    fn members_resolves_email_senders_via_alias() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        set_email_sender(&conn, "mg:test::m1", "TJ@Example.com");
        seed_team_member(&conn, "tm_tj");
        seed_email_alias(&conn, "tm_tj", "tj@example.com");

        seed_workstream_with_email_signal(&mut conn, "ws_x", "mg:test::m1");

        let listed = list_workstreams_active(&conn).unwrap();
        let row = listed.iter().find(|w| w.id == "ws_x").unwrap();
        assert!(
            row.members.iter().any(|m| m == "tm_tj"),
            "sender resolved via alias; got members={:?}",
            row.members
        );
    }

    /// Regression: `email_recipients.team_member_id` is set at sync time,
    /// so when the user adds a typed-alias email to a team member AFTER
    /// the email was synced, old recipient rows still have NULL
    /// team_member_id. The externals query must check the aliases too,
    /// not just the cached column.
    #[test]
    fn external_participants_excludes_recipients_resolved_by_alias_after_sync() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        set_email_sender(&conn, "mg:test::m1", "tom@x.io");
        seed_team_member(&conn, "tm_tom");
        seed_email_alias(&conn, "tm_tom", "tom@x.io");
        // Heike was on the recipient list when the email was synced,
        // but her team_member_id wasn't set on the recipient row.
        // Later the user added her email as an alias on tm_heike.
        seed_team_member(&conn, "tm_heike");
        add_recipient(
            &conn,
            "mg:test::m1",
            "heike@example.com",
            Some("Heike"),
            None, // team_member_id NULL — sync-time mapping missed her
        );
        seed_email_alias(&conn, "tm_heike", "heike@example.com");
        // And one genuinely external recipient as a control.
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
            "Heike is now a team member by alias even though the recipient row's team_member_id is stale"
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

    // ----- Hierarchy (#89) -------------------------------------------------

    #[test]
    fn set_workstream_parent_happy_path_then_clear() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_parent");
        seed_workstream(&mut conn, "ws_child");

        let r = set_workstream_parent(&conn, "ws_child", Some("ws_parent")).unwrap();
        assert!(r.is_ok(), "happy path: {:?}", r);

        let detail = get_workstream_detail(&conn, "ws_child").unwrap().unwrap();
        assert_eq!(detail.workstream.parent_workstream_id.as_deref(), Some("ws_parent"));
        let parent = get_workstream_detail(&conn, "ws_parent").unwrap().unwrap();
        assert_eq!(parent.children.len(), 1);
        assert_eq!(parent.children[0].id, "ws_child");

        // Clear → standalone again.
        let r = set_workstream_parent(&conn, "ws_child", None).unwrap();
        assert!(r.is_ok());
        let detail = get_workstream_detail(&conn, "ws_child").unwrap().unwrap();
        assert!(detail.workstream.parent_workstream_id.is_none());
    }

    #[test]
    fn set_workstream_parent_rejects_self_parent() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_self");
        let r = set_workstream_parent(&conn, "ws_self", Some("ws_self")).unwrap();
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("its own parent"));
    }

    #[test]
    fn set_workstream_parent_rejects_grandparent_chain() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_a");
        seed_workstream(&mut conn, "ws_b");
        seed_workstream(&mut conn, "ws_c");
        // A has parent B (so A is mid-level). Now trying to point C
        // at A would make a 3-level chain.
        set_workstream_parent(&conn, "ws_a", Some("ws_b"))
            .unwrap()
            .unwrap();
        let r = set_workstream_parent(&conn, "ws_c", Some("ws_a")).unwrap();
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("2 levels"));
    }

    #[test]
    fn set_workstream_parent_rejects_when_self_has_children() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_p");
        seed_workstream(&mut conn, "ws_c");
        seed_workstream(&mut conn, "ws_x");
        // ws_p already has a child (ws_c). Trying to set ws_p's parent
        // would push ws_c to a third level.
        set_workstream_parent(&conn, "ws_c", Some("ws_p"))
            .unwrap()
            .unwrap();
        let r = set_workstream_parent(&conn, "ws_p", Some("ws_x")).unwrap();
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("already has children"));
    }

    #[test]
    fn set_workstream_parent_rejects_unknown_parent_id() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_a");
        let r = set_workstream_parent(&conn, "ws_a", Some("ws_missing")).unwrap();
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("not found"));
    }

    #[test]
    fn get_workstream_detail_returns_empty_children_for_leaves_and_standalones() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_solo");
        let d = get_workstream_detail(&conn, "ws_solo").unwrap().unwrap();
        assert!(d.children.is_empty());
    }

    #[test]
    fn get_workstream_detail_orders_children_by_recency() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_p");
        seed_workstream(&mut conn, "ws_old");
        seed_workstream(&mut conn, "ws_new");
        // Stamp last_activity directly so the ordering is deterministic.
        conn.execute(
            "UPDATE workstreams SET last_activity_ms = 1000 WHERE id = 'ws_old'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE workstreams SET last_activity_ms = 2000 WHERE id = 'ws_new'",
            [],
        )
        .unwrap();
        set_workstream_parent(&conn, "ws_old", Some("ws_p"))
            .unwrap()
            .unwrap();
        set_workstream_parent(&conn, "ws_new", Some("ws_p"))
            .unwrap()
            .unwrap();
        let d = get_workstream_detail(&conn, "ws_p").unwrap().unwrap();
        let ids: Vec<&str> = d.children.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["ws_new", "ws_old"]);
    }

    #[test]
    fn delete_workstream_sets_children_parent_to_null() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_p");
        seed_workstream(&mut conn, "ws_c");
        set_workstream_parent(&conn, "ws_c", Some("ws_p"))
            .unwrap()
            .unwrap();

        conn.execute("DELETE FROM workstreams WHERE id = 'ws_p'", [])
            .unwrap();

        let d = get_workstream_detail(&conn, "ws_c").unwrap().unwrap();
        assert!(
            d.workstream.parent_workstream_id.is_none(),
            "FK ON DELETE SET NULL keeps the child but clears its parent"
        );
    }

    #[test]
    fn write_workstream_drops_invalid_parent_id_for_new_workstream() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_a");
        seed_workstream(&mut conn, "ws_b");
        // Make ws_a a child first; using it as a parent on a new write
        // should be rejected (would-be-grandparent).
        set_workstream_parent(&conn, "ws_a", Some("ws_b"))
            .unwrap()
            .unwrap();

        let tx = conn.transaction().unwrap();
        let mut record = make_ws(Some("ws_new"), "Spawn", &[], &[], &[]);
        record.parent_id = Some("ws_a".to_string());
        write_workstream(&tx, &record, 9_000).unwrap();
        tx.commit().unwrap();

        let d = get_workstream_detail(&conn, "ws_new").unwrap().unwrap();
        assert!(
            d.workstream.parent_workstream_id.is_none(),
            "invalid parent_id (would-be-grandparent) silently dropped"
        );
    }

    #[test]
    fn write_workstream_does_not_clobber_user_set_parent_on_resync() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_p");
        seed_workstream(&mut conn, "ws_c");
        set_workstream_parent(&conn, "ws_c", Some("ws_p"))
            .unwrap()
            .unwrap();

        // Resync the existing workstream; even with no parent_id in the
        // record, the user's parent assignment must survive.
        let tx = conn.transaction().unwrap();
        let record = make_ws(Some("ws_c"), "Renamed child", &[], &[], &[]);
        write_workstream(&tx, &record, 9_000).unwrap();
        tx.commit().unwrap();

        let d = get_workstream_detail(&conn, "ws_c").unwrap().unwrap();
        assert_eq!(d.workstream.parent_workstream_id.as_deref(), Some("ws_p"));
    }

    #[test]
    fn list_workstreams_active_populates_parent_workstream_id() {
        let mut conn = open_test_db();
        seed_workstream(&mut conn, "ws_p");
        seed_workstream(&mut conn, "ws_c");
        set_workstream_parent(&conn, "ws_c", Some("ws_p"))
            .unwrap()
            .unwrap();

        let listed = list_workstreams_active(&conn).unwrap();
        let parent = listed.iter().find(|w| w.id == "ws_p").unwrap();
        let child = listed.iter().find(|w| w.id == "ws_c").unwrap();
        assert!(parent.parent_workstream_id.is_none());
        assert_eq!(child.parent_workstream_id.as_deref(), Some("ws_p"));
    }


    // ---------- Manual attach / detach + unassigned feed (#108) ----------

    fn seed_teams_msg(conn: &Connection, id: &str, sent_at: i64) {
        conn.execute(
            "INSERT INTO teams_messages(\
                id, connector_id, external_id, chat_id, chat_kind, \
                sent_at_ms, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 'chat-1', 'oneOnOne', ?2, ?2)",
            params![id, sent_at],
        )
        .unwrap();
    }

    fn seed_ws_active(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO workstreams(id, title, summary, status, \
                                       last_activity_ms, created_ms, updated_ms) \
             VALUES (?1, 'WS', '', 'active', 0, 0, 0)",
            params![id],
        )
        .unwrap();
    }

    fn count_signals(conn: &Connection, ws: &str, kind: &str, item: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM workstream_signals \
              WHERE workstream_id = ?1 AND kind = ?2 AND item_id = ?3",
            params![ws, kind, item],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// `attach_signal` inserts a `workstream_signals` row.
    #[test]
    fn attach_signal_inserts_pivot_row() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        assert_eq!(count_signals(&conn, "ws1", "email", "em:1"), 1);
    }

    /// Re-attaching the same (workstream, kind, item) yields one row
    /// thanks to the UPSERT (#129: was INSERT OR IGNORE, now ON
    /// CONFLICT DO UPDATE to clear any tombstone).
    #[test]
    fn attach_signal_is_idempotent() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        attach_signal(&conn, "ws1", "email", "em:1", 6_000).unwrap();
        assert_eq!(count_signals(&conn, "ws1", "email", "em:1"), 1);
    }

    /// Direct read of `manual_detached_ms` for a (ws, kind, item).
    /// Returns `Some(None)` when the row exists and is attached,
    /// `Some(Some(ts))` when tombstoned, `None` when no row.
    fn tombstone_ms(
        conn: &Connection,
        ws: &str,
        kind: &str,
        item: &str,
    ) -> Option<Option<i64>> {
        conn.query_row(
            "SELECT manual_detached_ms FROM workstream_signals \
              WHERE workstream_id = ?1 AND kind = ?2 AND item_id = ?3",
            params![ws, kind, item],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .unwrap()
    }

    fn added_ms_of(
        conn: &Connection,
        ws: &str,
        kind: &str,
        item: &str,
    ) -> i64 {
        conn.query_row(
            "SELECT added_ms FROM workstream_signals \
              WHERE workstream_id = ?1 AND kind = ?2 AND item_id = ?3",
            params![ws, kind, item],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Detach tombstones the row (UPDATE, not DELETE). Other rows
    /// for the same workstream are untouched (#129).
    #[test]
    fn detach_signal_stamps_manual_detached_ms() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        seed_email(&conn, "em:2", 2_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        attach_signal(&conn, "ws1", "email", "em:2", 5_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:1", 9_000).unwrap();
        // Row still present, but tombstoned.
        assert_eq!(count_signals(&conn, "ws1", "email", "em:1"), 1);
        assert_eq!(tombstone_ms(&conn, "ws1", "email", "em:1"), Some(Some(9_000)));
        // Sibling row untouched.
        assert_eq!(tombstone_ms(&conn, "ws1", "email", "em:2"), Some(None));
    }

    /// The original `added_ms` survives a detach (only
    /// `manual_detached_ms` is set). Important for any future
    /// "when was this first attached" UX (#129).
    #[test]
    fn detach_signal_preserves_added_ms() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:1", 9_000).unwrap();
        assert_eq!(added_ms_of(&conn, "ws1", "email", "em:1"), 5_000);
    }

    /// A second detach on an already-tombstoned row leaves the
    /// original `manual_detached_ms` in place — the IS NULL guard
    /// in the UPDATE WHERE preserves the first-detach timestamp
    /// (#129).
    #[test]
    fn detach_signal_idempotent_doesnt_clobber_ms() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:1", 9_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:1", 12_000).unwrap();
        assert_eq!(tombstone_ms(&conn, "ws1", "email", "em:1"), Some(Some(9_000)));
    }

    /// Detaching a non-existent pivot row is a no-op, never an
    /// error. UPDATE matches zero rows; no INSERT happens.
    #[test]
    fn detach_signal_noop_when_absent() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        detach_signal(&conn, "ws1", "email", "em:missing", 9_000).unwrap();
        assert_eq!(count_signals(&conn, "ws1", "email", "em:missing"), 0);
    }

    /// Manual re-attach via the picker clears the tombstone and
    /// bumps `added_ms` (#129). After this the synth is free to
    /// keep the item attached.
    #[test]
    fn attach_signal_clears_tombstone_on_reattach() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:1", 9_000).unwrap();
        attach_signal(&conn, "ws1", "email", "em:1", 12_000).unwrap();
        assert_eq!(tombstone_ms(&conn, "ws1", "email", "em:1"), Some(None));
        assert_eq!(added_ms_of(&conn, "ws1", "email", "em:1"), 12_000);
    }

    /// Items already attached to any workstream are excluded from
    /// the unassigned feed.
    #[test]
    fn list_unassigned_excludes_items_in_workstream_signals() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:attached", 5_000);
        seed_email(&conn, "em:floating", 3_000);
        attach_signal(&conn, "ws1", "email", "em:attached", 5_000).unwrap();

        let items = list_unassigned(&conn, 100).unwrap();
        let ids: Vec<String> = items
            .iter()
            .filter_map(|u| match u {
                UnassignedItem::Email(m) => Some(m.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["em:floating"]);
    }

    /// Output is sorted by recency (sort_ms) DESC across kinds.
    #[test]
    fn list_unassigned_orders_by_recency_desc() {
        let conn = open_test_db();
        seed_email(&conn, "em:old", 1_000);
        seed_event(&conn, "ev:newest", 9_000);
        seed_note(&conn, "n:mid", 5_000);
        seed_teams_msg(&conn, "tm:second", 7_000);

        let items = list_unassigned(&conn, 100).unwrap();
        let order: Vec<&str> = items
            .iter()
            .map(|u| match u {
                UnassignedItem::Email(m) => m.id.as_str(),
                UnassignedItem::Event(e) => e.id.as_str(),
                UnassignedItem::Note(n) => n.note_path.as_str(),
                UnassignedItem::TeamsMessage(t) => t.id.as_str(),
            })
            .collect();
        assert_eq!(order, vec!["ev:newest", "tm:second", "n:mid", "em:old"]);
    }

    /// Archived notes don't appear in the feed.
    #[test]
    fn list_unassigned_skips_archived_notes() {
        let conn = open_test_db();
        seed_note(&conn, "n:live", 1_000);
        seed_note(&conn, "n:gone", 2_000);
        conn.execute(
            "UPDATE notes SET archived = 1 WHERE id = 'n:gone'",
            [],
        )
        .unwrap();
        let items = list_unassigned(&conn, 100).unwrap();
        let notes: Vec<String> = items
            .iter()
            .filter_map(|u| match u {
                UnassignedItem::Note(n) => Some(n.note_path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(notes, vec!["n:live"]);
    }

    /// Headline #129 behavior: a detached item moves *back* into the
    /// Unassigned feed even though its pivot row still exists
    /// (now tombstoned). The NOT EXISTS subqueries filter on
    /// `manual_detached_ms IS NULL`.
    #[test]
    fn list_unassigned_includes_detached_item() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 5_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        // Pre-detach: email is attached, not in Unassigned.
        let pre: Vec<String> = list_unassigned(&conn, 100)
            .unwrap()
            .iter()
            .filter_map(|u| match u {
                UnassignedItem::Email(m) => Some(m.id.clone()),
                _ => None,
            })
            .collect();
        assert!(pre.is_empty());

        detach_signal(&conn, "ws1", "email", "em:1", 9_000).unwrap();

        // Post-detach: email surfaces in Unassigned. Pivot row
        // still exists, just tombstoned.
        let post: Vec<String> = list_unassigned(&conn, 100)
            .unwrap()
            .iter()
            .filter_map(|u| match u {
                UnassignedItem::Email(m) => Some(m.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(post, vec!["em:1"]);
        assert_eq!(count_signals(&conn, "ws1", "email", "em:1"), 1);
    }

    /// `get_workstream_detail` hydrates emails/events/notes/teams via
    /// `signals::load_and_hydrate_for_workstream`, which now skips
    /// tombstoned rows. A detached email must not appear in the
    /// workstream's detail view (#129).
    #[test]
    fn get_workstream_detail_excludes_detached_items() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:keep", 1_000);
        seed_email(&conn, "em:drop", 2_000);
        attach_signal(&conn, "ws1", "email", "em:keep", 5_000).unwrap();
        attach_signal(&conn, "ws1", "email", "em:drop", 5_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:drop", 9_000).unwrap();
        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        let ids: Vec<&str> = detail.emails.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["em:keep"]);
    }

    /// Per-kind COUNT in the sidebar/list view filters tombstoned
    /// rows out — three attaches, one detach, count is two (#129).
    #[test]
    fn list_workstreams_active_counts_skip_detached() {
        let conn = open_test_db();
        seed_ws_active(&conn, "ws1");
        seed_email(&conn, "em:1", 1_000);
        seed_email(&conn, "em:2", 2_000);
        seed_email(&conn, "em:3", 3_000);
        attach_signal(&conn, "ws1", "email", "em:1", 5_000).unwrap();
        attach_signal(&conn, "ws1", "email", "em:2", 5_000).unwrap();
        attach_signal(&conn, "ws1", "email", "em:3", 5_000).unwrap();
        detach_signal(&conn, "ws1", "email", "em:2", 9_000).unwrap();
        let active = list_workstreams_active(&conn).unwrap();
        let ws = active.iter().find(|w| w.id == "ws1").unwrap();
        assert_eq!(ws.email_count, 2);
    }

    /// Core regression test for #129: when the synth re-clusters the
    /// same item back into the workstream after a manual detach, the
    /// tombstone survives the wholesale-replace pass in
    /// `write_workstream` and the item stays out of the detail view.
    #[test]
    fn write_workstream_preserves_tombstone_across_resynth() {
        let mut conn = open_test_db();
        seed_email(&conn, "em:1", 1_000);

        // Initial synth attach via write_workstream (the real path).
        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            Some("ws_resynth"),
            "Q3 planning",
            &["em:1"],
            &[],
            &[],
        );
        write_workstream(&tx, &ws, 5_000).unwrap();
        tx.commit().unwrap();
        assert_eq!(tombstone_ms(&conn, "ws_resynth", "email", "em:1"), Some(None));

        // User detaches.
        detach_signal(&conn, "ws_resynth", "email", "em:1", 9_000).unwrap();

        // Synth re-clusters with the SAME item — exactly the bug
        // #129 fixes. With the pre-fix wholesale DELETE this would
        // wipe the tombstone; post-fix the DELETE skips tombstoned
        // rows and INSERT OR IGNORE leaves the row alone.
        let tx = conn.transaction().unwrap();
        let ws_again = make_ws(
            Some("ws_resynth"),
            "Q3 planning",
            &["em:1"],
            &[],
            &[],
        );
        write_workstream(&tx, &ws_again, 15_000).unwrap();
        tx.commit().unwrap();

        // Tombstone intact, detail view excludes the item.
        assert_eq!(
            tombstone_ms(&conn, "ws_resynth", "email", "em:1"),
            Some(Some(9_000))
        );
        let detail = get_workstream_detail(&conn, "ws_resynth").unwrap().unwrap();
        assert!(detail.emails.is_empty());
    }

    /// `write_workstream`'s tombstone-aware DELETE still replaces
    /// non-tombstoned rows: re-syncing with a fresh membership set
    /// drops the old attached row and adds the new one, while a
    /// tombstoned (unrelated-but-same-workstream) row survives.
    #[test]
    fn write_workstream_replaces_non_tombstoned_rows_only() {
        let mut conn = open_test_db();
        seed_email(&conn, "em:tombstoned", 1_000);
        seed_email(&conn, "em:old", 2_000);
        seed_email(&conn, "em:fresh", 3_000);

        // First synth pass attaches em:tombstoned and em:old.
        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            Some("ws_x"),
            "X",
            &["em:tombstoned", "em:old"],
            &[],
            &[],
        );
        write_workstream(&tx, &ws, 5_000).unwrap();
        tx.commit().unwrap();

        // User detaches the first one — tombstone laid down.
        detach_signal(&conn, "ws_x", "email", "em:tombstoned", 6_000).unwrap();

        // Next synth pass: cluster only em:fresh (Claude moved on).
        let tx = conn.transaction().unwrap();
        let ws2 = make_ws(Some("ws_x"), "X", &["em:fresh"], &[], &[]);
        write_workstream(&tx, &ws2, 15_000).unwrap();
        tx.commit().unwrap();

        // Tombstoned row survives, untouched.
        assert_eq!(
            tombstone_ms(&conn, "ws_x", "email", "em:tombstoned"),
            Some(Some(6_000))
        );
        // Old non-tombstoned row was DELETE'd.
        assert_eq!(count_signals(&conn, "ws_x", "email", "em:old"), 0);
        // Fresh row attached.
        assert_eq!(tombstone_ms(&conn, "ws_x", "email", "em:fresh"), Some(None));
    }

    /// Recurring occurrences collapse to the earliest in the feed
    /// (#109 rule, mirrored from the embeddings worker).
    #[test]
    fn list_unassigned_skips_recurring_occurrences() {
        let conn = open_test_db();
        conn.execute(
            "INSERT INTO calendar_events(\
                id, connector_id, external_id, title, start_ms, end_ms, \
                all_day, modified_ms, series_master_id\
             ) VALUES \
             ('occ1', 'mg:test', 'occ1', 'Standup', 1_000, 1_000, 0, 1_000, 'mg:test::m1'), \
             ('occ2', 'mg:test', 'occ2', 'Standup', 2_000, 2_000, 0, 2_000, 'mg:test::m1'), \
             ('occ3', 'mg:test', 'occ3', 'Standup', 3_000, 3_000, 0, 3_000, 'mg:test::m1')",
            [],
        )
        .unwrap();
        let items = list_unassigned(&conn, 100).unwrap();
        let events: Vec<String> = items
            .iter()
            .filter_map(|u| match u {
                UnassignedItem::Event(e) => Some(e.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(events, vec!["occ1"]);
    }
}
