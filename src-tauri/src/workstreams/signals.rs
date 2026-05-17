//! Workstream signal hydration + snapshot layer (#85, #86).
//!
//! Workstreams cite items from many domains: emails, calendar events,
//! notes, and (future) GitHub PRs, Slack threads, Linear issues, etc.
//! The DB-side pivot is uniform — `workstream_signals(workstream_id,
//! kind, item_id)` — but each domain has its own rich row type. This
//! module abstracts each domain behind a single `Signal` trait plus a
//! `SignalRegistry` so the synthesizer's prompt loop and
//! `get_workstream_detail` both dispatch polymorphically without
//! growing per-source branches.
//!
//! Each impl owns three things:
//!   - **Hydration** (#85): `hydrate(conn, item_ids)` turns pivot rows
//!     into rich domain objects for the detail view + AI ask prompts.
//!   - **Snapshot** (#86): `snapshot(conn, window, cap)` reads the
//!     recent items the synthesizer should cluster.
//!   - **Render** (#86): `format(item)` produces the per-item lines
//!     for the cluster-pass user message. Multi-line is allowed (email
//!     emits header + indented body excerpt), so this is not strictly
//!     "one line".
//!
//! Per-source defaults — `default_window()`, `default_cap()`,
//! `label_prefix()` — live next to each impl so adding a source is a
//! one-file change with no synthesizer-side surgery.
//!
//! Adding a new source: define a `Signal` impl, register it in
//! `default_with_builtins`, done. No changes to `persist.rs`,
//! `synthesizer.rs`, or any UI code unless the new kind also gets its
//! own slot on `WorkstreamDetail`.

use std::collections::HashMap;
use std::sync::OnceLock;

use rusqlite::{params, Connection, OptionalExtension};

use super::NoteRef;
use crate::connectors::calendar::{self, CalendarEvent};
use crate::connectors::email::{self, EmailMessage};
use crate::connectors::teams::{self, TeamsMessage};
use crate::index;

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
    TeamsMessage(TeamsMessage),
}

/// One snapshot item from a source. `id` is the canonical id stored in
/// `workstream_signals.item_id`; `payload` is the rich row used for
/// rendering. Closed-enum mirrors `HydratedSignal` so consumers can
/// pattern-match without going through `Any`.
#[derive(Debug)]
pub struct SnapshotItem {
    pub id: String,
    pub payload: SnapshotPayload,
    /// Number of underlying rows this snapshot row represents. `1` for
    /// any non-collapsed source (emails, notes, Teams messages). For
    /// calendar events it carries `CollapsedEvent.instance_count` —
    /// the count of recurring occurrences within the snapshot window
    /// (#126). Lets `format` annotate the prompt line so the LLM can
    /// weight cadence-heavy workstreams correctly.
    pub instance_count: usize,
}

#[derive(Debug)]
pub enum SnapshotPayload {
    Email(EmailMessage),
    Event(CalendarEvent),
    Note(NoteRef),
    TeamsMessage(TeamsMessage),
}

/// Per-source time window for snapshotting. `back_ms` and
/// `forward_ms` are both non-negative durations; the source applies
/// `[now - back_ms, now + forward_ms]`. Forward is non-zero for
/// calendar (upcoming meetings count) and ~1 day for email (covers
/// clock skew on freshly-sent items); zero for notes.
#[derive(Debug, Clone, Copy)]
pub struct SignalWindow {
    pub back_ms: i64,
    pub forward_ms: i64,
}

/// Per-domain hydrator + snapshotter. `kind` is the discriminator
/// stored in the `workstream_signals.kind` column.
pub trait Signal: Send + Sync {
    /// The discriminator used in `workstream_signals.kind`. The
    /// registry already keys impls by the same string, so callers
    /// rarely need this — but it lets a `&dyn Signal` self-identify
    /// in logs and future generic dispatch.
    #[allow(dead_code)]
    fn kind(&self) -> &'static str;

