//! Storage layer for workstreams + their pivots and actions.
//!
//! Mirrors the per-domain pattern from `connectors/calendar.rs` /
//! `connectors/email.rs`: small, transparent functions that take a
//! `Connection` (or a `Transaction` on the write side), no hidden
//! state, no caching. The synthesizer composes these into the
//! end-to-end cluster pass.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::{
    ExternalParticipant, NoteRef, Workstream, WorkstreamDetail, WorkstreamLink,
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
    /// Optional parent workstream id from Claude (#89). When set, the
    /// synthesizer's write path validates against the 2-level cap +
    /// self-parent / unknown-id rules; invalid values are silently
    /// dropped to NULL with a log line.
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SynthesizedAction {
    pub text: String,
    pub due_ms: Option<i64>,
    pub source_kind: String,
    pub source_id: String,
    /// Resolved team_members.id when the synthesizer picked an
    /// `owner_label` (#100). None means unowned/user-owned.
    pub assignee_id: Option<String>,
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
                COALESCE((SELECT COUNT(*) FROM actions WHERE workstream_id = w.id AND done = 0), 0) AS ac, \
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
            open_action_count: r.get::<_, i64>(14)? as u32,
            link_count: r.get::<_, i64>(15)? as u32,
            parent_workstream_id: r.get(16)?,
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
                COALESCE((SELECT COUNT(*) FROM actions WHERE workstream_id = w.id AND done = 0), 0), \
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
            open_action_count: r.get::<_, i64>(14)? as u32,
            link_count: r.get::<_, i64>(15)? as u32,
            parent_workstream_id: r.get(16)?,
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
                COALESCE((SELECT COUNT(*) FROM actions WHERE workstream_id = w.id AND done = 0), 0), \
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
                open_action_count: r.get::<_, i64>(14)? as u32,
                link_count: r.get::<_, i64>(15)? as u32,
                parent_workstream_id: r.get(16)?,
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

    let actions = list_actions_for(conn, id)?;
    // Open questions on this workstream (#113) inherit from the
    // attached notes via `workstream_signals(kind='note')`. The
    // `list_open_questions` IPC already does that join; we just
    // call it with this workstream's id.
    let open_questions = crate::notes::list_open_questions_for(
        conn,
        crate::notes::QuestionScope::Open,
        None,
        Some(id),
    )
    .unwrap_or_default();
    let links = list_workstream_links(conn, id)?;
    let children = list_children_of(conn, id)?;

    Ok(Some(WorkstreamDetail {
        workstream,
        emails,
        events,
        notes,
        actions,
        open_questions,
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note'), 0), \
                COALESCE((SELECT COUNT(*) FROM actions WHERE workstream_id = w.id AND done = 0), 0), \
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
            open_action_count: r.get::<_, i64>(14)? as u32,
            link_count: r.get::<_, i64>(15)? as u32,
            parent_workstream_id: r.get(16)?,
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
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'email'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'event'), 0), \
                COALESCE((SELECT COUNT(*) FROM workstream_signals WHERE workstream_id = w.id AND kind = 'note'), 0), \
                COALESCE((SELECT COUNT(*) FROM actions WHERE workstream_id = w.id AND done = 0), 0), \
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
                open_action_count: r.get::<_, i64>(14)? as u32,
                link_count: r.get::<_, i64>(15)? as u32,
                parent_workstream_id: r.get(16)?,
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
              AND er.team_member_id IS NOT NULL \
            UNION \
            SELECT ws.workstream_id, tma.member_id AS member_id \
            FROM workstream_signals ws \
            JOIN email_recipients er ON er.message_id = ws.item_id \
            JOIN team_member_aliases tma \
              ON tma.kind = 'email' AND LOWER(tma.value) = LOWER(er.email) \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
              AND er.team_member_id IS NULL \
            UNION \
            SELECT ws.workstream_id, tma.member_id AS member_id \
            FROM workstream_signals ws \
            JOIN email_messages em ON em.id = ws.item_id \
            JOIN team_member_aliases tma \
              ON tma.kind = 'email' AND LOWER(tma.value) = LOWER(em.from_email) \
            WHERE ws.kind = 'email' AND ws.workstream_id IN ({placeholders}) \
            UNION \
            SELECT ws.workstream_id, ca.team_member_id AS member_id \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) \
              AND ca.team_member_id IS NOT NULL \
            UNION \
            SELECT ws.workstream_id, tma.member_id AS member_id \
            FROM workstream_signals ws \
            JOIN calendar_attendees ca ON ca.event_id = ws.item_id \
            JOIN team_member_aliases tma \
              ON tma.kind = 'email' AND LOWER(tma.value) = LOWER(ca.email) \
            WHERE ws.kind = 'event' AND ws.workstream_id IN ({placeholders}) \
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

/// Bulk fetch open action texts grouped by workstream id (#101). Used
/// by the synthesizer prompt to render each workstream's current open
/// actions so the model can dedupe against them on the next pass
/// instead of re-emitting near-identical TODOs. Returns texts only
/// (not full WorkstreamAction rows) — the prompt only displays them.
/// Ordered by created_ms ASC so the prompt shows actions in stable
/// chronological order; the caller caps per-workstream count.
pub fn list_open_action_texts_grouped(
    conn: &Connection,
) -> rusqlite::Result<HashMap<String, Vec<String>>> {
    // Unified actions table (#111): any row with a workstream_id
    // contributes — note-origin rows that have been pinned to a
    // workstream show up here too.
    let mut stmt = conn.prepare(
        "SELECT a.workstream_id, a.text \
         FROM actions a \
         JOIN workstreams w ON w.id = a.workstream_id \
         WHERE a.done = 0 AND w.status IN ('active', 'archived') \
         ORDER BY a.workstream_id, a.created_ms ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        let id: String = r.get(0)?;
        let text: String = r.get(1)?;
        Ok((id, text))
    })?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, text) = row?;
        out.entry(id).or_default().push(text);
    }
    Ok(out)
}

