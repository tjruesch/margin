//! Workstream synthesis pipeline (#70).
//!
//! Takes raw signals from #69 (emails), #63 (calendar events), and the
//! existing notes index, hands them to Claude, and writes back named
//! "workstreams" — ongoing efforts the user is participating in (e.g.
//! "Hyundai POC review", "Q3 hiring").
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
pub mod link_categorizer;
pub mod link_summarizer;
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
    /// Parent workstream id (#89). `None` for top-level workstreams.
    /// Hierarchy is flat 2-level; the synthesizer's `write_workstream`
    /// + the manual `set_workstream_parent` command both validate
    /// against would-be-grandparent / self-parent / has-children
    /// before persisting.
    pub parent_workstream_id: Option<String>,
    /// Derived list of team_member ids involved in the workstream
    /// (#81). Computed on read by joining the workstream's pivot
    /// emails / events against `email_recipients` / `calendar_attendees`
    /// where `team_member_id IS NOT NULL`. UI maps ids to names via
    /// the existing `listTeamMembers` cache.
    pub members: Vec<String>,
    pub email_count: u32,
    pub event_count: u32,
    pub note_count: u32,
    /// Count of user-curated external links (#88). Drives the small
    /// link-icon badge on the list card; the actual links land in
    /// `WorkstreamDetail.links` on detail-view fetch.
    pub link_count: u32,
    /// Email addresses that participate in the workstream's emails or
    /// events but don't resolve to any team_member. Sorted by signal
    /// count desc; deduped case-insensitively. Capped per workstream
    /// (see `EXTERNAL_PARTICIPANT_CAP` in `persist.rs`). Drives the
    /// "External" chip strip on the detail view and the "+N external"
    /// pill on the list card.
    pub external_participants: Vec<ExternalParticipant>,
}

/// One email address that participates in a workstream but has no
/// corresponding `team_member` (no team_member_id on the recipient /
/// attendee row, and — for senders — no matching `team_member_aliases`
/// entry of kind 'email'). Surfaces external counterparties on the
/// workstream detail + list views.
#[derive(Debug, Clone, Serialize)]
pub struct ExternalParticipant {
    /// Lowercased canonical email.
    pub email: String,
    /// First non-null display name encountered across the joined rows.
    /// `None` when only the bare address is known.
    pub display_name: Option<String>,
    /// Number of signals (emails + events) that involve this address.
    /// Used to sort the chip strip — frequent counterparties first.
    pub count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteRef {
    pub note_path: String,
    pub title: String,
    pub modified_ms: i64,
}

/// User-curated external URL on a workstream (#88). Pure user
/// curation — synthesizer never touches this. Rendered as clickable
/// chips on the detail view and folded into `read_workstream` for AI
/// ask context.
#[derive(Debug, Clone, Serialize)]
pub struct WorkstreamLink {
    pub id: String,
    pub workstream_id: String,
    pub label: String,
    pub url: String,
    /// Soft enum — `kinds` module below holds the canonical strings.
    /// `None` is allowed and renders with a generic link glyph.
    pub kind: Option<String>,
    pub position: i64,
    pub created_ms: i64,
    /// AI-generated 2–3 sentence summary of the linked page. Populated
    /// by a background task (Firecrawl scrape + Haiku summarize) after
    /// the link row is inserted; `None` while the task is in flight or
    /// after a silent failure (missing keys, scrape error, etc.).
    pub summary: Option<String>,
}

/// Canonical string values for `WorkstreamLink.kind`. Soft enum so
/// adding a new kind is non-breaking — just extend the icon mapping
/// client-side.
pub mod link_kinds {
    pub const GITHUB: &str = "github";
    pub const LINEAR: &str = "linear";
    pub const NOTION: &str = "notion";
    pub const FIGMA: &str = "figma";
    pub const OTHER: &str = "other";
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkstreamDetail {
    #[serde(flatten)]
    pub workstream: Workstream,
    pub emails: Vec<crate::connectors::email::EmailMessage>,
    pub events: Vec<crate::connectors::calendar::CalendarEvent>,
    pub notes: Vec<NoteRef>,
    /// Open questions inheriting from this workstream's attached
    /// notes via `workstream_signals(kind='note')` (#113).
    pub open_questions: Vec<crate::notes::OpenQuestionItem>,
    pub links: Vec<WorkstreamLink>,
    /// Teams chat messages attached to this workstream via the
    /// `workstream_signals` pivot (kind='teams_message'). Recency-desc
    /// like emails. Empty for workstreams without chat signal. (#105)
    pub teams_messages: Vec<crate::connectors::teams::TeamsMessage>,
    /// Direct children when this workstream is a parent (#89). Lean
    /// `Workstream` shape — counts + members already populated, no
    /// emails/events/notes hydration. Empty for leaves and
    /// standalones. Ordered by `last_activity_ms` desc.
    pub children: Vec<Workstream>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ClusterReport {
    pub workstreams_added: u32,
    pub workstreams_updated: u32,
    /// Workstreams the synthesizer resurrected from archived → active
    /// because new evidence rolled in (#78).
    pub workstreams_reopened: u32,
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
}