    /// Single-letter prefix for the labels Claude sees in the
    /// cluster-pass prompt — "M" for email, "E" for events, "N" for
    /// notes. Each label is `"{prefix}{1-based index}"`. Must be
    /// unique across registered sources; the registry asserts this.
    fn label_prefix(&self) -> &'static str;

    /// Markdown header used for this source's section in the cluster
    /// prompt. Includes the human-readable window so Claude sees the
    /// time horizon: e.g. "Recent emails (last 14 days)". Does NOT
    /// include the leading `#` — the synthesizer adds it.
    fn prompt_section_title(&self) -> &'static str;

    /// Default snapshot window for cluster passes. Lives here (not in
    /// `synthesizer.rs`) so adding a source is a one-file change.
    fn default_window(&self) -> SignalWindow;

    /// Default per-source cap on snapshot items. The synthesizer may
    /// later allocate against a global token budget; for now each
    /// source returns up to its own cap.
    fn default_cap(&self) -> usize;

    /// Read recent items from local storage. Implementations return
    /// recency-desc, already deduplicated. The cap is advisory — the
    /// caller may pass a smaller value when budget is tight. `now_ms`
    /// is injected (not read from `SystemTime::now()`) so tests can
    /// pin the window deterministically.
    fn snapshot(
        &self,
        conn: &Connection,
        now_ms: i64,
        window: SignalWindow,
        cap: usize,
    ) -> rusqlite::Result<Vec<SnapshotItem>>;