fn list_actions_for(
    conn: &Connection,
    workstream_id: &str,
) -> rusqlite::Result<Vec<crate::notes::ActionListItem>> {
    // After the #111 unification the workstream detail's action list
    // is just `list_actions` filtered by workstream_id with no
    // done-scope (we want both open and done on the detail page).
    // Surfacing through the unified type lets the WS detail page
    // reuse the ActionRow / AssigneeChip stack on the frontend.
    crate::index::list_actions(
        conn,
        crate::notes::ActionScope::All,
        None,
        Some(workstream_id),
        None,
        None,
    )
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

    // Self team_member id for events.actor_id fallback when an action
    // has no explicit assignee (#106).
    let self_id_for_events: Option<String> = tx
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;

    // Actions: write into the unified `actions` table with
    // origin_kind='synth' (#111). Two changes from the legacy
    // workstream_actions path:
    //
    //   1. Dedup against the literal `- [ ]` line: when source_kind
    //      is 'note' and the source note already carries a row with
    //      identical normalized text, attach workstream_id to that
    //      note-origin row instead of inserting a parallel 'synth'
    //      row. Exact match only (lowercased + trimmed) — see the
    //      design discussion in #111. Synth interpretations that
    //      diverge from the literal line keep their own row.
    //
    //   2. ON CONFLICT for an existing 'synth' row preserves done,
    //      created_ms, AND assignee_id (user-mutable state) and
    //      refreshes the synthesizer metadata. assignee_id is stamped
    //      on insert from the synthesizer's owner resolution; on
    //      update we keep whatever's there so a user reassignment
    //      survives the next cluster pass.
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO actions (\
                id, origin_kind, origin_synth_kind, origin_synth_id, \
                workstream_id, text, due_ms, done, created_ms, assignee_id\
             ) VALUES (?1, 'synth', ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET \
                text = excluded.text, \
                due_ms = excluded.due_ms, \
                origin_synth_kind = excluded.origin_synth_kind, \
                origin_synth_id = excluded.origin_synth_id, \
                workstream_id = excluded.workstream_id",
        )?;
        for a in &record.actions {
            // Dedup branch: for note-sourced synth output, look for an
            // existing note-origin row on the same note with matching
            // text. If found, pin the workstream_id on that row and
            // skip the synth insert entirely.
            if a.source_kind == "note" {
                let dedup_target: Option<String> = tx
                    .query_row(
                        "SELECT id FROM actions \
                          WHERE origin_kind = 'note' \
                            AND origin_note_id = ?1 \
                            AND lower(trim(text)) = lower(trim(?2)) \
                          LIMIT 1",
                        params![a.source_id, a.text],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(existing_id) = dedup_target {
                    tx.execute(
                        "UPDATE actions SET workstream_id = ?2 \
                           WHERE id = ?1 \
                             AND (workstream_id IS NULL OR workstream_id != ?2)",
                        params![existing_id, id],
                    )?;
                    counts.actions_updated += 1;
                    continue;
                }
            }

            let aid = action_id(&id, &a.text);
            let pre_existed_action: i64 = tx
                .query_row(
                    "SELECT 1 FROM actions WHERE id = ?1",
                    params![aid],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            stmt.execute(params![
                aid,
                a.source_kind,
                a.source_id,
                id,
                a.text,
                a.due_ms,
                now_ms,
                a.assignee_id,
            ])?;
            if pre_existed_action == 0 {
                // Live action_created event (#106).
                let actor = a
                    .assignee_id
                    .as_deref()
                    .or(self_id_for_events.as_deref());
                let payload = serde_json::json!({
                    "text": a.text,
                    "workstream_id": id,
                });
                crate::events::emit(
                    tx,
                    now_ms,
                    "action_created",
                    actor,
                    "action",
                    &aid,
                    &payload,
                )?;
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

/// DB-only write path for toggling done on a synth-origin (or other
/// non-note) row in the unified `actions` table (#111). Used by the
/// unified `set_action_done` IPC for non-note origins; note-origin
/// rows round-trip through the markdown file instead.
pub fn set_action_done(
    conn: &Connection,
    action_id: &str,
    done: bool,
) -> rusqlite::Result<()> {
    let was_done: i64 = conn
        .query_row(
            "SELECT done FROM actions WHERE id = ?1",
            params![action_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // Bump `manual_override` so the profile worker stops auto-touching
    // this row (#120 follow-up). Also clear the hysteresis state
    // (#124): a user-touched row shouldn't carry a stale
    // auto-resolved stamp or omission counter — the pill must vanish
    // on manual check, and a manual uncheck should reset cleanly so
    // the next worker tick starts the counter from zero.
    conn.execute(
        "UPDATE actions \
            SET done = ?2, \
                manual_override = 1, \
                auto_resolved_ms = NULL, \
                auto_resolve_omissions = 0 \
          WHERE id = ?1",
        params![action_id, done as i64],
    )?;
    // Live action_completed event on a 0→1 transition (#106). Skipped
    // when undoing (1→0) or when state didn't change.
    if done && was_done == 0 {
        let (text, assignee_id): (String, Option<String>) = conn
            .query_row(
                "SELECT text, assignee_id FROM actions WHERE id = ?1",
                params![action_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .unwrap_or_default();
        let self_id: Option<String> = conn
            .query_row(
                "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok();
        let actor = assignee_id.as_deref().or(self_id.as_deref());
        // The events insert isn't atomic with the UPDATE above —
        // worst-case desync: action is done in the table but no event
        // row. Acceptable for v1; downstream consumers won't see a
        // dropped completion as anything worse than missing telemetry.
        let payload = serde_json::json!({ "text": text });
        let tx = conn.unchecked_transaction()?;
        crate::events::emit(
            &tx,
            crate::events::current_unix_ms(),
            "action_completed",
            actor,
            "action",
            action_id,
            &payload,
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Undo a worker auto-resolution (#124). Reopens the row, locks it
/// against further auto-resolve, and clears the hysteresis state.
/// Guarded by `auto_resolved_ms IS NOT NULL` so the path is a no-op
/// on rows the user (rather than the worker) marked done — those
/// reopen via `set_action_done(_, false)` instead.
pub fn undo_auto_resolved_action(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE actions \
            SET done = 0, \
                manual_override = 1, \
                auto_resolved_ms = NULL, \
                auto_resolve_omissions = 0 \
          WHERE id = ?1 \
            AND origin_kind = 'synth' \
            AND auto_resolved_ms IS NOT NULL",
        params![action_id],
    )?;
    Ok(())
}

/// DB-only write path for reassigning a synth-origin row in the
/// unified `actions` table (#111). User-authored override; preserved
/// across re-synthesis because the upsert ON CONFLICT clause does not
/// touch `assignee_id`.
pub fn set_action_assignee(
    conn: &Connection,
    action_id: &str,
    assignee_id: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE actions SET assignee_id = ?2, manual_override = 1 WHERE id = ?1",
        params![action_id, assignee_id],
    )?;
    Ok(())
}

/// DB-only delete path for a synth-origin row in the unified
/// `actions` table (#111). The synthesizer content-hashes ids over
/// (workstream_id, text), so re-synthesis of the same text +
/// workstream pair will recreate it — same trade-off as `done`,
/// which is preserved on conflict.
pub fn delete_action(conn: &Connection, action_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM actions WHERE id = ?1",
        params![action_id],
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
        // #106 events table — needed by live emission in write_workstream
        // and set_action_done. Backfill block produces zero rows against
        // this fresh fixture (no source data); safe.
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
        // 024 (teams_messages) is unrelated to actions; skip.
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
            parent_id: None,
        }
    }

    fn make_action(text: &str, source_kind: &str, source_id: &str) -> SynthesizedAction {
        SynthesizedAction {
            text: text.to_string(),
            due_ms: None,
            source_kind: source_kind.to_string(),
            source_id: source_id.to_string(),
            assignee_id: None,
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
    fn write_workstream_round_trips_action_assignee() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_team_member(&conn, "tm_alice");

        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws1"),
                "WS",
                &["mg:test::m1"],
                &[],
                &[],
                vec![SynthesizedAction {
                    text: "Send recap".into(),
                    due_ms: None,
                    source_kind: "email".into(),
                    source_id: "mg:test::m1".into(),
                    assignee_id: Some("tm_alice".into()),
                }],
            ),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert_eq!(detail.actions.len(), 1);
        assert_eq!(detail.actions[0].assignee_id.as_deref(), Some("tm_alice"));
    }

    #[test]
    fn write_workstream_preserves_user_assignee_on_resync() {
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);
        seed_team_member(&conn, "tm_alice");
        seed_team_member(&conn, "tm_bob");

        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws1"),
                "WS",
                &["mg:test::m1"],
                &[],
                &[],
                vec![SynthesizedAction {
                    text: "Send recap".into(),
                    due_ms: None,
                    source_kind: "email".into(),
                    source_id: "mg:test::m1".into(),
                    assignee_id: Some("tm_alice".into()),
                }],
            ),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        // User reassigns to Bob.
        let aid = action_id("ws1", "Send recap");
        set_action_assignee(&conn, &aid, Some("tm_bob")).unwrap();

        // Synthesizer re-emits Alice for the same action text.
        let tx = conn.transaction().unwrap();
        write_workstream(
            &tx,
            &make_ws(
                Some("ws1"),
                "WS",
                &["mg:test::m1"],
                &[],
                &[],
                vec![SynthesizedAction {
                    text: "Send recap".into(),
                    due_ms: None,
                    source_kind: "email".into(),
                    source_id: "mg:test::m1".into(),
                    assignee_id: Some("tm_alice".into()),
                }],
            ),
            2_000,
        )
        .unwrap();
        tx.commit().unwrap();

        // The user's override must survive — ON CONFLICT does not
        // touch assignee_id.
        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert_eq!(detail.actions[0].assignee_id.as_deref(), Some("tm_bob"));
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
            &make_ws(Some("ws_umbrella"), "Umbrella", &[], &[], &[], vec![]),
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
            &make_ws(Some("ws_g"), "Grand", &[], &[], &[], vec![]),
            1_000,
        )
        .unwrap();
        write_workstream(
            &tx,
            &make_ws(Some("ws_p"), "Parent", &[], &[], &[], vec![]),
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
    fn delete_action_removes_row() {
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
                vec![make_action("Send recap", "email", "mg:test::m1")],
            ),
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let aid = action_id("ws1", "Send recap");
        delete_action(&conn, &aid).unwrap();

        let detail = get_workstream_detail(&conn, "ws1").unwrap().unwrap();
        assert!(detail.actions.is_empty(), "delete_action must remove the row");
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

    /// Helper: seed a synth waiting action with the worker's
    /// auto-resolved state. Mirrors what `auto_resolve_missing` would
    /// leave behind once the threshold is crossed.
    fn seed_auto_resolved_action(conn: &Connection, id: &str, ts_ms: i64) {
        conn.execute(
            "INSERT INTO actions \
                (id, text, done, created_ms, \
                 origin_kind, origin_synth_kind, origin_synth_id, \
                 manual_override, auto_resolve_omissions, auto_resolved_ms) \
             VALUES (?1, 'desc', 1, 0, \
                     'synth', 'teams_waiting', 'src1', \
                     0, 2, ?2)",
            params![id, ts_ms],
        )
        .unwrap();
    }

    /// User unchecks a worker-auto-resolved row via the normal toggle:
    /// `set_action_done(_, false)` must clear the pill state.
    #[test]
    fn set_action_done_clears_auto_resolved_ms_on_uncheck() {
        let conn = open_test_db();
        seed_auto_resolved_action(&conn, "a:x", 1_000);
        set_action_done(&conn, "a:x", false).unwrap();
        let (done, mo, ms, om): (i64, i64, Option<i64>, i64) = conn
            .query_row(
                "SELECT done, manual_override, auto_resolved_ms, auto_resolve_omissions \
                   FROM actions WHERE id = 'a:x'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(done, 0);
        assert_eq!(mo, 1);
        assert!(ms.is_none(), "auto_resolved_ms must clear on user-uncheck");
        assert_eq!(om, 0);
    }

    /// Undo path reopens the row, locks it against the worker, and
    /// clears the hysteresis state in one transaction.
    #[test]
    fn undo_auto_resolved_action_reopens_and_locks() {
        let conn = open_test_db();
        seed_auto_resolved_action(&conn, "a:x", 1_000);
        undo_auto_resolved_action(&conn, "a:x").unwrap();
        let (done, mo, ms, om): (i64, i64, Option<i64>, i64) = conn
            .query_row(
                "SELECT done, manual_override, auto_resolved_ms, auto_resolve_omissions \
                   FROM actions WHERE id = 'a:x'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(done, 0);
        assert_eq!(mo, 1);
        assert!(ms.is_none());
        assert_eq!(om, 0);
    }

    /// Undo on a user-checked row (no `auto_resolved_ms`) is a no-op —
    /// the WHERE guard prevents the frontend from accidentally
    /// resurrecting a manually-completed action.
    #[test]
    fn undo_auto_resolved_action_is_noop_on_user_checked_row() {
        let conn = open_test_db();
        // User-checked row: done=1 but auto_resolved_ms IS NULL.
        conn.execute(
            "INSERT INTO actions \
                (id, text, done, created_ms, \
                 origin_kind, origin_synth_kind, origin_synth_id, \
                 manual_override, auto_resolve_omissions, auto_resolved_ms) \
             VALUES ('a:user', 'desc', 1, 0, \
                     'synth', 'teams_waiting', 'src1', \
                     1, 0, NULL)",
            [],
        )
        .unwrap();
        undo_auto_resolved_action(&conn, "a:user").unwrap();
        let done: i64 = conn
            .query_row(
                "SELECT done FROM actions WHERE id = 'a:user'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(done, 1, "user-completed row must not reopen via Undo path");
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
        let mut record = make_ws(Some("ws_new"), "Spawn", &[], &[], &[], vec![]);
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
        let record = make_ws(Some("ws_c"), "Renamed child", &[], &[], &[], vec![]);
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

    // Seed a note-origin row in the unified actions table for the
    // dedup tests below.
    fn seed_note_action(conn: &Connection, id: &str, note_id: &str, text: &str) {
        conn.execute(
            "INSERT INTO actions \
                (id, origin_kind, origin_note_id, origin_line, text, \
                 done, created_ms) \
             VALUES (?1, 'note', ?2, 1, ?3, 0, 100)",
            params![id, note_id, text],
        )
        .unwrap();
    }

    #[test]
    fn synth_dedup_attaches_workstream_to_existing_note_action() {
        // When the synthesizer extracts an action whose source is a
        // note that already carries the literal `- [ ]` line, skip
        // emitting a parallel synth row and instead pin
        // workstream_id on the existing note-origin row (#111).
        let mut conn = open_test_db();
        seed_note(&conn, "/n/a.md", 1_000);
        seed_note_action(&conn, "n:1", "/n/a.md", "Send invoice");

        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            Some("ws_x"),
            "X",
            &[],
            &[],
            &["/n/a.md"],
            vec![make_action("send invoice", "note", "/n/a.md")],
        );
        let counts = write_workstream(&tx, &ws, 2_000).unwrap();
        tx.commit().unwrap();

        // No new synth row should exist; the note row gets pinned.
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "synth must NOT emit a parallel row");
        let pinned: Option<String> = conn
            .query_row(
                "SELECT workstream_id FROM actions WHERE id = 'n:1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pinned.as_deref(), Some("ws_x"));
        assert_eq!(counts.actions_added, 0);
        assert_eq!(counts.actions_updated, 1);
    }

    #[test]
    fn synth_does_not_dedup_across_notes() {
        // The dedup is scoped to the same `source_id` note. A synth
        // row paraphrasing the same text from a *different* note must
        // produce its own row.
        let mut conn = open_test_db();
        seed_note(&conn, "/n/a.md", 1_000);
        seed_note(&conn, "/n/b.md", 1_000);
        seed_note_action(&conn, "n:a", "/n/a.md", "Send invoice");

        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            Some("ws_x"),
            "X",
            &[],
            &[],
            &["/n/b.md"],
            vec![make_action("Send invoice", "note", "/n/b.md")],
        );
        write_workstream(&tx, &ws, 2_000).unwrap();
        tx.commit().unwrap();

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
        // The original note row is untouched (still NULL workstream).
        let na_ws: Option<String> = conn
            .query_row(
                "SELECT workstream_id FROM actions WHERE id = 'n:a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(na_ws, None);
    }

    #[test]
    fn synth_does_not_dedup_when_source_kind_is_email() {
        // The dedup branch only fires for source_kind='note'.
        // Email/event synth rows always go in as their own rows.
        let mut conn = open_test_db();
        seed_email(&conn, "mg:test::m1", 1_000);

        let tx = conn.transaction().unwrap();
        let ws = make_ws(
            Some("ws_x"),
            "X",
            &["mg:test::m1"],
            &[],
            &[],
            vec![make_action("Reply to Anna", "email", "mg:test::m1")],
        );
        let counts = write_workstream(&tx, &ws, 2_000).unwrap();
        tx.commit().unwrap();

        assert_eq!(counts.actions_added, 1);
        let kind: String = conn
            .query_row(
                "SELECT origin_kind FROM actions LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "synth");
    }
}
