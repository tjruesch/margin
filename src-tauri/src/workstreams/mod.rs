//! Workstream synthesis pipeline (#70).
//!
//! Takes raw signals from #69 (emails), #63 (calendar events), and the
//! existing notes index, hands them to Claude, and writes back named
//! "workstreams" — ongoing efforts the user is participating in (e.g.
//! "Hyundai POC review", "Q3 hiring") with attached action items.
//!
//! No UI in this module — the data layer is callable end-to-end after
//! this PR; the Workstreams view (#71) and AI ask integration (#72)
//! consume the rows produced here.
//!
//! Concurrency: `CLUSTER_LOCK` ensures at most one synthesis pass is
//! in flight at a time. `try_lock` callers (boot tick + manual
//! refresh) early-return when the lock is held instead of queueing.

use std::sync::OnceLock;

use serde::Serialize;
use tokio::sync::Mutex;

pub mod commands;
pub mod persist;
pub mod signals;
pub mod synthesizer;

/// Process-wide guard against overlapping synthesis passes. The boot
/// hook fires `maybe_cluster(false)` ~5s after launch; the user can
/// also click Refresh in the Workstreams view (#71) at any time. Both
/// paths must serialize on this lock to avoid duplicate Anthropic
/// calls and racing pivot writes.
pub fn cluster_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Persisted workstream row + joined counts for the list view.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Workstream {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub status: String,
    pub last_activity_ms: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
    /// User-authored ground-truth context (#77). Treated as
    /// authoritative by the synthesizer prompt and surfaced in
    /// `read_workstream` tool output. `None` when empty.
    pub user_notes: Option<String>,
    /// Stamped on archive transitions (#78). Manual unarchive clears
    /// this; synthesizer-driven resurrect leaves it as historical
    /// record so the UI can show "archived 12 days ago, reopened today".
    pub archived_at_ms: Option<i64>,
    /// Set when the synthesizer flips a previously-archived workstream
    /// back to active because new evidence rolled in (#78). Cleared on
    /// detail-view unmount via `mark_workstream_seen`. The "Reopened"
    /// badge is just `reopened_at_ms.is_some() && status == "active"`.
    pub reopened_at_ms: Option<i64>,
    /// User-set internal owner of the workstream (#81). Single
    /// team_member id; `None` when unassigned. Synthesizer never sets
    /// this — same authority pattern as user_notes.
    pub owner_member_id: Option<String>,
    /// Derived list of team_member ids involved in the workstream
    /// (#81). Computed on read by joining the workstream's pivot
    /// emails / events against `email_recipients` / `calendar_attendees`
    /// where `team_member_id IS NOT NULL`. UI maps ids to names via
    /// the existing `listTeamMembers` cache.
    pub members: Vec<String>,
    pub email_count: u32,
    pub event_count: u32,
    pub note_count: u32,
    pub open_action_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkstreamAction {
    pub id: String,
    pub workstream_id: String,
    pub text: String,
    pub due_ms: Option<i64>,
    /// "email" | "event" | "note"
    pub source_kind: String,
    pub source_id: String,
    pub done: bool,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteRef {
    pub note_path: String,
    pub title: String,
    pub modified_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkstreamDetail {
    #[serde(flatten)]
    pub workstream: Workstream,
    pub emails: Vec<crate::connectors::email::EmailMessage>,
    pub events: Vec<crate::connectors::calendar::CalendarEvent>,
    pub notes: Vec<NoteRef>,
    pub actions: Vec<WorkstreamAction>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ClusterReport {
    pub workstreams_added: u32,
    pub workstreams_updated: u32,
    /// Workstreams the synthesizer resurrected from archived → active
    /// because new evidence rolled in (#78).
    pub workstreams_reopened: u32,
    pub actions_added: u32,
    pub actions_updated: u32,
    pub items_clustered: u32,
    pub model: String,
    pub last_clustered_ms: i64,
    /// "synced" on a successful pass, "skipped" when stale-check or
    /// lock guard short-circuited, "errored" on failure (caller
    /// returns Err in that case but the field is left here for
    /// future event payload reuse).
    pub state: String,
}

/// Per-workstream contribution to a `ClusterReport`. Returned from
/// `persist::write_workstream` so the synthesizer can roll up totals.
#[derive(Debug, Default, Clone)]
pub struct WriteCounts {
    pub workstream_added: bool,
    pub actions_added: u32,
    pub actions_updated: u32,
}