    /// Render one snapshot item for the cluster-pass user message.
    /// `label` is the Claude-facing identifier (e.g. "M3"). Multi-line
    /// output is allowed — email emits header + indented body excerpt.
    /// The trailing newline is the caller's responsibility.
    fn format(&self, label: &str, item: &SnapshotItem) -> String;

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

// ----- Built-in sources ---------------------------------------------------

pub struct EmailSignal;

/// Email: 14d back, +1d forward (covers clock skew on freshly-sent
/// items), label prefix "M". Cap of 500 mirrors the previous
/// `list_messages_in_range` limit in `synthesizer.rs`.
const EMAIL_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const EMAIL_WINDOW_FORWARD_MS: i64 = 24 * 3600 * 1000;
const EMAIL_CAP: usize = 500;

impl Signal for EmailSignal {
    fn kind(&self) -> &'static str {
        "email"
    }
    fn label_prefix(&self) -> &'static str {
        "M"
    }
    fn prompt_section_title(&self) -> &'static str {
        "Recent emails (last 14 days)"
    }
    fn default_window(&self) -> SignalWindow {
        SignalWindow {
            back_ms: EMAIL_WINDOW_BACK_MS,
            forward_ms: EMAIL_WINDOW_FORWARD_MS,
        }
    }
    fn default_cap(&self) -> usize {
        EMAIL_CAP
    }
    fn snapshot(
        &self,
        conn: &Connection,
        now_ms: i64,
        window: SignalWindow,
        cap: usize,
    ) -> rusqlite::Result<Vec<SnapshotItem>> {
        let messages = email::list_messages_in_range(
            conn,
            now_ms - window.back_ms,
            now_ms + window.forward_ms,
            None,
            cap,
        )?;
        Ok(messages
            .into_iter()
            .map(|m| SnapshotItem {
                id: m.id.clone(),
                payload: SnapshotPayload::Email(m),
                instance_count: 1,
            })
            .collect())
    }
    fn format(&self, label: &str, item: &SnapshotItem) -> String {
        let m = match &item.payload {
            SnapshotPayload::Email(m) => m,
            _ => return String::new(),
        };
        let date = format_iso_date(m.sent_at_ms);
        let from = format_from(&m.from_email, m.from_name.as_deref());
        let body = email_body_excerpt(m);
        // Trailing blank line is intentional — emails are multi-line
        // and the blank separator keeps headers visually distinct in
        // the prompt. Events and notes are single-line and don't need it.
        format!(
            "[{label}] {date} — From: {from} — Subject: {subject}\n{body}\n\n",
            subject = m.subject,
        )
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

/// Calendar: ±14d, label prefix "E".
const EVENT_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const EVENT_WINDOW_FORWARD_MS: i64 = 14 * 24 * 3600 * 1000;
/// list_events_in_range has no LIMIT param; cap is enforced post-fetch.
const EVENT_CAP: usize = 500;

impl Signal for EventSignal {
    fn kind(&self) -> &'static str {
        "event"
    }
    fn label_prefix(&self) -> &'static str {
        "E"
    }
    fn prompt_section_title(&self) -> &'static str {
        "Recent calendar events (window: -14d .. +14d)"
    }
    fn default_window(&self) -> SignalWindow {
        SignalWindow {
            back_ms: EVENT_WINDOW_BACK_MS,
            forward_ms: EVENT_WINDOW_FORWARD_MS,
        }
    }
    fn default_cap(&self) -> usize {
        EVENT_CAP
    }
    fn snapshot(
        &self,
        conn: &Connection,
        now_ms: i64,
        window: SignalWindow,
        cap: usize,
    ) -> rusqlite::Result<Vec<SnapshotItem>> {
        let events = calendar::list_events_in_range(
            conn,
            now_ms - window.back_ms,
            now_ms + window.forward_ms,
            None,
        )?;
        // Collapse recurring occurrences before the cap (#109). A
        // daily standup used to contribute 14 [E*] lines; now it
        // contributes one canonical row per series. Cap then truncates
        // the post-collapse list — recurring series can no longer
        // crowd out distinct one-off meetings from the prompt.
        let mut collapsed = calendar::collapse_recurring(events, now_ms);
        collapsed.truncate(cap);
        Ok(collapsed
            .into_iter()
            .map(|c| SnapshotItem {
                id: c.canonical.id.clone(),
                instance_count: c.instance_count,
                payload: SnapshotPayload::Event(c.canonical),
            })
            .collect())
    }
    fn format(&self, label: &str, item: &SnapshotItem) -> String {
        let e = match &item.payload {
            SnapshotPayload::Event(e) => e,
            _ => return String::new(),
        };
        let when = format_iso_datetime(e.start_ms);
        let attendees: Vec<&str> =
            e.attendees.iter().take(8).map(|a| a.email.as_str()).collect();
        let attendees_str = if attendees.is_empty() {
            String::new()
        } else {
            format!(" — Attendees: {}", attendees.join(", "))
        };
        // #126: when this row collapses multiple occurrences of a
        // recurring series (#109), tell the LLM the cadence so it can
        // weight workstreams organised around daily standups vs. one-
        // off meetings correctly. Singletons render unchanged.
        let recurrence_str = if item.instance_count > 1 {
            format!(
                " (recurring, {n} occurrences in window)",
                n = item.instance_count
            )
        } else {
            String::new()
        };
        format!(
            "[{label}] {when} — {title}{attendees_str}{recurrence_str}\n",
            title = e.title
        )
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

/// Notes: 30d back, label prefix "N". The directory cap (200) is the
/// upstream ceiling; window filtering is applied after.
const NOTE_WINDOW_BACK_MS: i64 = 30 * 24 * 3600 * 1000;
const NOTE_DIRECTORY_LIMIT: usize = 200;
const NOTE_CAP: usize = 200;

impl Signal for NoteSignal {
    fn kind(&self) -> &'static str {
        "note"
    }
    fn label_prefix(&self) -> &'static str {
        "N"
    }
    fn prompt_section_title(&self) -> &'static str {
        "Recent notes (last 30 days)"
    }
    fn default_window(&self) -> SignalWindow {
        SignalWindow {
            back_ms: NOTE_WINDOW_BACK_MS,
            forward_ms: 0,
        }
    }
    fn default_cap(&self) -> usize {
        NOTE_CAP
    }
    fn snapshot(
        &self,
        conn: &Connection,
        now_ms: i64,
        window: SignalWindow,
        cap: usize,
    ) -> rusqlite::Result<Vec<SnapshotItem>> {
        let cutoff = now_ms - window.back_ms;
        let directory = index::list_directory(conn, NOTE_DIRECTORY_LIMIT)?;
        let mut items: Vec<SnapshotItem> = directory
            .into_iter()
            .filter(|d| d.modified_ms >= cutoff)
            .take(cap)
            .map(|d| SnapshotItem {
                id: d.note_path.clone(),
                payload: SnapshotPayload::Note(NoteRef {
                    note_path: d.note_path,
                    title: d.title,
                    modified_ms: d.modified_ms,
                }),
                instance_count: 1,
            })
            .collect();
        // `index::list_directory` already returns recency-desc; the
        // sort here just locks the contract in case that ever shifts.
        items.sort_by(|a, b| {
            let am = match &a.payload {
                SnapshotPayload::Note(n) => n.modified_ms,
                _ => 0,
            };
            let bm = match &b.payload {
                SnapshotPayload::Note(n) => n.modified_ms,
                _ => 0,
            };
            bm.cmp(&am)
        });
        Ok(items)
    }
    fn format(&self, label: &str, item: &SnapshotItem) -> String {
        let n = match &item.payload {
            SnapshotPayload::Note(n) => n,
            _ => return String::new(),
        };
        let date = format_iso_date(n.modified_ms);
        format!("[{label}] {date} — {title}\n", title = n.title)
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
            "SELECT n.id, COALESCE(n.title, ''), COALESCE(n.modified_ms, 0) \
             FROM notes n \
             WHERE n.id IN ({placeholders}) \
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

pub struct TeamsMessageSignal;

/// Teams messages: 14d back, label prefix "C" for "chat". Cap of 300
/// keeps the synthesizer prompt manageable for chatty accounts —
/// short messages don't add much per-token signal so the cap is
/// lower than email's 500.
const TEAMS_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const TEAMS_CAP: usize = 300;

impl Signal for TeamsMessageSignal {
    fn kind(&self) -> &'static str {
        "teams_message"
    }
    fn label_prefix(&self) -> &'static str {
        "C"
    }
    fn prompt_section_title(&self) -> &'static str {
        "Recent Teams messages (last 14 days)"
    }
    fn default_window(&self) -> SignalWindow {
        SignalWindow {
            back_ms: TEAMS_WINDOW_BACK_MS,
            forward_ms: 0,
        }
    }
    fn default_cap(&self) -> usize {
        TEAMS_CAP
    }
    fn snapshot(
        &self,
        conn: &Connection,
        now_ms: i64,
        window: SignalWindow,
        cap: usize,
    ) -> rusqlite::Result<Vec<SnapshotItem>> {
        let messages = teams::list_messages_in_range(
            conn,
            now_ms - window.back_ms,
            now_ms + window.forward_ms,
            cap,
        )?;
        Ok(messages
            .into_iter()
            .map(|m| SnapshotItem {
                id: m.id.clone(),
                payload: SnapshotPayload::TeamsMessage(m),
                instance_count: 1,
            })
            .collect())
    }
    fn format(&self, label: &str, item: &SnapshotItem) -> String {
        let m = match &item.payload {
            SnapshotPayload::TeamsMessage(m) => m,
            _ => return String::new(),
        };
        let date = format_iso_date(m.sent_at_ms);
        let chat = m
            .chat_topic
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| match m.chat_kind.as_str() {
                "oneOnOne" => "DM",
                "group" => "Group chat",
                "meeting" => "Meeting chat",
                _ => "Chat",
            });
        let from = m
            .from_name
            .as_deref()
            .or(m.from_email.as_deref())
            .unwrap_or("(unknown sender)");
        let body_raw: String = m
            .body_preview
            .clone()
            .or_else(|| m.body_html.as_deref().map(strip_html))
            .unwrap_or_default();
        let body = collapse_inline(&body_raw);
        format!("[{label}] {date} — {chat} — {from}: {body}\n")
    }
    fn hydrate(
        &self,
        conn: &Connection,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>> {
        let mut messages = teams::get_message_details_batch(conn, item_ids)?;
        messages.sort_by(|a, b| b.sent_at_ms.cmp(&a.sent_at_ms));
        Ok(messages
            .into_iter()
            .map(HydratedSignal::TeamsMessage)
            .collect())
    }
}

/// Inline-collapse for Teams previews — strips newlines + tabs to a
/// single space so the prompt line stays one line.
fn collapse_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

// ----- Render helpers -----------------------------------------------------

fn email_body_excerpt(m: &EmailMessage) -> String {
    let raw = m
        .body_html
        .as_deref()
        .or(m.body_preview.as_deref())
        .unwrap_or("");
    if raw.is_empty() {
        return String::new();
    }
    // Strip HTML tags + collapse whitespace. Keeps the prompt token-
    // efficient without pulling in a full HTML parser; preview-only
    // emails skip the strip logic.
    let stripped = if raw.contains('<') {
        strip_html(raw)
    } else {
        raw.to_string()
    };
    let collapsed = collapse_ws(&stripped);
    let truncated: String = collapsed.chars().take(800).collect();
    format!("    {}", truncated)
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            out.push(' ');
            continue;
        }
        if !in_tag {
            out.push(ch);
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn format_from(email: &str, name: Option<&str>) -> String {
    match name {
        Some(n) if !n.is_empty() => format!("{n} <{email}>"),
        _ => email.to_string(),
    }
}

fn format_iso_date(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_iso_datetime(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ----- Registry ------------------------------------------------------------

pub struct SignalRegistry {
    /// Sources in the order they appear in the cluster-pass prompt.
    /// Stable order is part of the contract: re-ordering would change
    /// the labels Claude returns and (more importantly) churn the
    /// prompt Anthropic sees on every cluster pass.
    sources: Vec<Box<dyn Signal>>,
    /// Index into `sources` keyed by `kind()`.
    by_kind: HashMap<&'static str, usize>,
}

impl SignalRegistry {
    pub fn default_with_builtins() -> Self {
        let sources: Vec<Box<dyn Signal>> = vec![
            Box::new(EmailSignal),
            Box::new(EventSignal),
            Box::new(NoteSignal),
            Box::new(TeamsMessageSignal),
        ];
        Self::from_sources(sources)
    }

    /// Build a registry from an explicit ordered list. Asserts that
    /// `kind()` and `label_prefix()` are unique across sources — a
    /// duplicate would silently shadow a peer in `by_kind` or collide
    /// in Claude's labels.
    pub fn from_sources(sources: Vec<Box<dyn Signal>>) -> Self {
        let mut by_kind: HashMap<&'static str, usize> = HashMap::new();
        let mut seen_prefixes: HashMap<&'static str, &'static str> = HashMap::new();
        for (idx, src) in sources.iter().enumerate() {
            let kind = src.kind();
            assert!(
                by_kind.insert(kind, idx).is_none(),
                "duplicate Signal kind: {kind}"
            );
            let prefix = src.label_prefix();
            if let Some(other) = seen_prefixes.insert(prefix, kind) {
                panic!("Signal label_prefix {prefix:?} used by both {other:?} and {kind:?}");
            }
        }
        Self { sources, by_kind }
    }

    /// Iterate sources in stable prompt-section order. Used by the
    /// synthesizer to drive the `# Recent <kind>` loop.
    pub fn iter_in_prompt_order(&self) -> impl Iterator<Item = &dyn Signal> {
        self.sources.iter().map(|b| b.as_ref())
    }

    /// Look up a source by `kind`. `None` for unknown kinds; the
    /// synthesizer only needs this for the email-specific lazy-body
    /// pre-step.
    #[allow(dead_code)]
    pub fn get(&self, kind: &str) -> Option<&dyn Signal> {
        self.by_kind.get(kind).map(|&idx| self.sources[idx].as_ref())
    }

    pub fn hydrate(
        &self,
        conn: &Connection,
        kind: &str,
        item_ids: &[String],
    ) -> rusqlite::Result<Vec<HydratedSignal>> {
        match self.by_kind.get(kind) {
            Some(&idx) => self.sources[idx].hydrate(conn, item_ids),
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
           AND manual_detached_ms IS NULL \
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
                 id          TEXT PRIMARY KEY,
                 bundle_id   TEXT NOT NULL DEFAULT '',
                 title       TEXT NOT NULL,
                 modified_ms INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/009_calendar.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/010_event_note_link.sql"))
            .unwrap();
        // #112 renamed linked_note_path → linked_note_id on
        // calendar_events. Apply just that column rename here so the
        // event-signal hydrator's SELECT works.
        conn.execute_batch(
            "ALTER TABLE calendar_events ADD COLUMN linked_note_id TEXT;\
             DROP INDEX IF EXISTS idx_events_linked_note;\
             ALTER TABLE calendar_events DROP COLUMN linked_note_path;",
        )
        .unwrap();
        // #109 added series_master_id to calendar_events.
        conn.execute_batch(include_str!(
            "../migrations/033_calendar_series_master_id.sql"
        ))
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
        // 034 adds workstream_signals.manual_detached_ms (#129).
        conn.execute_batch(include_str!(
            "../migrations/034_workstream_signal_tombstone.sql"
        ))
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

    fn seed_note(conn: &Connection, note_id: &str, modified: i64) {
        // After #112 the path param holds a note id.
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms) VALUES (?1, ?1, ?2, ?3)",
            params![note_id, "Note", modified],
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

    // ----- Snapshot + format (#86) ----------------------------------------

    #[test]
    fn registry_iter_in_prompt_order_is_stable() {
        let reg = SignalRegistry::default_with_builtins();
        let kinds: Vec<&'static str> =
            reg.iter_in_prompt_order().map(|s| s.kind()).collect();
        assert_eq!(kinds, vec!["email", "event", "note", "teams_message"]);
        let prefixes: Vec<&'static str> = reg
            .iter_in_prompt_order()
            .map(|s| s.label_prefix())
            .collect();
        assert_eq!(prefixes, vec!["M", "E", "N", "C"]);
    }

    #[test]
    #[should_panic(expected = "duplicate Signal kind")]
    fn registry_panics_on_duplicate_kind() {
        // Two EmailSignals share kind "email" — registry must reject.
        let sources: Vec<Box<dyn Signal>> =
            vec![Box::new(EmailSignal), Box::new(EmailSignal)];
        SignalRegistry::from_sources(sources);
    }

    #[test]
    fn email_snapshot_returns_window_recency_desc() {
        let conn = open_test_db();
        let now = 1_000_000_000;
        let day = 24 * 3600 * 1000;
        seed_email(&conn, "mg:test::recent", now - day);
        seed_email(&conn, "mg:test::older", now - 5 * day);
        seed_email(&conn, "mg:test::ancient", now - 60 * day); // outside 14d window

        let items = EmailSignal
            .snapshot(&conn, now, EmailSignal.default_window(), 100)
            .unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["mg:test::recent", "mg:test::older"]);
    }

    #[test]
    fn email_format_renders_header_then_indented_body() {
        let m = EmailMessage {
            id: "mg:test::m".into(),
            connector_id: "mg:test".into(),
            external_id: "m".into(),
            thread_id: "t".into(),
            subject: "Renewal".into(),
            from_email: "alice@x.io".into(),
            from_name: Some("Alice".into()),
            sent_at_ms: 1_700_000_000_000,
            body_preview: Some("Hi, please review.".into()),
            body_html: None,
            has_attachments: false,
            is_read: true,
            raw_etag: None,
            modified_ms: 1_700_000_000_000,
            recipients: Vec::new(),
        };
        let item = SnapshotItem {
            id: m.id.clone(),
            payload: SnapshotPayload::Email(m),
            instance_count: 1,
        };
        let out = EmailSignal.format("M3", &item);
        // 2023-11-14 — From: Alice <alice@x.io> — Subject: Renewal
        assert!(out.starts_with("[M3] 2023-11-14 — From: Alice <alice@x.io> — Subject: Renewal\n"));
        assert!(out.contains("    Hi, please review."));
        assert!(out.ends_with("\n\n"), "trailing blank line for multi-line item");
    }

    #[test]
    fn event_format_renders_single_line_with_attendees() {
        let e = CalendarEvent {
            id: "mg:test::e".into(),
            connector_id: "mg:test".into(),
            external_id: "e".into(),
            title: "Hyundai sync".into(),
            start_ms: 1_700_000_000_000,
            end_ms: 1_700_000_000_000 + 3600_000,
            all_day: false,
            location: None,
            description: None,
            source_calendar: None,
            status: None,
            raw_etag: None,
            modified_ms: 1_700_000_000_000,
            linked_note_id: None,
            series_master_id: None,
            attendees: vec![calendar::CalendarAttendee {
                email: "bob@x.io".into(),
                display_name: None,
                response_status: None,
                is_self: false,
                is_organizer: false,
                team_member_id: None,
            }],
        };
        let item = SnapshotItem {
            id: e.id.clone(),
            payload: SnapshotPayload::Event(e),
            instance_count: 1,
        };
        let out = EventSignal.format("E2", &item);
        assert_eq!(
            out,
            "[E2] 2023-11-14 22:13 — Hyundai sync — Attendees: bob@x.io\n"
        );
    }

    /// #126: when `instance_count > 1` the event line gains a
    /// `(recurring, N occurrences in window)` suffix so the LLM knows
    /// this row stands for many underlying meetings. `instance_count = 1`
    /// renders unchanged (covered by the test above).
    #[test]
    fn event_format_renders_recurring_count_suffix() {
        let e = CalendarEvent {
            id: "mg:test::e".into(),
            connector_id: "mg:test".into(),
            external_id: "e".into(),
            title: "Daily standup".into(),
            start_ms: 1_700_000_000_000,
            end_ms: 1_700_000_000_000 + 30 * 60 * 1000,
            all_day: false,
            location: None,
            description: None,
            source_calendar: None,
            status: None,
            raw_etag: None,
            modified_ms: 1_700_000_000_000,
            linked_note_id: None,
            series_master_id: Some("master-x".into()),
            attendees: Vec::new(),
        };
        let item = SnapshotItem {
            id: e.id.clone(),
            payload: SnapshotPayload::Event(e),
            instance_count: 14,
        };
        let out = EventSignal.format("E1", &item);
        assert_eq!(
            out,
            "[E1] 2023-11-14 22:13 — Daily standup (recurring, 14 occurrences in window)\n"
        );
    }

    #[test]
    fn note_format_renders_single_line() {
        let n = NoteRef {
            note_path: "/notes/x.md".into(),
            title: "Sourcing plan".into(),
            modified_ms: 1_700_000_000_000,
        };
        let item = SnapshotItem {
            id: n.note_path.clone(),
            payload: SnapshotPayload::Note(n),
            instance_count: 1,
        };
        let out = NoteSignal.format("N1", &item);
        assert_eq!(out, "[N1] 2023-11-14 — Sourcing plan\n");
    }
}
