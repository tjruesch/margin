//! AI-powered Q&A over the user's notes (#31 follow-up).
//!
//! The search palette can escalate a query to "Ask" mode (Cmd+Enter). We
//! retrieve top-N candidate notes via the existing FTS index, build a
//! prompt with labeled excerpts, and stream Anthropic's response back to
//! the frontend as `ai-stream` events. The model is instructed to cite
//! sources via `[N]` markers; the frontend renders those as clickable
//! chips that open the underlying note.
//!
//! Tool use: the model can mid-stream invoke `read_note(n)` and
//! `read_transcript(n)` to pull the full body or transcript of a
//! directory entry beyond what we preloaded. We stream-parse SSE,
//! accumulate input_json across deltas per content block, dispatch each
//! tool call, append the result to messages[], and re-POST. Outer loop
//! iterates until the model returns a non-tool stop_reason or we hit
//! `MAX_TOOL_ITERATIONS`.
//!
//! Streaming: SSE events are forwarded to the frontend as `Delta` for
//! text and `ToolUseStart`/`ToolUseDone` for tool calls. Errors at any
//! stage emit a single `Error` event with a user-facing message.

use std::path::PathBuf;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::anthropic::{ANTHROPIC_VERSION, DEFAULT_MODEL, ENDPOINT};
use crate::{index::DirectoryEntry, keychain};

const MAX_TOKENS: u32 = 2048;

/// Top-K retrieved notes whose full body is loaded into the prompt.
const RETRIEVAL_K: usize = 12;
/// Cap on the "all notes directory" entries (title + date + preview).
/// Plenty for personal use; if a user has more, the directory becomes
/// recency-biased — deep matches still surface via retrieval.
const DIRECTORY_CAP: usize = 200;
/// Per-retrieved-note full-body excerpt cap (characters). Keeps the
/// "deep" section of the prompt bounded.
const PER_NOTE_BODY_CAP: usize = 2000;
/// Per-team-member profile excerpt cap (characters).
const PER_PROFILE_CAP: usize = 1500;
/// Per-directory-entry preview cap (characters). The DB already stores
/// a short preview; this is a safety belt.
const PER_PREVIEW_CAP: usize = 200;
/// Per-transcript tool call cap (characters). Transcripts can be tens
/// of thousands of words; we truncate to keep token budget bounded.
const TRANSCRIPT_CHARS_CAP: usize = 3000;
/// Max tool-use round-trips per turn before we force the model to
/// answer with what it has. Guards against runaway tool loops.
const MAX_TOOL_ITERATIONS: u32 = 6;
/// Window for events surfaced in the Schedule section (#64). Past
/// events give context for "what did we last talk about with X";
/// future events for "what's coming up with Y".
const SCHEDULE_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const SCHEDULE_FORWARD_MS: i64 = 14 * 24 * 3600 * 1000;
/// Hard cap on events embedded in the prompt. Recency-prioritized;
/// older events still loadable via `read_event_details` if the model
/// spots a relevant title.
const SCHEDULE_CAP: usize = 50;
/// Per-event description cap when fetched via `read_event_details`.
const EVENT_DESCRIPTION_CAP: usize = 1500;
/// Hard cap on synthesized workstreams embedded in the prompt (#72).
/// Recency-prioritized via `last_activity_ms desc`.
const WORKSTREAM_CAP: usize = 30;
/// Recency window for the `# Recent Teams messages` prompt section (#136).
/// Anything older than this is only reachable via the workstream surface.
const TEAMS_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
/// Hard cap on Teams messages embedded in the prompt section (#136).
/// Sized to comfortably hold ~5 days of typical traffic — empirically
/// the DB has ~40 messages/day for one active user, so 30 was way too
/// tight (covered only ~4 hours and dropped same-day unanswered asks).
/// 200 mirrors `DIRECTORY_CAP`; previews are one line each so the token
/// cost is bounded.
const TEAMS_MESSAGE_CAP: usize = 200;
/// Recency window for the `# Recent emails awaiting attention` section (#137).
const EMAIL_FOLLOWUP_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
/// Cap on emails in the follow-up section (#137). 30 mirrors the issue
/// spec; the noise + bulk filter in `email::list_messages_for_followup`
/// usually returns well under this for typical inboxes (~20-30 for a
/// 500+/14d firehose dominated by automated senders).
const EMAIL_FOLLOWUP_CAP: usize = 30;
/// Sender-volume threshold for the bulk filter (#137). Any sender with
/// `>= N` messages in the window where every message is its own thread
/// (no conversation depth) is treated as automated. 20 catches obvious
/// firehoses (`buchhaltung@` at 125/14d) without nuking a real
/// human/team contact who happens to have sent a lot of one-offs.
const EMAIL_FOLLOWUP_BULK_THRESHOLD: usize = 20;
/// Per-dispatch content cap when storing tool output in `prompt_dumps`
/// (#134). Tool outputs like `read_note` of a long meeting transcript
/// can be 50KB+; truncating in the dump keeps a session's dump rows
/// small while still showing enough context for inspection.
const DISPATCH_CONTENT_CAP: usize = 4096;

/// Names of every tool exposed to the model in `tool_definitions()`.
/// Stored alongside the prompt dump so the inspector can list "what
/// the model could have called" even if it called none. Kept as a
/// const slice rather than re-derived from `tool_definitions()` so the
/// inspector doesn't have to round-trip through serde_json.
const TOOL_NAMES: &[&str] = &[
    "read_note",
    "read_transcript",
    "read_event_details",
    "read_event_series",
    "read_workstream",
    "read_teams_message",
    "read_email",
    "search_similar",
    "read_edges",
];
/// Per-event-series cap on occurrences returned by `read_event_series`
/// (#128). A weekly meeting that's been running 3 years has ~150 rows;
/// 300 leaves headroom for a 6y series at weekly cadence without
/// risking a 1k+-row dump if a connector synced an outlier.
const SERIES_OCCURRENCE_CAP: usize = 300;
/// Threshold for "attendees_present_in_most" on `series_summary`
/// (#128). 50% catches steady members without flagging one-offs.
const SERIES_STEADY_MEMBER_RATIO: f32 = 0.5;

/// Coverage assertion: every `workstreams::signals::registry` kind must
/// have a path into the AI prompt — either a labeled section here in
/// `format_user_message`, or a read-tool in `dispatch_tool` whose output
/// surfaces items of that kind. The Teams-messages miss (a kind in the
/// DB with no path into the model's view) was caught by inspection; the
/// `registry_coverage` test below catches the next one structurally.
///
/// Add a new entry when you ship a new section or a new read-tool that
/// covers a registry kind. Removing an entry without replacing coverage
/// elsewhere will fail the test.
///
/// Kinds with their own labeled `# Section` header in `format_user_message`.
const PROMPT_SECTION_KINDS: &[&str] = &[
    "note",          // `# Notes directory` + `# Top candidates`
    "event",         // `# Schedule (last 14 days, next 14 days)`
    "teams_message", // `# Recent Teams messages (last 14 days)`
    "email",         // `# Recent emails awaiting attention (last 14 days)` (#137)
];
/// Kinds resolvable through a `dispatch_tool` tool-call result. Note that
/// `read_workstream` also surfaces a workstream's bundled `Recent emails`,
/// `Recent meetings`, `Recent notes`, and `Recent Teams messages`
/// (see `format_workstream_detail`) — those kinds remain reachable via
/// that path even when their dedicated tool isn't called.
const TOOL_RESOLVABLE_KINDS: &[&str] = &[
    "note",          // `read_note(n)`
    "event",         // `read_event_details(n)`
    "teams_message", // `read_teams_message(n)` + bundled in `read_workstream`
    "email",         // `read_email(n)` (#137) + bundled in `read_workstream`
];
/// Per-category top-N when expanding a workstream via `read_workstream`
/// (emails / events / notes returned per call).
const WORKSTREAM_DETAIL_TOP_N: usize = 5;
/// Per-workstream summary cap when listed in the prompt section.
const WORKSTREAM_SUMMARY_CAP: usize = 200;
/// Cap on user_notes length when included in any prompt (#77). DB has
/// no cap; this only protects the token budget. Mirrors the same
/// constant in `workstreams::synthesizer` so the two consumers
/// truncate identically.
const USER_NOTES_PROMPT_CAP: usize = 4000;

/// Emitted on the unified `ai-stream` channel. The frontend filters by
/// `turn_id` so a stale stream that arrives after the user navigates
/// away can't corrupt the active conversation.
///
/// Event ordering: one `Sources` first, then any number of `Delta` /
/// `ToolUseStart` / `ToolUseDone` interleaved (in the order the model
/// emits text vs tool calls), then one terminal `Done` or `Error`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StreamEvent {
    Sources {
        turn_id: String,
        sources: Vec<AskSource>,
    },
    Delta {
        turn_id: String,
        text: String,
    },
    /// The model has issued a tool call. Carries enough info for the UI
    /// to render an inline pill ("Reading [3] 'All-hands April'…" or
    /// "Reading event [E2] 'Standup'…").
    ToolUseStart {
        turn_id: String,
        tool_id: String,
        name: String,
        /// 1-based n the tool was called with — preserved for backward
        /// compatibility with frontend code that hasn't migrated to
        /// `target_label` yet. Carries the same value as the integer
        /// portion of `target_label`.
        target_n: u32,
        target_title: String,
        /// Citation label format the UI renders inside `[…]`. Notes:
        /// `"3"` / `"12"`. Events: `"E1"` / `"E14"`.
        target_label: String,
        target_kind: AskSourceKind,
    },
    /// The tool call resolved. `ok=false` on out-of-range / I/O errors;
    /// the model still gets the error text and can recover next turn.
    ToolUseDone {
        turn_id: String,
        tool_id: String,
        ok: bool,
    },
    Done {
        turn_id: String,
    },
    Error {
        turn_id: String,
        message: String,
    },
}

/// Discriminator for citation sources. Notes use `[N]` labels (e.g.
/// `[3]`); events use `[E<N>]` labels (e.g. `[E2]`); workstreams use
/// `[W<N>]` (e.g. `[W2]`). The frontend picks chip styling and click
/// destination based on this.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AskSourceKind {
    Note,
    Event,
    Workstream,
    /// `[T<N>]` — a recent chat message from the Teams connector (#136).
    /// Chip click navigates to the first workstream the message is
    /// attached to (via `workstream_id`); unattached messages are
    /// soft no-ops in v1.
    TeamsMessage,
    /// `[U<N>]` — a recent inbound email surfaced through the noise +
    /// bulk follow-up filter (#137). Chip click navigates to the
    /// message's attached workstream if any (no dedicated email viewer
    /// yet); unattached emails are soft no-ops.
    Email,
}

/// One source the model can cite. The full directory of notes plus
/// the schedule of events is sent up-front; the UI renders chips only
/// for labels the model actually emits, but the entire surface is
/// consistent so out-of-frame citations resolve correctly.
#[derive(Serialize, Clone)]
pub struct AskSource {
    pub kind: AskSourceKind,
    /// Citation label as it appears in `[label]`. Notes carry the
    /// 1-based directory index (e.g. `"3"`); events carry the
    /// E-prefixed index (e.g. `"E2"`).
    pub label: String,
    pub title: String,
    /// For notes: file mtime; for events: start_ms. Lets the frontend
    /// sort the source strip in a sensible order if it needs to.
    pub modified_ms: i64,
    /// Set when `kind == Note`. Frontend opens this path on chip click.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// Set when `kind == Event`. Frontend invokes
    /// `openOrCreateEventNote(event_id)` on chip click (#62).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Set when `kind == Workstream`, or when `kind == TeamsMessage`
    /// and the message is attached to a workstream (so the chip click
    /// can still navigate somewhere — #136). Frontend dispatches a
    /// `margin:open-workstream` event with this id on chip click (#72).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workstream_id: Option<String>,
    /// Set when `kind == TeamsMessage` (#136). Carried for a future
    /// dedicated message-viewer surface; not used by the v1 chip click
    /// (which falls through to `workstream_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams_message_id: Option<String>,
    /// Set when `kind == Email` (#137). Carried so a future dedicated
    /// email viewer can resolve the chip click; v1 chip click falls
    /// through to `workstream_id` when the email is attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_id: Option<String>,
}

/// One past turn in the conversation, threaded back to the model.
/// Frontend only ever stores text content; we wrap it in a single
/// text content block when composing the API request.
#[derive(Deserialize, Clone)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

/// One tool dispatch's input + output, captured for the prompt
/// inspector (#134). `content` is truncated to `DISPATCH_CONTENT_CAP`
/// so a giant `read_note` body doesn't bloat the dump row.
#[derive(Serialize, Clone)]
struct DispatchRecord {
    tool_name: String,
    input: serde_json::Value,
    content: String,
    is_error: bool,
    duration_ms: i64,
}

/// One row from `prompt_dumps`, hydrated for the inspector (#134).
/// `sources` and `dispatches` come back as raw JSON so the frontend
/// reshapes them with its own TS types instead of needing matching
/// serde structs on the Rust side.
#[derive(Serialize)]
pub struct PromptDumpView {
    pub turn_id: String,
    pub prompt: String,
    pub system_prompt: String,
    pub tool_names: Vec<String>,
    pub sources: serde_json::Value,
    pub dispatches: serde_json::Value,
    pub latency_ms: i64,
    pub created_ms: i64,
    pub query: String,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// Prompt-cache write count (#142). NULL on pre-#142 rows + on
    /// turns where caching was inactive.
    pub cache_creation_tokens: Option<i64>,
    /// Prompt-cache read count (#142). Same NULL semantics.
    pub cache_read_tokens: Option<i64>,
}

/// Per-turn telemetry row for the Settings → Diagnostics view (#135).
/// Joins `prompt_dumps` with the assistant `chat_messages` row so the
/// table can render the original query, the model's response length,
/// citations, and counts in one go.
#[derive(Serialize)]
pub struct ChatTurnMetric {
    pub turn_id: String,
    pub conversation_id: Option<String>,
    pub created_ms: i64,
    pub latency_ms: i64,
    pub query: String,
    /// Length of the assistant message; 0 when no row joined (errored
    /// turn or a dump landing without its assistant message).
    pub assistant_text_chars: i64,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// Prompt-cache write count (#142). NULL when caching inactive.
    pub cache_creation_tokens: Option<i64>,
    /// Prompt-cache read count (#142). NULL when caching inactive.
    pub cache_read_tokens: Option<i64>,
    pub sources_total: i64,
    /// `{"note": 200, "event": 50, "workstream": 30, "teams_message": 200}`
    pub sources_by_kind: serde_json::Value,
    /// Labels (`["T1", "E2", ...]`) the model actually emitted in the
    /// assistant text. Parsed via the same regex the frontend uses.
    pub citations: Vec<String>,
    pub tool_call_count: i64,
    pub had_error_dispatch: bool,
}

const SYSTEM_PROMPT: &str = "You are answering questions about the user's personal notes (meeting \
notes, hand-typed notes, transcripts), their team profiles, their calendar, and recent Teams chat \
messages.

The user's message contains up to six sections:

1. **Notes directory** — every non-archived note, labeled `[1]`, `[2]`, etc., with title, date, and \
a short preview. This is the master index; you may cite *any* `[N]` from this directory.

2. **Top candidates** — a subset of the directory whose full bodies have been loaded for deep \
context. The same `[N]` labels apply — these are the same notes, just expanded. When citing details \
that came from a body, cite the directory `[N]`.

3. **Team profiles** — short bios for each colleague: display name, aliases, role, profile text. \
Use these to interpret references to people in the notes (e.g. \"Heike\" maps to a known team \
member). You may cite directly attributable claims from a profile by the person's name in prose; \
profiles aren't `[N]`-citable — only notes, events, and workstreams are.

4. **Schedule** — calendar events from connected Microsoft / Google accounts, labeled `[E1]`, \
`[E2]`, etc., covering the last 14 days and the next 14 days. Each entry: title, time range, \
attendees, location. Cite events with their `[E<N>]` label inline, same shape as note `[N]`s.

5. **Recent Teams messages** — chat messages from connected Microsoft Teams accounts, labeled \
`[T1]`, `[T2]`, etc., covering the last 14 days (capped at 30 most recent). Each entry: sender, \
chat name (when set; 1:1 DMs may have none), send time, one-line preview. Cite messages with \
`[T<N>]` inline. Use these for any question about pings, recent asks, or unanswered messages.

6. **Workstreams** — synthesized clusters of related emails, meetings, and notes labeled `[W1]`, \
`[W2]`, etc. Each entry has a title, one-line summary, and item counts. Workstreams are the right \
citation when the answer IS the ongoing thread itself (\"how's the Hyundai POC going?\"); when \
citing a *specific* item within a workstream, prefer the underlying `[N]` / `[E<N>]` / `[T<N>]` \
label if one is available.

You have seven tools for digging deeper:
- **`read_note(n)`** — returns the full markdown body of directory entry `[n]`. Use when a preview \
hints at relevance but you need the body to answer.
- **`read_transcript(n)`** — returns the meeting transcript text for `[n]`, if it has audio. Use \
when the question is likely about something said in a meeting but not captured in the typed body.
- **`read_event_details(n)`** — returns the full attendee list, description, location, and exact \
times for event `[E<n>]`. When the event belongs to a recurring series, also includes a series \
summary (occurrence count, first/last seen, steady members). Use when answering questions about a \
single meeting's participants or content. Pass the integer after the `E` as `n` (e.g. for `[E3]` \
call `read_event_details(3)`).
- **`read_event_series(n)`** — returns every known occurrence of the recurring series that `[E<n>]` \
belongs to, each with its own attendee list and linked-note pointer. Use for cadence / history \
questions: \"which of the last 4 standups did Alice miss?\", \"tell me about our weekly Bridge \
sync\", \"how often does this meeting actually happen?\". Errors if the event isn't recurring. \
Pass the integer after the `E` as `n` — the dispatcher resolves it to the series master id \
internally.
- **`read_workstream(n)`** — returns the workstream's full summary and the most recent emails / \
events / notes that belong to it. Use for status questions \
(\"what's happening with X?\"). Pass the integer after the `W` as `n` (e.g. for `[W2]` call \
`read_workstream(2)`).
- **`read_teams_message(n)`** — returns the full body of Teams message `[T<n>]` plus a few \
surrounding messages from the same chat (3 before + 1 after, chronological), so you can see the \
thread context around it. Use when a preview hints at an open ask and you need the full text — \
or to confirm whether the user has already replied. Pass the integer after the `T` as `n` (e.g. \
for `[T3]` call `read_teams_message(3)`).
- **`read_email(n)`** — returns the full body of inbound email `[U<n>]` plus the rest of the \
thread it belongs to (chronological), so you can see whether the user already replied and what \
the sender is actually asking. The `# Recent emails awaiting attention` list is pre-filtered to \
drop automated senders and bulk-sender firehoses — but the user's read state may be unreliable \
(many users work through clients that don't sync read status), so don't assume an unread mark means \
they haven't seen it; check the thread instead. Pass the integer after the `U` as `n` (e.g. for \
`[U3]` call `read_email(3)`).

Use tools sparingly — most questions can be answered from the directory + top candidates + schedule \
already in context. Don't speculate; call a tool if you genuinely need the content. Up to 6 tool \
calls per question; after that you must answer with what you have.

Rules:
- Answer in natural prose. Be specific and concise — 1-4 short paragraphs unless the question asks \
for a list.
- Cite sources inline with `[N]` (notes), `[E<N>]` (events), `[W<N>]` (workstreams), \
`[T<N>]` (Teams messages), or `[U<N>]` (emails) immediately after each claim that came from one. \
Multiple citations: `[1][3]` or `[E1][E2]` or `[W2][U1]` or any mix. Never make up citation labels \
— only use ones you actually received.
- For \"when did we first…\" questions, identify the *earliest* dated note that matches and cite it.
- If neither the notes nor the profiles nor the schedule nor the Teams messages contain the \
answer, say so clearly. Don't speculate.
- Don't pad with caveats or restate the question. Open with the answer.
- When the user's question implies recency (\"today\", \"this week\", \"latest\", \"recent\", \"rn\", \
\"right now\", \"currently\", \"now\"), and the most recent source you can cite is older than 7 \
days, explicitly note the gap. For example: \"The most recent message I can see from Heike is 2 \
weeks old; I may be missing newer activity.\" This caveat goes at the END of your answer, not the \
beginning — answer first, then disclose the staleness. Don't fire this caveat when recent sources \
*are* available, or when the question is timeless (\"what is X's role?\", \"how does Y work?\").
- Don't echo note titles back as a heading; cite them inline.";

/// Public entry point — the Tauri command lives in lib.rs and forwards
/// here. The frontend generates `turn_id` so the assistant message can
/// be tagged with it *before* the first `ai-stream` event arrives,
/// avoiding a race where `Sources` got emitted before the listener
/// could associate it with a message.
pub async fn start(
    app: AppHandle,
    turn_id: String,
    query: String,
    history: Vec<ChatTurn>,
    model: Option<String>,
) -> Result<(), String> {
    let key = keychain::read_anthropic_api_key().map_err(|_| {
        "Anthropic API key not configured — open Settings → AI to add one".to_string()
    })?;

    // Pull the all-notes directory + retrieval set + team roster +
    // schedule window + active workstreams + recent Teams messages in
    // one lock. Profile.md content is read off-lock below.
    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let now_ms = current_unix_ms();
    let (
        directory,
        retrieved_paths,
        team,
        schedule,
        workstreams,
        teams_messages,
        teams_attached_workstreams,
        unread_emails,
        email_attached_workstreams,
    ) = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        let directory = crate::index::list_directory(&c, DIRECTORY_CAP)
            .map_err(|e| e.to_string())?;
        let hits = crate::index::retrieve_for_ask(&c, &query, RETRIEVAL_K)
            .map_err(|e| e.to_string())?;
        let retrieved_paths: std::collections::HashSet<String> =
            hits.iter().map(|h| h.note_path.clone()).collect();
        let team = crate::team::list_team_members_raw(&c).unwrap_or_default();
        let mut schedule = crate::connectors::calendar::list_events_in_range(
            &c,
            now_ms - SCHEDULE_BACK_MS,
            now_ms + SCHEDULE_FORWARD_MS,
            None,
        )
        .map_err(|e| e.to_string())?;
        schedule.truncate(SCHEDULE_CAP);
        let mut workstreams =
            crate::workstreams::persist::list_workstreams_active(&c).unwrap_or_default();
        workstreams.truncate(WORKSTREAM_CAP);
        let teams_messages = crate::connectors::teams::list_messages_in_range(
            &c,
            now_ms - TEAMS_WINDOW_BACK_MS,
            now_ms,
            TEAMS_MESSAGE_CAP,
        )
        .unwrap_or_default();
        // One batch lookup: for each Teams message id, the most recent
        // non-tombstoned workstream it's attached to (if any). Powers
        // the chip-click navigation; unattached messages are soft no-ops.
        let teams_attached_workstreams =
            load_teams_message_workstream_map(&c, &teams_messages);
        // #137: surface inbound mail through the noise + bulk filter.
        // `is_read` is unreliable when the user works through a separate
        // client (Front, etc.) that doesn't sync read state back; the
        // filter is sender-shape based instead. Self-email lookup is
        // best-effort — if no team_member is marked `is_self` yet, the
        // filter still works (it just doesn't suppress outbound, which
        // today's connector doesn't sync anyway).
        let self_email = lookup_self_email(&c);
        let unread_emails = crate::connectors::email::list_messages_for_followup(
            &c,
            self_email.as_deref(),
            now_ms - EMAIL_FOLLOWUP_WINDOW_BACK_MS,
            now_ms,
            EMAIL_FOLLOWUP_BULK_THRESHOLD,
            EMAIL_FOLLOWUP_CAP,
        )
        .unwrap_or_default();
        let email_attached_workstreams = load_email_workstream_map(&c, &unread_emails);
        (
            directory,
            retrieved_paths,
            team,
            schedule,
            workstreams,
            teams_messages,
            teams_attached_workstreams,
            unread_emails,
            email_attached_workstreams,
        )
    };

    // Build the citation surface: every directory entry gets a 1-based
    // [N] label, every schedule entry gets an [E<N>] label, every
    // workstream gets a [W<N>] label.
    let mut sources: Vec<AskSource> = Vec::with_capacity(
        directory.len()
            + schedule.len()
            + workstreams.len()
            + teams_messages.len()
            + unread_emails.len(),
    );
    for (i, e) in directory.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::Note,
            label: (i + 1).to_string(),
            note_path: Some(e.note_path.clone()),
            bundle_id: Some(e.bundle_id.clone()),
            event_id: None,
            workstream_id: None,
            teams_message_id: None,
            email_id: None,
            title: e.title.clone(),
            modified_ms: e.modified_ms,
        });
    }
    for (i, e) in schedule.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::Event,
            label: format!("E{}", i + 1),
            note_path: e.linked_note_id.clone(),
            bundle_id: None,
            event_id: Some(e.id.clone()),
            workstream_id: None,
            teams_message_id: None,
            email_id: None,
            title: e.title.clone(),
            modified_ms: e.start_ms,
        });
    }
    for (i, w) in workstreams.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::Workstream,
            label: format!("W{}", i + 1),
            note_path: None,
            bundle_id: None,
            event_id: None,
            workstream_id: Some(w.id.clone()),
            teams_message_id: None,
            email_id: None,
            title: w.title.clone(),
            modified_ms: w.last_activity_ms,
        });
    }
    for (i, m) in teams_messages.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::TeamsMessage,
            label: format!("T{}", i + 1),
            note_path: None,
            bundle_id: None,
            event_id: None,
            workstream_id: teams_attached_workstreams.get(&m.id).cloned(),
            teams_message_id: Some(m.id.clone()),
            email_id: None,
            title: teams_chip_title(m),
            modified_ms: m.sent_at_ms,
        });
    }
    for (i, e) in unread_emails.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::Email,
            label: format!("U{}", i + 1),
            note_path: None,
            bundle_id: None,
            event_id: None,
            workstream_id: email_attached_workstreams.get(&e.id).cloned(),
            teams_message_id: None,
            email_id: Some(e.id.clone()),
            title: email_chip_title(e),
            modified_ms: e.sent_at_ms,
        });
    }

    // Emit sources before the first token. UI renders chips only for
    // [N]s that appear in the streamed answer.
    let _ = app.emit(
        "ai-stream",
        StreamEvent::Sources {
            turn_id: turn_id.clone(),
            sources: sources.clone(),
        },
    );

    if directory.is_empty() && team.is_empty() && history.is_empty() {
        // No notes at all and no team members configured — the model
        // can only refuse. Short-circuit.
        let app_for_emit = app.clone();
        let tid = turn_id.clone();
        tauri::async_runtime::spawn(async move {
            let _ = app_for_emit.emit(
                "ai-stream",
                StreamEvent::Delta {
                    turn_id: tid.clone(),
                    text: "There are no notes or team profiles to search yet."
                        .to_string(),
                },
            );
            let _ = app_for_emit.emit("ai-stream", StreamEvent::Done { turn_id: tid });
        });
        return Ok(());
    }

    // Pull the latest profile snapshot per team member (#107) and
    // render the same multi-line excerpt the prompt has historically
    // consumed. Missing snapshots degrade to an empty excerpt — the
    // model still has display_name + aliases from the directory.
    let snapshot_map = {
        let state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
        let c = state.lock().map_err(|e| e.to_string())?;
        let ids: Vec<&str> = team.iter().map(|m| m.id.as_str()).collect();
        crate::profiles::persist::get_latest_map(&c, &ids).unwrap_or_else(|e| {
            eprintln!("[ask] get_latest_map failed: {e}");
            std::collections::HashMap::new()
        })
    };
    let mut profile_excerpts: Vec<(crate::team::TeamMember, String)> =
        Vec::with_capacity(team.len());
    for m in team {
        let excerpt = snapshot_map
            .get(&m.id)
            .map(|snap| {
                crate::profiles::prompt::render_snapshot_excerpt(
                    &snap.body,
                    PER_PROFILE_CAP,
                )
            })
            .unwrap_or_default();
        profile_excerpts.push((m, excerpt));
    }

    let user_message = format_user_message(
        &query,
        &directory,
        &retrieved_paths,
        &profile_excerpts,
        &schedule,
        &workstreams,
        &teams_messages,
        &unread_emails,
    );

    // Compose `messages[]`: prior history first, then this turn's user
    // message. History rows arrive as plain strings — wrap each in a
    // single text content block. Anything other than user/assistant is
    // dropped so a bad payload can't poison the request.
    let mut messages: Vec<ApiMessage> = Vec::with_capacity(history.len() + 1);
    for h in history {
        if h.role == "user" || h.role == "assistant" {
            messages.push(ApiMessage {
                role: h.role,
                content: vec![ContentBlock::Text {
                    text: h.content,
                    cache_control: None,
                }],
            });
        }
    }
    // Split the assembled user message into a stable context block
    // (cache_control: ephemeral) and a per-turn question block (no
    // marker so a new question doesn't bust the cache prefix). #142.
    // The full string still gets persisted to prompt_dumps for the
    // inspector — splitting only affects request shape.
    let (context_block, question_block) = split_at_question_marker(&user_message);
    let mut user_content: Vec<ContentBlock> = Vec::new();
    user_content.push(ContentBlock::Text {
        text: context_block,
        cache_control: Some(CacheControl { kind: "ephemeral" }),
    });
    if !question_block.is_empty() {
        user_content.push(ContentBlock::Text {
            text: question_block,
            cache_control: None,
        });
    }
    messages.push(ApiMessage {
        role: "user".to_string(),
        content: user_content,
    });

    let model = model.as_deref().unwrap_or(DEFAULT_MODEL).to_string();

    // Spawn the network + tool-use loop. Errors emit an `error` event
    // and exit; success emits deltas + a final `done` event.
    let app_bg = app.clone();
    let turn_id_bg = turn_id.clone();
    let sources_for_dump = sources.clone();
    let query_for_dump = query.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(msg) = run_loop(
            &app_bg,
            &turn_id_bg,
            &key,
            &model,
            messages,
            &directory,
            &schedule,
            &workstreams,
            &teams_messages,
            &unread_emails,
            &user_message,
            &sources_for_dump,
            &query_for_dump,
        )
        .await
        {
            let _ = app_bg.emit(
                "ai-stream",
                StreamEvent::Error {
                    turn_id: turn_id_bg.clone(),
                    message: msg,
                },
            );
        }
    });

    Ok(())
}

#[derive(Serialize, Clone, Copy)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'static str,
    cache_control: CacheControl,
}

/// One content block in a request `messages[]` content array. Mirrors
/// Anthropic's content block schema for assistant `text` / `tool_use`
/// and user `tool_result` blocks. The `type` discriminator is emitted
/// via serde. `cache_control` on `Text` and `ToolResult` blocks is
/// opt-in per #142 — marking a block extends the cache prefix to
/// include everything up to and including it.
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Split an assembled user-message string at the `# Question` marker
/// (#142). Returns `(context, question)`. When the marker is absent
/// the full text is treated as context with an empty question — the
/// request will then send a single content block as before, just
/// with `cache_control` set. `rfind` so the last `# Question`
/// occurrence wins if a user happens to paste one in their query.
fn split_at_question_marker(user_message: &str) -> (String, String) {
    const MARKER: &str = "# Question\n\n";
    match user_message.rfind(MARKER) {
        Some(i) => (
            user_message[..i].to_string(),
            user_message[i..].to_string(),
        ),
        None => (user_message.to_string(), String::new()),
    }
}

#[derive(Serialize, Clone)]
struct ApiMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    system: Vec<SystemBlock>,
    messages: &'a [ApiMessage],
    /// Tool definitions. Empty/None on the final non-tool-allowed call
    /// after we hit the iteration cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a serde_json::Value>,
}

/// Outer loop driving multiple POSTs against /v1/messages until the
/// model returns a non-tool stop_reason or we hit the iteration cap.
async fn run_loop(
    app: &AppHandle,
    turn_id: &str,
    api_key: &str,
    model: &str,
    mut messages: Vec<ApiMessage>,
    directory: &[DirectoryEntry],
    schedule: &[crate::connectors::calendar::CalendarEvent],
    workstreams: &[crate::workstreams::Workstream],
    teams_messages: &[crate::connectors::teams::TeamsMessage],
    unread_emails: &[crate::connectors::email::EmailMessage],
    // Captured for the prompt-inspector dump (#134). The assembled
    // user-message string and the full source surface go straight to
    // `prompt_dumps` at turn end so the UI can show "what did the AI see?"
    // post-hoc. Live streaming doesn't need either. `query` is the raw
    // user question — stored separately from `prompt` so the diagnostics
    // view (#135) can render the original text without parsing it back
    // out of the assembled section dump.
    prompt: &str,
    sources: &[AskSource],
    query: &str,
) -> Result<(), String> {
    let tools = tool_definitions();
    let run_start = std::time::Instant::now();
    let mut dispatches: Vec<DispatchRecord> = Vec::new();
    // Token accumulators (#135). Multi-pass turns re-send the full
    // history each pass so input_tokens double-counts repeated context;
    // it's a directional indicator for the diagnostics view, not a
    // precise billing metric. The UI labels it accordingly. With
    // caching active (#142) most of the per-pass input is served from
    // cache → tokens_in stays small and the cache_* totals carry the
    // bulk; the diagnostics row formats accordingly.
    let mut total_tokens_in: i64 = 0;
    let mut total_tokens_out: i64 = 0;
    let mut total_cache_creation: i64 = 0;
    let mut total_cache_read: i64 = 0;

    for _ in 0..MAX_TOOL_ITERATIONS {
        let body = ApiRequest {
            model,
            max_tokens: MAX_TOKENS,
            stream: true,
            system: vec![SystemBlock {
                kind: "text",
                text: SYSTEM_PROMPT,
                cache_control: CacheControl { kind: "ephemeral" },
            }],
            messages: &messages,
            tools: Some(&tools),
        };

        let pass = stream_pass(app, turn_id, api_key, &body).await?;
        total_tokens_in += pass.tokens_in;
        total_tokens_out += pass.tokens_out;
        total_cache_creation += pass.cache_creation_tokens;
        total_cache_read += pass.cache_read_tokens;

        if pass.pending_tool_calls.is_empty() {
            // No tool calls — the model is done with this turn.
            let _ = app.emit(
                "ai-stream",
                StreamEvent::Done {
                    turn_id: turn_id.to_string(),
                },
            );
            persist_prompt_dump(
                app,
                turn_id,
                prompt,
                sources,
                &dispatches,
                run_start.elapsed().as_millis() as i64,
                query,
                Some(total_tokens_in),
                Some(total_tokens_out),
                Some(total_cache_creation),
                Some(total_cache_read),
            );
            return Ok(());
        }

        // Append the assistant's full response (text + tool_use blocks)
        // and run each tool, accumulating tool_result blocks for the
        // next user message.
        messages.push(ApiMessage {
            role: "assistant".to_string(),
            content: pass.assistant_blocks,
        });

        let pending_total = pass.pending_tool_calls.len();
        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(pending_total);
        for (tc_idx, tc) in pass.pending_tool_calls.into_iter().enumerate() {
            let target_n = tc
                .input
                .get("n")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let idx = target_n.saturating_sub(1) as usize;
            let (target_title, target_label, target_kind) = match tc.name.as_str() {
                "read_event_details" | "read_event_series" => (
                    schedule.get(idx).map(|e| e.title.clone()).unwrap_or_default(),
                    format!("E{}", target_n),
                    AskSourceKind::Event,
                ),
                "read_workstream" => (
                    workstreams
                        .get(idx)
                        .map(|w| w.title.clone())
                        .unwrap_or_default(),
                    format!("W{}", target_n),
                    AskSourceKind::Workstream,
                ),
                "read_email" => (
                    unread_emails
                        .get(idx)
                        .map(|e| {
                            let s = e.subject.trim();
                            if s.is_empty() {
                                e.from_email.clone()
                            } else {
                                s.to_string()
                            }
                        })
                        .unwrap_or_default(),
                    format!("U{}", target_n),
                    AskSourceKind::Email,
                ),
                _ => (
                    directory.get(idx).map(|e| e.title.clone()).unwrap_or_default(),
                    target_n.to_string(),
                    AskSourceKind::Note,
                ),
            };

            let _ = app.emit(
                "ai-stream",
                StreamEvent::ToolUseStart {
                    turn_id: turn_id.to_string(),
                    tool_id: tc.id.clone(),
                    name: tc.name.clone(),
                    target_n,
                    target_title: target_title.clone(),
                    target_label,
                    target_kind,
                },
            );

            let dispatch_start = std::time::Instant::now();
            let result = dispatch_tool(
                app,
                &tc.name,
                &tc.input,
                directory,
                schedule,
                workstreams,
                teams_messages,
                unread_emails,
            );
            let dispatch_duration_ms = dispatch_start.elapsed().as_millis() as i64;

            // Snapshot the dispatch for the prompt-inspector (#134) before
            // moving `result.content` into the tool_result block below.
            dispatches.push(DispatchRecord {
                tool_name: tc.name.clone(),
                input: tc.input.clone(),
                content: truncate_chars(&result.content, DISPATCH_CONTENT_CAP),
                is_error: result.is_error,
                duration_ms: dispatch_duration_ms,
            });

            let _ = app.emit(
                "ai-stream",
                StreamEvent::ToolUseDone {
                    turn_id: turn_id.to_string(),
                    tool_id: tc.id.clone(),
                    ok: !result.is_error,
                },
            );

            // Mark only the last tool_result with cache_control so the
            // request stays under the 4-breakpoint budget while still
            // extending the cache prefix across tool-use passes (#142).
            let is_last_pending = tc_idx + 1 == pending_total;
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: tc.id,
                content: result.content,
                is_error: result.is_error,
                cache_control: if is_last_pending {
                    Some(CacheControl { kind: "ephemeral" })
                } else {
                    None
                },
            });
        }
        messages.push(ApiMessage {
            role: "user".to_string(),
            content: result_blocks,
        });
    }

    // Hit the cap. Force the model to answer with what it has by
    // re-POSTing without tools available.
    let final_body = ApiRequest {
        model,
        max_tokens: MAX_TOKENS,
        stream: true,
        system: vec![SystemBlock {
            kind: "text",
            text: SYSTEM_PROMPT,
            cache_control: CacheControl { kind: "ephemeral" },
        }],
        messages: &messages,
        tools: None,
    };
    let final_pass = stream_pass(app, turn_id, api_key, &final_body).await?;
    total_tokens_in += final_pass.tokens_in;
    total_tokens_out += final_pass.tokens_out;
    total_cache_creation += final_pass.cache_creation_tokens;
    total_cache_read += final_pass.cache_read_tokens;
    let _ = app.emit(
        "ai-stream",
        StreamEvent::Done {
            turn_id: turn_id.to_string(),
        },
    );
    persist_prompt_dump(
        app,
        turn_id,
        prompt,
        sources,
        &dispatches,
        run_start.elapsed().as_millis() as i64,
        query,
        Some(total_tokens_in),
        Some(total_tokens_out),
        Some(total_cache_creation),
        Some(total_cache_read),
    );
    Ok(())
}

/// Static tool definitions. Built once and reused across iterations.
/// Both tools key off the 1-based directory index `n` so the model
/// can reference any source it sees in the prompt.
fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "read_note",
            "description": "Read the full markdown body of a note by its directory index [N]. Use when a directory entry's title or preview suggests it might answer the question and you need its body to be sure. Returns the body or a not-found error.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [N] label from the Notes directory section."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "read_transcript",
            "description": "Read the meeting transcript text for a note by its directory index [N]. Use when the question is likely answered by something said in a meeting (rather than captured in the typed body). Returns the transcript text or a 'no transcript' notice if the note has no audio.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [N] label from the Notes directory section."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "read_event_details",
            "description": "Read the full details of a calendar event by its schedule label [E<N>]. Returns the title, exact start/end times, location, description, and the full attendee list (with response statuses and resolved team_member IDs where Margin knows the person). Use when an event preview hints at relevance but you need attendees or the body. NOTE: the `n` argument is the integer after the `E` (e.g. for `[E3]` pass `n: 3`).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [E<N>] label from the Schedule section."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "read_event_series",
            "description": "Read every known occurrence of the recurring series that event [E<N>] belongs to. Each occurrence includes its own start time, attendee list (with response statuses), and linked-note pointer. Use for cadence and history questions: 'which of the last 4 standups did Alice miss?', 'tell me about our weekly sync', 'how often does this meeting actually happen?'. Errors if [E<N>] is a one-off (not part of a series). NOTE: the `n` argument is the integer after the `E` (e.g. for `[E3]` pass `n: 3`); the dispatcher resolves it to the series master id.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [E<N>] label of any occurrence in the series."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "read_workstream",
            "description": "Read the full details of a workstream by its label [W<N>]. Returns the summary and the most recent emails / events / notes that belong to this workstream. Use when the user is asking about ongoing work, status updates, or 'what's happening with X'. NOTE: the `n` argument is the integer after the `W` (e.g. for `[W3]` pass `n: 3`).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [W<N>] label from the Workstreams section."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "read_teams_message",
            "description": "Read the full body of a Teams message by its [T<N>] label, plus the 3 messages immediately before and 1 immediately after it in the same chat — for conversational context. Use when a preview hints at an open ask or you need to confirm whether the user has already replied. NOTE: the `n` argument is the integer after the `T` (e.g. for `[T3]` pass `n: 3`).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [T<N>] label from the Recent Teams messages section."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "read_email",
            "description": "Read the full body of an inbound email by its [U<N>] label, plus the rest of the thread it belongs to (chronological). Use when a preview hints at an open ask or you need to see whether the user has already replied. The `is_read` flag in the underlying mail store is unreliable for users who work through clients (Front, etc.) that don't sync read state — so don't infer 'not yet read' from the absence of a read mark; check the thread for an outbound reply instead. NOTE: the `n` argument is the integer after the `U` (e.g. for `[U3]` pass `n: 3`).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based [U<N>] label from the Recent emails awaiting attention section."
                    }
                },
                "required": ["n"]
            }
        },
        {
            "name": "search_similar",
            "description": "Search the user's content semantically (via the Voyage embedding index, #104). Use this for questions like 'what was I working on around X', 'who said anything about Y last month', 'remind me what we decided about Z' — where keyword search would miss the answer because the user's wording differs from the original. Returns up to `limit` hits across notes, emails, calendar events, and workstreams, ranked by cosine similarity to the query.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language query."
                    },
                    "kinds": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["note","email","event","workstream","teams_message"]
                        },
                        "description": "Optional. Restrict results to a subset of entity kinds."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Max hits to return. Default 10."
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "read_edges",
            "description": "Retrieve the 1-hop graph neighborhood of a node. Returns every edge whose source OR target is the given node, with the relationship kind, confidence, and the other side's display label. Use to discover relationships ('who attended this meeting', 'who is mentioned in this note', 'which workstreams include this email'). The graph is populated by the deterministic edge synthesizer (#103); current edge kinds are AUTHORED, REPLIED_TO, MENTIONED, CO_ATTENDED, ATTENDED, INCLUDES.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "node_kind": {
                        "type": "string",
                        "enum": ["person", "event", "note", "email", "workstream"],
                        "description": "Entity kind of the node to look up."
                    },
                    "node_id": {
                        "type": "string",
                        "description": "Canonical id of the node. For person: team_members.id. For event: calendar_events.id. For note: note_path. For email: email_messages.id. For workstream: workstreams.id."
                    }
                },
                "required": ["node_kind", "node_id"]
            }
        }
    ])
}

/// Result of one streaming round-trip.
struct PassResult {
    assistant_blocks: Vec<ContentBlock>,
    pending_tool_calls: Vec<PendingToolCall>,
    /// `usage.input_tokens` from the SSE `message_start` event (#135).
    /// One pass = one HTTP request, so this is the input cost for that
    /// specific request. `run_loop` sums across passes. With caching
    /// enabled (#142), this is the count NOT served from cache.
    tokens_in: i64,
    /// `usage.output_tokens` from the SSE `message_delta` event (#135).
    /// Anthropic streams cumulative counts; the last value wins.
    tokens_out: i64,
    /// `usage.cache_creation_input_tokens` — tokens *written* to the
    /// cache on this request (1.25× billed). Absent on a miss → 0 (#142).
    cache_creation_tokens: i64,
    /// `usage.cache_read_input_tokens` — tokens *read* from the cache
    /// (0.1× billed). Absent before any cache exists → 0 (#142).
    cache_read_tokens: i64,
}

struct PendingToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// Per-content-block scratchpad while the model streams. Anthropic
/// emits `content_block_start` then any number of deltas then
/// `content_block_stop` per block, identified by `index`.
enum BlockState {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
}

async fn stream_pass(
    app: &AppHandle,
    turn_id: &str,
    api_key: &str,
    body: &ApiRequest<'_>,
) -> Result<PassResult, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(ENDPOINT)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let raw = resp.text().await.unwrap_or_default();
        let msg = match status.as_u16() {
            401 => format!("Invalid Anthropic API key — check Settings → AI ({raw})"),
            429 => "Rate limited by Anthropic — try again shortly".to_string(),
            _ => format!("Anthropic returned {status}: {raw}"),
        };
        return Err(msg);
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut blocks: std::collections::BTreeMap<u64, BlockState> =
        std::collections::BTreeMap::new();
    // Token counters for the prompt-inspector telemetry view (#135).
    // input_tokens comes once on message_start; output_tokens streams
    // cumulatively on message_delta so the last value is the final.
    let mut tokens_in: i64 = 0;
    let mut tokens_out: i64 = 0;
    // #142 cache telemetry: present on `message_start.usage` only when
    // caching is active for this request. Absent → 0.
    let mut cache_creation_tokens: i64 = 0;
    let mut cache_read_tokens: i64 = 0;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| format!("stream chunk: {e}"))?;
        let s = std::str::from_utf8(&bytes).map_err(|e| format!("stream utf8: {e}"))?;
        buf.push_str(s);

        while let Some(boundary) = find_event_boundary(&buf) {
            let event_block: String = buf.drain(..boundary).collect();
            // Drop the trailing `\n\n` (or `\r\n\r\n`) the boundary
            // marker pointed at.
            buf.drain(..buf.len().min(2));

            let payload = match data_payload(&event_block) {
                Some(p) => p,
                None => continue,
            };
            if payload == "[DONE]" {
                continue;
            }
            let parsed: serde_json::Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[ask] sse parse error ({e}); payload: {payload}");
                    continue;
                }
            };
            let kind = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match kind {
                "content_block_start" => {
                    let index = parsed
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cb = parsed.get("content_block");
                    let cb_type = cb
                        .and_then(|c| c.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if cb_type == "text" {
                        blocks.insert(index, BlockState::Text { text: String::new() });
                    } else if cb_type == "tool_use" {
                        let id = cb
                            .and_then(|c| c.get("id"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = cb
                            .and_then(|c| c.get("name"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        blocks.insert(
                            index,
                            BlockState::ToolUse {
                                id,
                                name,
                                json_buf: String::new(),
                            },
                        );
                    }
                }
                "content_block_delta" => {
                    let index = parsed
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let delta = parsed.get("delta");
                    let dtype = delta
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if dtype == "text_delta" {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            if let Some(BlockState::Text { text: t }) =
                                blocks.get_mut(&index)
                            {
                                t.push_str(text);
                            }
                            let _ = app.emit(
                                "ai-stream",
                                StreamEvent::Delta {
                                    turn_id: turn_id.to_string(),
                                    text: text.to_string(),
                                },
                            );
                        }
                    } else if dtype == "input_json_delta" {
                        let pj = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        if let Some(BlockState::ToolUse { json_buf, .. }) =
                            blocks.get_mut(&index)
                        {
                            json_buf.push_str(pj);
                        }
                    }
                }
                "content_block_stop" => {
                    // Block boundaries are tracked by `blocks`; nothing
                    // to do here. The accumulated state finalizes once
                    // the stream ends.
                }
                "message_start" => {
                    // Initial usage payload: `input_tokens` is fixed for
                    // this request (no output yet). #135 telemetry. Also
                    // carries cache_* fields when caching is active — see
                    // #142. Absent → defaults to 0.
                    if let Some(usage) = parsed
                        .get("message")
                        .and_then(|m| m.get("usage"))
                    {
                        if let Some(t) = usage.get("input_tokens").and_then(|n| n.as_i64()) {
                            tokens_in = t;
                        }
                        if let Some(t) = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|n| n.as_i64())
                        {
                            cache_creation_tokens = t;
                        }
                        if let Some(t) = usage
                            .get("cache_read_input_tokens")
                            .and_then(|n| n.as_i64())
                        {
                            cache_read_tokens = t;
                        }
                    }
                }
                "message_delta" => {
                    // Carries `stop_reason` on the final delta. Also
                    // carries cumulative `usage.output_tokens` — last
                    // value wins for #135 telemetry.
                    if let Some(t) = parsed
                        .get("usage")
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(|n| n.as_i64())
                    {
                        tokens_out = t;
                    }
                }
                "message_stop" => {
                    // End of this pass; further chunks (if any) won't
                    // contain new events. The stream will close naturally.
                }
                "error" => {
                    let msg = parsed
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Anthropic streaming error")
                        .to_string();
                    return Err(msg);
                }
                _ => {} // ping, etc.
            }
        }
    }

    // Finalize: walk blocks in index order, build assistant_blocks +
    // pending_tool_calls in the order the model emitted them. This
    // ordering matters — Anthropic requires assistant tool_use blocks
    // to be paired with user tool_result blocks in the same sequence.
    let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
    let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();
    for (_, state) in blocks {
        match state {
            BlockState::Text { text } => {
                if !text.is_empty() {
                    assistant_blocks.push(ContentBlock::Text {
                        text,
                        cache_control: None,
                    });
                }
            }
            BlockState::ToolUse { id, name, json_buf } => {
                let input: serde_json::Value = if json_buf.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&json_buf).unwrap_or_else(|_| {
                        serde_json::json!({})
                    })
                };
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                pending_tool_calls.push(PendingToolCall { id, name, input });
            }
        }
    }

    Ok(PassResult {
        assistant_blocks,
        pending_tool_calls,
        tokens_in,
        tokens_out,
        cache_creation_tokens,
        cache_read_tokens,
    })
}

/// Find the first `\n\n` (or `\r\n\r\n`) that ends an SSE event block.
/// Returns the byte index of the start of the blank-line terminator.
fn find_event_boundary(s: &str) -> Option<usize> {
    if let Some(i) = s.find("\r\n\r\n") {
        return Some(i);
    }
    s.find("\n\n")
}

/// Extract the SSE `data:` line(s) from one event block. Multi-line
/// data is concatenated with `\n` per the SSE spec, but Anthropic's
/// stream uses single-line `data:` payloads in practice.
fn data_payload(block: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let trimmed = rest.trim_start();
            match &mut out {
                Some(acc) => {
                    acc.push('\n');
                    acc.push_str(trimmed);
                }
                None => out = Some(trimmed.to_string()),
            }
        }
    }
    out
}

struct ToolResult {
    content: String,
    is_error: bool,
}

/// Resolve a tool call. Both tools key off the directory index `n`.
/// Errors (out-of-range, missing files, parse failures) become
/// `is_error: true` results so the model sees them and can recover
/// rather than the whole turn aborting.
fn dispatch_tool(
    app: &AppHandle,
    name: &str,
    input: &serde_json::Value,
    directory: &[DirectoryEntry],
    schedule: &[crate::connectors::calendar::CalendarEvent],
    workstreams: &[crate::workstreams::Workstream],
    teams_messages: &[crate::connectors::teams::TeamsMessage],
    unread_emails: &[crate::connectors::email::EmailMessage],
) -> ToolResult {
    // read_edges takes (node_kind, node_id) strings, not the `n` index
    // every other tool uses. Handle it before the `n` validation.
    if name == "read_edges" {
        return dispatch_read_edges(app, input);
    }
    // search_similar — natural-language query + optional filters (#104).
    // Also non-`n`-indexed; dispatched via block_on so we keep the
    // tool-dispatch surface synchronous (called from inside the streaming
    // loop which is already on a Tokio runtime).
    if name == "search_similar" {
        return dispatch_search_similar(app, input);
    }

    let n = match input.get("n").and_then(|v| v.as_u64()) {
        Some(v) if v >= 1 => v as usize,
        _ => {
            return ToolResult {
                content: "Tool input missing or invalid required field 'n' (must be a 1-based index).".to_string(),
                is_error: true,
            };
        }
    };

    // read_event_details indexes the schedule, not the notes directory.
    // Handled before the notes-directory bounds check so an out-of-range
    // event call gives the model a useful error.
    if name == "read_event_details" {
        return dispatch_read_event_details(app, n, schedule);
    }
    // read_event_series indexes the schedule as well; the dispatcher
    // resolves [En] → series_master_id and loads every occurrence
    // (#128). Needs the conn for the series fetch.
    if name == "read_event_series" {
        return dispatch_read_event_series(app, n, schedule);
    }
    // read_workstream indexes the workstreams slice; needs the conn
    // mutex to load the joined detail.
    if name == "read_workstream" {
        return dispatch_read_workstream(app, n, workstreams);
    }
    // read_teams_message indexes the Teams-messages slice (#136). Bounds
    // check + chat-context load happen inside the dispatcher.
    if name == "read_teams_message" {
        return dispatch_read_teams_message(app, n, teams_messages);
    }
    // read_email indexes the inbound-email follow-up slice (#137).
    if name == "read_email" {
        return dispatch_read_email(app, n, unread_emails);
    }

    if n > directory.len() {
        return ToolResult {
            content: format!(
                "[{n}] is out of range. Notes directory has {len} entries — valid range is [1]..[{len}].",
                n = n,
                len = directory.len()
            ),
            is_error: true,
        };
    }
    let entry = &directory[n - 1];

    match name {
        "read_note" => {
            let body = read_note_body(&PathBuf::from(&entry.note_path));
            let content = if body.is_empty() {
                format!("[{n}] '{title}' is empty.", n = n, title = entry.title.trim())
            } else {
                format!(
                    "# [{n}] {title}\n\n{body}",
                    n = n,
                    title = entry.title.trim(),
                    body = body
                )
            };
            ToolResult {
                content,
                is_error: false,
            }
        }
        "read_transcript" => {
            let bundle_dir = match PathBuf::from(&entry.note_path).parent() {
                Some(p) => p.to_path_buf(),
                None => {
                    return ToolResult {
                        content: format!("[{n}] '{}' has no resolvable bundle directory.", entry.title.trim()),
                        is_error: true,
                    };
                }
            };
            let transcript_path = bundle_dir.join(crate::notes::TRANSCRIPT_FILENAME);
            if !transcript_path.exists() {
                return ToolResult {
                    content: format!(
                        "No transcript available for [{n}] '{title}' — this note has no audio recording.",
                        n = n,
                        title = entry.title.trim()
                    ),
                    is_error: false,
                };
            }
            let raw = match std::fs::read_to_string(&transcript_path) {
                Ok(s) => s,
                Err(e) => {
                    return ToolResult {
                        content: format!("Failed to read transcript for [{n}]: {e}"),
                        is_error: true,
                    };
                }
            };
            let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult {
                        content: format!("Transcript JSON for [{n}] is malformed: {e}"),
                        is_error: true,
                    };
                }
            };
            let mut text = String::new();
            if let Some(segs) = parsed.get("segments").and_then(|s| s.as_array()) {
                for seg in segs {
                    if let Some(t) = seg.get("text").and_then(|t| t.as_str()) {
                        let trimmed = t.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(trimmed);
                    }
                }
            }
            let truncated = truncate_chars(&text, TRANSCRIPT_CHARS_CAP);
            ToolResult {
                content: format!(
                    "# Transcript for [{n}] {title}\n\n{body}",
                    n = n,
                    title = entry.title.trim(),
                    body = truncated
                ),
                is_error: false,
            }
        }
        _ => ToolResult {
            content: format!("Unknown tool: {name}. Available tools: read_note, read_transcript, read_event_details, read_workstream."),
            is_error: true,
        },
    }
}

/// Format a single workstream into a structured text block the model
/// can quote from. Includes the summary and the top-N most recent items
/// per category.
fn dispatch_read_workstream(
    app: &AppHandle,
    n: usize,
    workstreams: &[crate::workstreams::Workstream],
) -> ToolResult {
    if n > workstreams.len() {
        return ToolResult {
            content: format!(
                "[W{n}] is out of range. Workstreams section has {len} entries — valid range is [W1]..[W{len}].",
                n = n,
                len = workstreams.len()
            ),
            is_error: true,
        };
    }
    let ws = &workstreams[n - 1];
    let label = format!("W{n}");

    let (detail, team_by_id) = {
        let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
        let c = match conn_state.lock() {
            Ok(g) => g,
            Err(e) => {
                return ToolResult {
                    content: format!("[{label}] could not be loaded — db lock: {e}"),
                    is_error: true,
                };
            }
        };
        let detail = match crate::workstreams::persist::get_workstream_detail(&c, &ws.id) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return ToolResult {
                    content: format!(
                        "[{label}] '{title}' was found in the index but has no detail row — it may have been archived between this turn and the last sync.",
                        title = ws.title.trim()
                    ),
                    is_error: false,
                };
            }
            Err(e) => {
                return ToolResult {
                    content: format!("[{label}] could not be loaded: {e}"),
                    is_error: true,
                };
            }
        };
        let team = crate::team::list_team_members_raw(&c).unwrap_or_default();
        let team_by_id: std::collections::HashMap<String, String> = team
            .into_iter()
            .map(|m| (m.id, m.display_name))
            .collect();
        (detail, team_by_id)
    };

    ToolResult {
        content: format_workstream_detail(&label, &detail, &team_by_id),
        is_error: false,
    }
}

/// 1-hop graph neighborhood for a node (#103). Walks the `edges` table
/// for any row whose source OR target matches `(node_kind, node_id)`,
/// joins the other side back to a display label, and formats the
/// result grouped by edge_kind. Output is markdown bullets so it pastes
/// cleanly into the assistant's reply context.
fn dispatch_read_edges(app: &AppHandle, input: &serde_json::Value) -> ToolResult {
    let node_kind = match input.get("node_kind").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return ToolResult {
                content: "read_edges: missing or empty `node_kind`.".into(),
                is_error: true,
            };
        }
    };
    let node_id = match input.get("node_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return ToolResult {
                content: "read_edges: missing or empty `node_id`.".into(),
                is_error: true,
            };
        }
    };

    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let c = match conn_state.lock() {
        Ok(g) => g,
        Err(e) => {
            return ToolResult {
                content: format!("read_edges: db lock: {e}"),
                is_error: true,
            };
        }
    };

    let mut rows: Vec<EdgeRow> = Vec::new();
    let sql = "SELECT src_kind, src_id, tgt_kind, tgt_id, edge_kind, confidence, last_seen_ms \
               FROM edges \
               WHERE (src_kind = ?1 AND src_id = ?2) \
                  OR (tgt_kind = ?1 AND tgt_id = ?2) \
               ORDER BY last_seen_ms DESC \
               LIMIT 200";
    let mut stmt = match c.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            return ToolResult {
                content: format!("read_edges: prepare: {e}"),
                is_error: true,
            };
        }
    };
    let qrows = stmt.query_map(rusqlite::params![&node_kind, &node_id], |r| {
        Ok(EdgeRow {
            src_kind: r.get(0)?,
            src_id: r.get(1)?,
            tgt_kind: r.get(2)?,
            tgt_id: r.get(3)?,
            edge_kind: r.get(4)?,
            confidence: r.get(5)?,
            last_seen_ms: r.get(6)?,
        })
    });
    match qrows {
        Ok(iter) => {
            for r in iter.flatten() {
                rows.push(r);
            }
        }
        Err(e) => {
            return ToolResult {
                content: format!("read_edges: query: {e}"),
                is_error: true,
            };
        }
    }

    if rows.is_empty() {
        return ToolResult {
            content: format!(
                "No edges found for ({kind}, {id}). The edge synthesizer (#103) may not have run yet, \
                 or this node has no resolvable relationships.",
                kind = node_kind,
                id = node_id
            ),
            is_error: false,
        };
    }

    // Resolve other-side display labels in batches per kind.
    let labels = resolve_edge_labels(&c, &rows);

    let mut out = String::new();
    out.push_str(&format!(
        "# Edges for ({kind}, {id}) — {n} rows\n\n",
        kind = node_kind,
        id = node_id,
        n = rows.len()
    ));

    // Group by edge_kind in stable order, then by direction.
    let mut grouped: std::collections::BTreeMap<&str, Vec<&EdgeRow>> =
        std::collections::BTreeMap::new();
    for r in &rows {
        grouped.entry(r.edge_kind.as_str()).or_default().push(r);
    }
    for (kind, group) in grouped {
        out.push_str(&format!("## {kind} ({})\n", group.len()));
        for r in group {
            let is_outgoing = r.src_kind == node_kind && r.src_id == node_id;
            let (other_kind, other_id, arrow) = if is_outgoing {
                (r.tgt_kind.as_str(), r.tgt_id.as_str(), "→")
            } else {
                (r.src_kind.as_str(), r.src_id.as_str(), "←")
            };
            let label = labels
                .get(&(other_kind.to_string(), other_id.to_string()))
                .map(String::as_str)
                .unwrap_or(other_id);
            out.push_str(&format!(
                "- {arrow} [{other_kind}] {label} (conf {conf:.2})\n",
                arrow = arrow,
                other_kind = other_kind,
                label = label,
                conf = r.confidence
            ));
        }
        out.push('\n');
    }

    ToolResult {
        content: out,
        is_error: false,
    }
}

/// Semantic retrieval entry point for the assistant (#104). Calls into
/// `embeddings::retrieve`, formats hits as markdown for the model to
/// quote from. Synchronously wraps the async helper via `block_on`
/// inside Tauri's runtime — the dispatch_tool surface is sync to keep
/// the streaming-loop bookkeeping simple.
fn dispatch_search_similar(app: &AppHandle, input: &serde_json::Value) -> ToolResult {
    let query = match input.get("query").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            return ToolResult {
                content: "search_similar: missing or empty `query`.".into(),
                is_error: true,
            };
        }
    };
    let kinds: Option<Vec<String>> = input.get("kinds").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|el| el.as_str().map(|s| s.to_string()))
            .collect()
    });
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(10);

    let opts = crate::embeddings::RetrieveOpts { kinds, limit };
    let app_clone = app.clone();
    let query_for_call = query.clone();
    // `dispatch_tool` is sync but is called from inside `run_loop`,
    // which is async (i.e. we're on a tokio worker). A bare `block_on`
    // here panics with "Cannot start a runtime from within a runtime".
    // `block_in_place` parks this worker so other tasks keep
    // progressing, then nested `block_on` is safe.
    let result = tokio::task::block_in_place(move || {
        tauri::async_runtime::block_on(async move {
            crate::embeddings::retrieve(&app_clone, &query_for_call, opts).await
        })
    });

    match result {
        Ok(hits) => {
            if hits.is_empty() {
                return ToolResult {
                    content: format!(
                        "No semantic hits for \"{query}\". The embedding index may not have caught up to recent content yet (worker ticks every 15s)."
                    ),
                    is_error: false,
                };
            }
            let mut out = format!(
                "# Top {} semantic hits for \"{query}\"\n\n",
                hits.len()
            );
            for (i, h) in hits.iter().enumerate() {
                out.push_str(&format!(
                    "{idx}. [{kind}] {preview}  _(distance {dist:.3}, id `{id}`)_\n",
                    idx = i + 1,
                    kind = h.ref_kind,
                    preview = h.preview,
                    dist = h.distance,
                    id = h.ref_id
                ));
            }
            ToolResult {
                content: out,
                is_error: false,
            }
        }
        Err(e) => ToolResult {
            content: format!("search_similar failed: {e}"),
            is_error: true,
        },
    }
}

struct EdgeRow {
    src_kind: String,
    src_id: String,
    tgt_kind: String,
    tgt_id: String,
    edge_kind: String,
    confidence: f64,
    last_seen_ms: i64,
}

/// Resolve `(kind, id)` pairs from the other side of each edge into a
/// short human-readable label. Looks up:
/// - person → team_members.display_name
/// - event → calendar_events.title
/// - note → notes.title
/// - email → email_messages.subject
/// - workstream → workstreams.title
/// Missing rows fall back to the raw id at render time.
fn resolve_edge_labels(
    conn: &rusqlite::Connection,
    rows: &[EdgeRow],
) -> std::collections::HashMap<(String, String), String> {
    use std::collections::HashMap;
    let mut by_kind: HashMap<&'static str, Vec<String>> = HashMap::new();
    for r in rows {
        by_kind.entry(map_kind(&r.src_kind)).or_default().push(r.src_id.clone());
        by_kind.entry(map_kind(&r.tgt_kind)).or_default().push(r.tgt_id.clone());
    }
    let mut out: HashMap<(String, String), String> = HashMap::new();
    for (k, ids) in by_kind {
        if k.is_empty() || ids.is_empty() {
            continue;
        }
        let (sql, kind_str) = match k {
            "person" => (
                "SELECT id, display_name FROM team_members WHERE id = ?1",
                "person",
            ),
            "event" => ("SELECT id, title FROM calendar_events WHERE id = ?1", "event"),
            "note" => ("SELECT id, title FROM notes WHERE id = ?1", "note"),
            "email" => (
                "SELECT id, subject FROM email_messages WHERE id = ?1",
                "email",
            ),
            "workstream" => (
                "SELECT id, title FROM workstreams WHERE id = ?1",
                "workstream",
            ),
            _ => continue,
        };
        if let Ok(mut stmt) = conn.prepare(sql) {
            for id in &ids {
                if let Ok((rid, label)) = stmt.query_row(rusqlite::params![id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }) {
                    out.insert((kind_str.into(), rid), trim_label(&label));
                }
            }
        }
    }
    out
}

fn map_kind(k: &str) -> &'static str {
    match k {
        "person" => "person",
        "event" => "event",
        "note" => "note",
        "email" => "email",
        "workstream" => "workstream",
        _ => "",
    }
}

fn trim_label(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= 80 {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(77).collect();
        format!("{cut}…")
    }
}

fn format_workstream_detail(
    label: &str,
    detail: &crate::workstreams::WorkstreamDetail,
    team_by_id: &std::collections::HashMap<String, String>,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# [{label}] {title}\n",
        label = label,
        title = detail.workstream.title.trim()
    ));
    if !detail.workstream.summary.is_empty() {
        s.push_str(&format!("\n{}\n", detail.workstream.summary.trim()));
    }

    // Owner + members (#81). Both are user/derived data — show before
    // user_notes so the model has the people first.
    if let Some(owner_id) = detail.workstream.owner_member_id.as_deref() {
        if let Some(name) = team_by_id.get(owner_id) {
            s.push_str(&format!("\nOwner: {name}\n"));
        }
    }
    if !detail.workstream.members.is_empty() {
        let names: Vec<&str> = detail
            .workstream
            .members
            .iter()
            .filter_map(|id| team_by_id.get(id).map(String::as_str))
            .collect();
        if !names.is_empty() {
            s.push_str(&format!("Members: {}\n", names.join(", ")));
        }
    }

    // External participants (#?) — email addresses on the workstream's
    // emails / events that don't resolve to a team_member. Lets the
    // model answer "who else is on Bridge?" without us having to
    // re-render every email. Cap mirrors the per-workstream cap in
    // `attach_external_participants`; no truncation suffix because
    // that cap already bounds the list.
    if !detail.workstream.external_participants.is_empty() {
        let externals: Vec<String> = detail
            .workstream
            .external_participants
            .iter()
            .map(|p| match p.display_name.as_deref().filter(|n| !n.trim().is_empty()) {
                Some(name) => format!("{name} <{}>", p.email),
                None => p.email.clone(),
            })
            .collect();
        s.push_str(&format!("External: {}\n", externals.join(", ")));
    }

    // User-authored notes are ground truth (#77). Surface in full near
    // the top so the model reads them before reasoning about the
    // synthesized summary or any inferred state.
    if let Some(notes) = detail
        .workstream
        .user_notes
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        s.push_str("\nUser notes (ground truth):\n");
        s.push_str(&truncate_chars(notes.trim(), USER_NOTES_PROMPT_CAP));
        s.push('\n');
    }

    // User-curated external links (#88). Markdown link syntax so Claude
    // can cite them naturally ("the repo for X is at github.com/…").
    // The optional kind tag is a hint, not a constraint, so it goes in
    // parens after the link.
    if !detail.links.is_empty() {
        s.push_str("\n## Links\n\n");
        for link in &detail.links {
            let label = link.label.trim();
            let url = link.url.trim();
            let kind_suffix = match link
                .kind
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
            {
                Some(kind) => format!(" ({kind})"),
                None => String::new(),
            };
            // Append the AI-generated summary (#?) when populated. The
            // chip view shows the same text inline; including it here
            // means "what does this link describe?" questions land
            // without an extra tool call.
            let summary_suffix = match link
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(summary) => format!(" — {summary}"),
                None => String::new(),
            };
            s.push_str(&format!(
                "- [{label}]({url}){kind_suffix}{summary_suffix}\n"
            ));
        }
    }

    if !detail.emails.is_empty() {
        s.push_str(&format!(
            "\nRecent emails (top {n} of {total}):\n",
            n = detail.emails.len().min(WORKSTREAM_DETAIL_TOP_N),
            total = detail.emails.len()
        ));
        for m in detail.emails.iter().take(WORKSTREAM_DETAIL_TOP_N) {
            let date = format_date(m.sent_at_ms);
            let from = m
                .from_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(m.from_email.as_str());
            let preview = m
                .body_preview
                .as_deref()
                .map(preview_one_line)
                .unwrap_or_default();
            let preview_suffix = if preview.is_empty() {
                String::new()
            } else {
                format!(" — {preview}")
            };
            s.push_str(&format!(
                "- {date} {from} — {subject}{preview_suffix}\n",
                date = date,
                from = from,
                subject = m.subject.trim(),
                preview_suffix = preview_suffix
            ));
        }
    }

    if !detail.events.is_empty() {
        s.push_str(&format!(
            "\nRecent meetings (top {n} of {total}):\n",
            n = detail.events.len().min(WORKSTREAM_DETAIL_TOP_N),
            total = detail.events.len()
        ));
        for e in detail.events.iter().take(WORKSTREAM_DETAIL_TOP_N) {
            let when = format_date(e.start_ms);
            let attendees: Vec<&str> =
                e.attendees.iter().take(5).map(|a| a.email.as_str()).collect();
            let attendees_suffix = if attendees.is_empty() {
                String::new()
            } else {
                format!(" — {}", attendees.join(", "))
            };
            s.push_str(&format!(
                "- {when} {title}{attendees_suffix}\n",
                when = when,
                title = e.title.trim(),
                attendees_suffix = attendees_suffix
            ));
        }
    }

    if !detail.notes.is_empty() {
        s.push_str(&format!(
            "\nRecent notes (top {n} of {total}):\n",
            n = detail.notes.len().min(WORKSTREAM_DETAIL_TOP_N),
            total = detail.notes.len()
        ));
        for n_ref in detail.notes.iter().take(WORKSTREAM_DETAIL_TOP_N) {
            let date = format_date(n_ref.modified_ms);
            let title = if n_ref.title.is_empty() {
                n_ref.note_path.as_str()
            } else {
                n_ref.title.as_str()
            };
            s.push_str(&format!(
                "- {date} {title}\n",
                date = date,
                title = title.trim()
            ));
        }
    }

    // Children rollup (#89). Title + summary per child, no per-child
    // hydration. Lets the model answer "how's [Bridge] going?" with a
    // parent-level summary plus status across each sub-thread without
    // growing the prompt by a full WorkstreamDetail per child.
    if !detail.children.is_empty() {
        s.push_str("\n## Children\n\n");
        for child in &detail.children {
            let summary = truncate_chars(child.summary.trim(), 200);
            s.push_str(&format!(
                "- [{id}] {title} — {summary}\n",
                id = child.id,
                title = child.title.trim(),
            ));
        }
    }

    s
}

/// Format a single calendar event into a structured text block the
/// model can quote from. Includes attendees with response statuses,
/// the linked-note pointer (if any), and a truncated description.
fn dispatch_read_event_details(
    app: &AppHandle,
    n: usize,
    schedule: &[crate::connectors::calendar::CalendarEvent],
) -> ToolResult {
    if n > schedule.len() {
        return ToolResult {
            content: format!(
                "[E{n}] is out of range. Schedule has {len} entries — valid range is [E1]..[E{len}].",
                n = n,
                len = schedule.len()
            ),
            is_error: true,
        };
    }
    let event = &schedule[n - 1];
    let label = format!("E{n}");
    let mut s = String::new();
    s.push_str(&format!(
        "# [{label}] {title}\n",
        label = label,
        title = event.title.trim(),
    ));
    s.push_str(&format!(
        "When: {}\n",
        format_dt_range(event.start_ms, event.end_ms, event.all_day)
    ));
    if let Some(loc) = event.location.as_deref().filter(|l| !l.is_empty()) {
        s.push_str(&format!("Location: {loc}\n"));
    }
    s.push_str(&format!("Source: {}\n", event.connector_id));
    if let Some(status) = event.status.as_deref().filter(|x| !x.is_empty()) {
        s.push_str(&format!("Status: {status}\n"));
    }
    s.push_str(&format!(
        "Linked note: {}\n",
        event
            .linked_note_id
            .as_deref()
            .unwrap_or("(none yet)")
    ));

    s.push_str("\nAttendees:\n");
    if event.attendees.is_empty() {
        s.push_str("- _(none)_\n");
    } else {
        for a in &event.attendees {
            let name = a
                .display_name
                .as_deref()
                .filter(|x| !x.is_empty())
                .unwrap_or(&a.email);
            let mut tags: Vec<String> = Vec::new();
            if a.is_organizer {
                tags.push("organizer".to_string());
            }
            if a.is_self {
                tags.push("self".to_string());
            }
            if let Some(rs) = a.response_status.as_deref().filter(|x| !x.is_empty()) {
                tags.push(rs.to_string());
            }
            if let Some(tm) = a.team_member_id.as_deref() {
                tags.push(format!("team_member: {tm}"));
            }
            let tag_str = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            s.push_str(&format!("- {name} <{email}>{tag_str}\n", email = a.email));
        }
    }

    if let Some(desc) = event.description.as_deref().filter(|x| !x.trim().is_empty()) {
        s.push_str("\nDescription (excerpt):\n");
        s.push_str(&truncate_chars(desc.trim(), EVENT_DESCRIPTION_CAP));
        s.push('\n');
    }

    // #128: when the event belongs to a recurring series, fold in a
    // compact series summary so the model can place this occurrence in
    // its cadence without needing a second tool call. The full per-
    // occurrence list is still one `read_event_series` away when the
    // question is actually about history.
    if let Some(master_id) = event.series_master_id.as_deref() {
        let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
        let series: Vec<crate::connectors::calendar::CalendarEvent> = match conn_state.lock() {
            Ok(c) => crate::connectors::calendar::list_events_by_series_id(&c, master_id)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        s.push_str(&format_series_summary(&series));
    }

    ToolResult {
        content: s,
        is_error: false,
    }
}

/// Compact summary line / block for a recurring series (#128). Drops a
/// section header so the model can quote from it; computes "steady"
/// members (present in >= SERIES_STEADY_MEMBER_RATIO of occurrences)
/// from the union of attendee emails across all known occurrences.
/// Returns an empty string for an empty input — the caller has already
/// decided to render it; the empty fallback is defensive.
fn format_series_summary(series: &[crate::connectors::calendar::CalendarEvent]) -> String {
    if series.is_empty() {
        return String::new();
    }
    let total = series.len();
    let first = series.iter().map(|e| e.start_ms).min().unwrap_or(0);
    let last = series.iter().map(|e| e.start_ms).max().unwrap_or(0);

    // Per-attendee occurrence count keyed by lowercased email. Lowercase
    // because Graph occasionally varies casing across occurrences of
    // the same series and we don't want to split the count.
    let mut counts: std::collections::HashMap<String, (String, usize)> =
        std::collections::HashMap::new();
    for ev in series {
        for a in &ev.attendees {
            if a.is_self || a.email.trim().is_empty() {
                continue;
            }
            let key = a.email.to_ascii_lowercase();
            let display = a
                .display_name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| a.email.clone());
            let entry = counts.entry(key).or_insert((display, 0));
            entry.1 += 1;
        }
    }
    let cutoff = ((total as f32) * SERIES_STEADY_MEMBER_RATIO).ceil() as usize;
    let mut steady: Vec<(String, String, usize)> = counts
        .into_iter()
        .filter(|(_, (_, c))| *c >= cutoff.max(1))
        .map(|(email, (name, c))| (email, name, c))
        .collect();
    // Sort most-frequent first, then alpha by name for deterministic output.
    steady.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));

    let mut s = String::new();
    s.push_str("\n## Series summary\n");
    s.push_str(&format!("Total occurrences known: {total}\n"));
    s.push_str(&format!("First seen: {}\n", format_date(first)));
    s.push_str(&format!("Last seen: {}\n", format_date(last)));
    s.push_str(&format!(
        "Steady members (present in >= {}% of occurrences):\n",
        (SERIES_STEADY_MEMBER_RATIO * 100.0) as u32
    ));
    if steady.is_empty() {
        s.push_str("- _(no recurring attendees besides self)_\n");
    } else {
        for (email, name, count) in steady {
            s.push_str(&format!(
                "- {name} <{email}> ({count}/{total})\n",
                name = name,
                email = email,
                count = count,
                total = total,
            ));
        }
    }
    s
}

/// Read every known occurrence of the series that `[E<n>]` belongs to
/// (#128). The dispatcher resolves the schedule index → series_master_id
/// → all rows in `calendar_events` with that master id. Errors when
/// the event is one-off (no `series_master_id`). Per-occurrence
/// attendee lists let the model answer questions like "which of the
/// last 4 standups did Alice miss?" with concrete dates.
fn dispatch_read_event_series(
    app: &AppHandle,
    n: usize,
    schedule: &[crate::connectors::calendar::CalendarEvent],
) -> ToolResult {
    if n > schedule.len() {
        return ToolResult {
            content: format!(
                "[E{n}] is out of range. Schedule has {len} entries — valid range is [E1]..[E{len}].",
                n = n,
                len = schedule.len()
            ),
            is_error: true,
        };
    }
    let event = &schedule[n - 1];
    let master_id = match event.series_master_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => {
            return ToolResult {
                content: format!(
                    "[E{n}] '{title}' is a one-off event, not part of a recurring series — \
                     read_event_series doesn't apply. Use read_event_details instead.",
                    n = n,
                    title = event.title.trim()
                ),
                is_error: true,
            };
        }
    };
    let series = {
        let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
        let c = match conn_state.lock() {
            Ok(c) => c,
            Err(_) => {
                return ToolResult {
                    content: "Internal error: connection lock poisoned.".to_string(),
                    is_error: true,
                };
            }
        };
        crate::connectors::calendar::list_events_by_series_id(&c, master_id).unwrap_or_default()
    };
    render_event_series_output(n, event, &series)
}

/// Pure renderer for `read_event_series` output. Split for unit
/// testability — feed it any synthetic series and assert the shape.
fn render_event_series_output(
    n: usize,
    anchor_event: &crate::connectors::calendar::CalendarEvent,
    series: &[crate::connectors::calendar::CalendarEvent],
) -> ToolResult {
    if series.is_empty() {
        return ToolResult {
            content: format!(
                "[E{n}] '{title}' is part of a recurring series, but no occurrences are stored \
                 (the connector may not have synced any). Try read_event_details for the single \
                 occurrence.",
                n = n,
                title = anchor_event.title.trim()
            ),
            is_error: false,
        };
    }
    let total = series.len();
    let returned = total.min(SERIES_OCCURRENCE_CAP);
    // Take the most recent `cap` so the model sees the current state of
    // a long-running series rather than the original kickoff meeting.
    let truncated = total > SERIES_OCCURRENCE_CAP;
    let slice: Vec<&crate::connectors::calendar::CalendarEvent> = if truncated {
        series.iter().rev().take(returned).rev().collect()
    } else {
        series.iter().collect()
    };

    let mut s = String::new();
    s.push_str(&format!(
        "# Series for [E{n}] {title}\n",
        n = n,
        title = anchor_event.title.trim()
    ));
    s.push_str(&format!("Total known occurrences: {total}\n"));
    if truncated {
        s.push_str(&format!(
            "Showing the {returned} most recent (older occurrences elided).\n",
            returned = returned
        ));
    }
    s.push_str(&format_series_summary(series));
    s.push_str("\n## Occurrences\n");
    for ev in slice {
        s.push_str(&format!(
            "\n### {when}\n",
            when = format_dt_range(ev.start_ms, ev.end_ms, ev.all_day)
        ));
        s.push_str(&format!(
            "Linked note: {}\n",
            ev.linked_note_id.as_deref().unwrap_or("(none yet)")
        ));
        if let Some(status) = ev.status.as_deref().filter(|x| !x.is_empty()) {
            s.push_str(&format!("Status: {status}\n"));
        }
        s.push_str("Attendees:\n");
        if ev.attendees.is_empty() {
            s.push_str("- _(none)_\n");
        } else {
            for a in &ev.attendees {
                let name = a
                    .display_name
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .unwrap_or(&a.email);
                let mut tags: Vec<String> = Vec::new();
                if a.is_organizer {
                    tags.push("organizer".to_string());
                }
                if a.is_self {
                    tags.push("self".to_string());
                }
                if let Some(rs) = a.response_status.as_deref().filter(|x| !x.is_empty()) {
                    tags.push(rs.to_string());
                }
                let tag_str = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", tags.join(", "))
                };
                s.push_str(&format!("- {name} <{email}>{tag_str}\n", email = a.email));
            }
        }
    }
    ToolResult {
        content: s,
        is_error: false,
    }
}

fn read_note_body(note_path: &std::path::Path) -> String {
    let raw = match std::fs::read_to_string(note_path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let (_yaml, body) = crate::notes::split_frontmatter(&raw);
    truncate_chars(body.trim(), PER_NOTE_BODY_CAP)
}

/// Write one row into `prompt_dumps` capturing what the AI saw for
/// this turn (#134). Best-effort: failures are logged and swallowed —
/// the diagnostic dump must never break the user-visible response.
fn persist_prompt_dump(
    app: &AppHandle,
    turn_id: &str,
    prompt: &str,
    sources: &[AskSource],
    dispatches: &[DispatchRecord],
    latency_ms: i64,
    query: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    cache_creation_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
) {
    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let conn = match conn_state.lock() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ask] prompt_dump: conn lock poisoned: {e}");
            return;
        }
    };
    if let Err(e) = write_prompt_dump(
        &conn,
        turn_id,
        prompt,
        sources,
        dispatches,
        latency_ms,
        current_unix_ms(),
        query,
        tokens_in,
        tokens_out,
        cache_creation_tokens,
        cache_read_tokens,
    ) {
        eprintln!("[ask] prompt_dump write failed: {e}");
    }
}

/// Pure DB writer for the prompt dump — split from `persist_prompt_dump`
/// so it's unit-testable with a bare `&Connection` (no Tauri state).
fn write_prompt_dump(
    conn: &rusqlite::Connection,
    turn_id: &str,
    prompt: &str,
    sources: &[AskSource],
    dispatches: &[DispatchRecord],
    latency_ms: i64,
    now_ms: i64,
    query: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    cache_creation_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
) -> rusqlite::Result<()> {
    let tool_names_json =
        serde_json::to_string(TOOL_NAMES).unwrap_or_else(|_| "[]".to_string());
    let sources_json =
        serde_json::to_string(sources).unwrap_or_else(|_| "[]".to_string());
    let dispatches_json =
        serde_json::to_string(dispatches).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO prompt_dumps(turn_id, prompt, system_prompt, tool_names_json, \
                                  sources_json, dispatches_json, latency_ms, created_ms, \
                                  query, tokens_in, tokens_out, \
                                  cache_creation_tokens, cache_read_tokens) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
         ON CONFLICT(turn_id) DO UPDATE SET \
            prompt = excluded.prompt, \
            system_prompt = excluded.system_prompt, \
            tool_names_json = excluded.tool_names_json, \
            sources_json = excluded.sources_json, \
            dispatches_json = excluded.dispatches_json, \
            latency_ms = excluded.latency_ms, \
            query = excluded.query, \
            tokens_in = excluded.tokens_in, \
            tokens_out = excluded.tokens_out, \
            cache_creation_tokens = excluded.cache_creation_tokens, \
            cache_read_tokens = excluded.cache_read_tokens",
        rusqlite::params![
            turn_id,
            prompt,
            SYSTEM_PROMPT,
            tool_names_json,
            sources_json,
            dispatches_json,
            latency_ms,
            now_ms,
            query,
            tokens_in,
            tokens_out,
            cache_creation_tokens,
            cache_read_tokens,
        ],
    )?;
    Ok(())
}

/// Hydrate a prompt-dump row for the inspector UI (#134). Returns
/// `None` for turn ids without a stored dump (errored turns, or
/// historical turns from before this issue landed).
fn read_prompt_dump(
    conn: &rusqlite::Connection,
    turn_id: &str,
) -> rusqlite::Result<Option<PromptDumpView>> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        "SELECT turn_id, prompt, system_prompt, tool_names_json, sources_json, \
                dispatches_json, latency_ms, created_ms, query, tokens_in, tokens_out, \
                cache_creation_tokens, cache_read_tokens \
         FROM prompt_dumps WHERE turn_id = ?1",
        rusqlite::params![turn_id],
        |r| {
            let tool_names: Vec<String> = serde_json::from_str(&r.get::<_, String>(3)?)
                .unwrap_or_default();
            let sources: serde_json::Value = serde_json::from_str(&r.get::<_, String>(4)?)
                .unwrap_or_else(|_| serde_json::json!([]));
            let dispatches: serde_json::Value = serde_json::from_str(&r.get::<_, String>(5)?)
                .unwrap_or_else(|_| serde_json::json!([]));
            Ok(PromptDumpView {
                turn_id: r.get(0)?,
                prompt: r.get(1)?,
                system_prompt: r.get(2)?,
                tool_names,
                sources,
                dispatches,
                latency_ms: r.get(6)?,
                created_ms: r.get(7)?,
                query: r.get(8)?,
                tokens_in: r.get(9)?,
                tokens_out: r.get(10)?,
                cache_creation_tokens: r.get(11)?,
                cache_read_tokens: r.get(12)?,
            })
        },
    )
    .optional()
}

/// Fetch the structured prompt dump for an assistant turn (#134).
/// Powers the 🔍 inspector on assistant message bubbles.
#[tauri::command]
pub fn get_prompt_dump(
    turn_id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Option<PromptDumpView>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    read_prompt_dump(&c, &turn_id).map_err(|e| e.to_string())
}

/// Parse citation labels (`[N]`, `[E2]`, `[W3]`, `[T7]`) out of the
/// assistant's response text. Mirrors the regex used frontend-side in
/// `ChatMessage.tsx` (`/\[([WET]?\d{1,3})\]/g`). Returns labels in
/// first-appearance order, dedup'd. Hand-rolled to avoid adding the
/// `regex` crate for one tiny pattern.
fn extract_citations(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        // Optional single letter prefix (W/E/T).
        if j < bytes.len() && (bytes[j] == b'W' || bytes[j] == b'E' || bytes[j] == b'T') {
            j += 1;
        }
        // 1..=3 digits.
        let digit_start = j;
        while j < bytes.len() && j - digit_start < 3 && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == digit_start || j >= bytes.len() || bytes[j] != b']' {
            i += 1;
            continue;
        }
        // `text[start..j]` is the label between the brackets.
        let label = match std::str::from_utf8(&bytes[start..j]) {
            Ok(s) => s.to_string(),
            Err(_) => {
                i = j + 1;
                continue;
            }
        };
        if seen.insert(label.clone()) {
            out.push(label);
        }
        i = j + 1;
    }
    out
}

/// Aggregate the source list by `kind` for the diagnostics view (#135).
fn count_sources_by_kind(sources: &serde_json::Value) -> serde_json::Value {
    let mut by_kind: std::collections::BTreeMap<String, i64> =
        std::collections::BTreeMap::new();
    if let Some(arr) = sources.as_array() {
        for s in arr {
            if let Some(kind) = s.get("kind").and_then(|v| v.as_str()) {
                *by_kind.entry(kind.to_string()).or_insert(0) += 1;
            }
        }
    }
    serde_json::to_value(by_kind).unwrap_or_else(|_| serde_json::json!({}))
}

/// One row of the per-turn telemetry table. Joins `prompt_dumps` with
/// the assistant `chat_messages` row.
fn read_chat_turn_metrics(
    conn: &rusqlite::Connection,
    limit: usize,
) -> rusqlite::Result<Vec<ChatTurnMetric>> {
    let mut stmt = conn.prepare(
        "SELECT p.turn_id, cm.conversation_id, p.created_ms, p.latency_ms, p.query, \
                COALESCE(LENGTH(cm.text), 0) AS assistant_text_chars, \
                p.tokens_in, p.tokens_out, p.sources_json, p.dispatches_json, \
                cm.text, p.cache_creation_tokens, p.cache_read_tokens \
           FROM prompt_dumps p \
           LEFT JOIN chat_messages cm \
                  ON cm.turn_id = p.turn_id AND cm.role = 'assistant' \
          ORDER BY p.created_ms DESC \
          LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
        let sources_json: String = r.get(8)?;
        let sources: serde_json::Value =
            serde_json::from_str(&sources_json).unwrap_or_else(|_| serde_json::json!([]));
        let dispatches_json: String = r.get(9)?;
        let dispatches: serde_json::Value =
            serde_json::from_str(&dispatches_json).unwrap_or_else(|_| serde_json::json!([]));
        let assistant_text: Option<String> = r.get(10)?;
        let citations = match assistant_text.as_deref() {
            Some(t) => extract_citations(t),
            None => Vec::new(),
        };
        let sources_total = sources
            .as_array()
            .map(|a| a.len() as i64)
            .unwrap_or(0);
        let sources_by_kind = count_sources_by_kind(&sources);
        let (tool_call_count, had_error) = match dispatches.as_array() {
            Some(arr) => (
                arr.len() as i64,
                arr.iter().any(|d| {
                    d.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
                }),
            ),
            None => (0, false),
        };
        Ok(ChatTurnMetric {
            turn_id: r.get(0)?,
            conversation_id: r.get(1)?,
            created_ms: r.get(2)?,
            latency_ms: r.get(3)?,
            query: r.get(4)?,
            assistant_text_chars: r.get(5)?,
            tokens_in: r.get(6)?,
            tokens_out: r.get(7)?,
            cache_creation_tokens: r.get(11)?,
            cache_read_tokens: r.get(12)?,
            sources_total,
            sources_by_kind,
            citations,
            tool_call_count,
            had_error_dispatch: had_error,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
}

/// List recent chat turns with derived telemetry for the Diagnostics
/// view (#135). Default cap of 100 rows; bumpable via the optional
/// `limit` param. All compute happens server-side per row — cheap at
/// the scale we're operating (single-user, thousands of turns lifetime).
#[tauri::command]
pub fn list_chat_turn_metrics(
    limit: Option<usize>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<ChatTurnMetric>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    read_chat_turn_metrics(&c, limit.unwrap_or(100)).map_err(|e| e.to_string())
}

fn truncate_chars(s: &str, cap: usize) -> String {
    let mut out = s.to_string();
    if out.chars().count() > cap {
        let cutoff = out
            .char_indices()
            .nth(cap)
            .map(|(i, _)| i)
            .unwrap_or(out.len());
        out.truncate(cutoff);
        out.push('…');
    }
    out
}

fn format_user_message(
    query: &str,
    directory: &[DirectoryEntry],
    retrieved_paths: &std::collections::HashSet<String>,
    profiles: &[(crate::team::TeamMember, String)],
    schedule: &[crate::connectors::calendar::CalendarEvent],
    workstreams: &[crate::workstreams::Workstream],
    teams_messages: &[crate::connectors::teams::TeamsMessage],
    unread_emails: &[crate::connectors::email::EmailMessage],
) -> String {
    let mut s = String::new();

    s.push_str("# Notes directory\n\n");
    if directory.is_empty() {
        s.push_str("_(no notes yet)_\n\n");
    } else {
        for (i, e) in directory.iter().enumerate() {
            let n = i + 1;
            let date = format_date(e.modified_ms);
            let preview = preview_one_line(&e.preview);
            let _ = std::fmt::Write::write_fmt(
                &mut s,
                format_args!("[{n}] {title} ({date}) — {preview}\n",
                    title = e.title.trim(),
                    date = date,
                    preview = preview),
            );
        }
        s.push('\n');
    }

    s.push_str("# Top candidates (full body)\n\n");
    let mut deep_count = 0usize;
    for (i, e) in directory.iter().enumerate() {
        if !retrieved_paths.contains(&e.note_path) {
            continue;
        }
        deep_count += 1;
        let n = i + 1;
        let date = format_date(e.modified_ms);
        let body = read_note_body(&PathBuf::from(&e.note_path));
        let _ = std::fmt::Write::write_fmt(
            &mut s,
            format_args!(
                "[{n}] {title} ({date})\n{body}\n\n---\n\n",
                title = e.title.trim(),
                date = date,
                body = body.trim()
            ),
        );
    }
    if deep_count == 0 {
        s.push_str(
            "_(no notes matched the keywords; reason from the directory previews above, or call read_note/read_transcript on a likely entry)_\n\n",
        );
    }

    if !profiles.is_empty() {
        s.push_str("# Team profiles\n\n");
        for (m, excerpt) in profiles {
            let aliases = if m.aliases.is_empty() {
                String::new()
            } else {
                let vals: Vec<&str> = m.aliases.iter().map(|a| a.value.as_str()).collect();
                format!(" (aliases: {})", vals.join(", "))
            };
            let role = if m.role.is_empty() {
                String::new()
            } else {
                format!(" — {}", m.role)
            };
            let body = if excerpt.is_empty() {
                "_(profile is empty)_".to_string()
            } else {
                excerpt.clone()
            };
            let _ = std::fmt::Write::write_fmt(
                &mut s,
                format_args!(
                    "## {name}{role}{aliases}\n\n{body}\n\n",
                    name = m.display_name,
                    role = role,
                    aliases = aliases,
                    body = body
                ),
            );
        }
    }

    s.push_str(&format_schedule_section(schedule));

    if !teams_messages.is_empty() {
        s.push_str(&format_teams_messages_section(teams_messages));
    }

    if !unread_emails.is_empty() {
        // Build a small map of team_member email aliases so the section
        // formatter can append a `(team)` marker on senders the user
        // already knows — gives the model an immediate "this is a real
        // colleague" cue without needing a profile lookup. Built here
        // from the already-loaded `profiles` slice so we don't re-query
        // the team table inside the formatter.
        let mut team_emails: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (m, _) in profiles {
            for a in &m.aliases {
                if a.kind == "email" {
                    team_emails.insert(a.value.to_ascii_lowercase());
                }
            }
        }
        s.push_str(&format_unread_emails_section(unread_emails, &team_emails));
    }

    if !workstreams.is_empty() {
        let team_by_id: std::collections::HashMap<&str, &str> = profiles
            .iter()
            .map(|(m, _)| (m.id.as_str(), m.display_name.as_str()))
            .collect();
        s.push_str(&format_workstreams_section(workstreams, &team_by_id));
    }

    s.push_str("# Question\n\n");
    s.push_str(query.trim());
    s
}

/// Render synthesized workstreams as a labeled prompt section.
/// Each workstream becomes one line with its `[W<N>]` label, title,
/// one-line summary, and item counts. Empty input emits nothing — the
/// caller decides whether to skip the section entirely.
fn format_workstreams_section(
    workstreams: &[crate::workstreams::Workstream],
    team_by_id: &std::collections::HashMap<&str, &str>,
) -> String {
    let mut s = String::new();
    s.push_str("# Workstreams\n\n");
    for (i, w) in workstreams.iter().enumerate() {
        let label = format!("W{}", i + 1);
        let summary = workstream_one_line_summary(&w.summary);
        let mut counts: Vec<String> = Vec::new();
        if w.email_count > 0 {
            counts.push(format!("{} emails", w.email_count));
        }
        if w.event_count > 0 {
            counts.push(format!("{} meetings", w.event_count));
        }
        if w.note_count > 0 {
            counts.push(format!("{} notes", w.note_count));
        }
        let counts_suffix = if counts.is_empty() {
            String::new()
        } else {
            format!(" ({})", counts.join(" · "))
        };
        let _ = std::fmt::Write::write_fmt(
            &mut s,
            format_args!(
                "[{label}] {title} — {summary}{counts}\n",
                label = label,
                title = w.title.trim(),
                summary = summary,
                counts = counts_suffix,
            ),
        );
        // Owner + members (#81). One-line excerpts; the full lists are
        // in `read_workstream` for richer reasoning.
        if let Some(owner_id) = w.owner_member_id.as_deref() {
            if let Some(name) = team_by_id.get(owner_id) {
                s.push_str(&format!("    Owner: {name}\n"));
            }
        }
        if !w.members.is_empty() {
            let names: Vec<&str> = w
                .members
                .iter()
                .filter_map(|id| team_by_id.get(id.as_str()).copied())
                .take(8)
                .collect();
            if !names.is_empty() {
                let suffix = if w.members.len() > names.len() {
                    format!(" (+{} more)", w.members.len() - names.len())
                } else {
                    String::new()
                };
                s.push_str(&format!(
                    "    Members: {names}{suffix}\n",
                    names = names.join(", "),
                    suffix = suffix
                ));
            }
        }
        // User-authored ground truth (#77). Show a one-line excerpt
        // here so the model knows it exists; the full text is in the
        // `read_workstream` tool result.
        if let Some(notes) = w.user_notes.as_deref().filter(|s| !s.trim().is_empty()) {
            let one_line = workstream_one_line_summary(notes);
            s.push_str(&format!("    (user notes: {one_line})\n"));
        }
    }
    s.push('\n');
    s
}

fn workstream_one_line_summary(s: &str) -> String {
    // Collapse whitespace so multi-line summaries render as a single
    // entry; truncate at WORKSTREAM_SUMMARY_CAP chars.
    let collapsed: String = {
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
    };
    truncate_chars(&collapsed, WORKSTREAM_SUMMARY_CAP)
}

/// Render the recent Teams chat messages list as a labeled prompt
/// section (#136). Each entry: `[T<N>] @sender in "chat" (date) — preview`.
/// Topic-less chats (DMs) omit the `in "..."` clause. Caller is
/// responsible for the empty-slice short-circuit; we always emit the
/// header when called.
fn format_teams_messages_section(
    messages: &[crate::connectors::teams::TeamsMessage],
) -> String {
    let mut s = String::new();
    s.push_str("# Recent Teams messages (last 14 days)\n\n");
    for (i, m) in messages.iter().enumerate() {
        let label = format!("T{}", i + 1);
        let when = format_date(m.sent_at_ms);
        let sender = m
            .from_name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .or(m.from_email.as_deref().filter(|e| !e.trim().is_empty()))
            .unwrap_or("(unknown sender)");
        let chat_label = match &m.chat_topic {
            Some(topic) if !topic.trim().is_empty() => {
                format!(" in \"{}\"", topic.trim())
            }
            _ => String::new(),
        };
        let preview = preview_one_line(m.body_preview.as_deref().unwrap_or(""));
        let _ = std::fmt::Write::write_fmt(
            &mut s,
            format_args!("[{label}] @{sender}{chat_label} ({when}) — {preview}\n"),
        );
    }
    s.push('\n');
    s
}

/// Render the inbound-email follow-up list as a labeled prompt section
/// (#137). Each entry: `[U<N>] from {Sender} (date) — "Subject" — preview`.
/// The `(team)` marker is appended on senders that resolve to a
/// `team_members` row via the supplied alias set, so the model gets an
/// immediate "real colleague" cue without a profile lookup.
///
/// The section title intentionally says "awaiting attention" rather than
/// "unread" — the underlying `is_read` flag is unreliable for users who
/// work through clients (Front, etc.) that don't sync read state, so the
/// filter is sender-shape based (`is_noise_sender` + bulk-sender drop)
/// not `is_read = 0`. The model is told this in the header so it
/// reasons about the list correctly.
fn format_unread_emails_section(
    messages: &[crate::connectors::email::EmailMessage],
    team_emails_lower: &std::collections::HashSet<String>,
) -> String {
    let mut s = String::new();
    s.push_str(
        "# Recent emails awaiting attention (last 14 days, automated senders filtered)\n\n",
    );
    for (i, m) in messages.iter().enumerate() {
        let label = format!("U{}", i + 1);
        let when = format_date(m.sent_at_ms);
        let sender_display = m
            .from_name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(m.from_email.as_str());
        let team_marker = if team_emails_lower.contains(&m.from_email.to_ascii_lowercase()) {
            " (team)"
        } else {
            ""
        };
        let subject = m.subject.trim();
        let subject_clause = if subject.is_empty() {
            String::new()
        } else {
            format!(" — \"{subject}\"")
        };
        let preview = preview_one_line(m.body_preview.as_deref().unwrap_or(""));
        let preview_clause = if preview.is_empty() {
            String::new()
        } else {
            format!(" — {preview}")
        };
        let _ = std::fmt::Write::write_fmt(
            &mut s,
            format_args!(
                "[{label}] from {sender_display}{team_marker} ({when}){subject_clause}{preview_clause}\n"
            ),
        );
    }
    s.push('\n');
    s
}

/// One-line chip title for a Teams message in the `sources` strip
/// emitted to the frontend (#136). The chip's visible text on hover.
fn teams_chip_title(m: &crate::connectors::teams::TeamsMessage) -> String {
    let sender = m
        .from_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .or(m.from_email.as_deref().filter(|e| !e.trim().is_empty()))
        .unwrap_or("unknown");
    let preview = m
        .body_preview
        .as_deref()
        .map(|s| preview_one_line(s))
        .unwrap_or_default();
    if preview.is_empty() {
        format!("@{sender}")
    } else {
        format!("@{sender}: {preview}")
    }
}

/// One-line chip title for an email in the `sources` strip (#137).
/// Sender name (or address) + subject keeps the chip useful when the
/// model cites `[U<N>]` mid-sentence.
fn email_chip_title(m: &crate::connectors::email::EmailMessage) -> String {
    let sender = m
        .from_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(m.from_email.as_str());
    let subject = m.subject.trim();
    if subject.is_empty() {
        format!("@{sender}")
    } else {
        format!("@{sender}: {subject}")
    }
}

/// Same shape as `load_teams_message_workstream_map`: resolve each
/// surfaced email id to a workstream attachment so the `[U<N>]` chip
/// has somewhere to navigate (#137). One query per email — input set
/// is capped at EMAIL_FOLLOWUP_CAP (=30) so this stays cheap.
fn load_email_workstream_map(
    conn: &rusqlite::Connection,
    messages: &[crate::connectors::email::EmailMessage],
) -> std::collections::HashMap<String, String> {
    use rusqlite::OptionalExtension as _;
    let mut out: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(messages.len());
    if messages.is_empty() {
        return out;
    }
    for m in messages {
        let ws_id: Option<String> = conn
            .query_row(
                "SELECT workstream_id FROM workstream_signals \
                  WHERE kind = 'email' AND item_id = ?1 \
                    AND manual_detached_ms IS NULL \
                  ORDER BY added_ms DESC LIMIT 1",
                rusqlite::params![m.id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(id) = ws_id {
            out.insert(m.id.clone(), id);
        }
    }
    out
}

/// Self email address (lowercased) from the `team_members` row marked
/// `is_self = 1`. Used by the follow-up filter to drop the user's own
/// outbound mail — even though today's connector only syncs inbound,
/// keeping the filter symmetric protects against future Sent-Items
/// support. `None` when no `is_self` row or no email alias exists.
fn lookup_self_email(conn: &rusqlite::Connection) -> Option<String> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        "SELECT LOWER(tma.value) \
         FROM team_member_aliases tma \
         JOIN team_members tm ON tm.id = tma.member_id \
         WHERE tm.is_self = 1 AND tma.kind = 'email' \
         LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Resolve each Teams message id to the most recent non-tombstoned
/// workstream it's attached to, if any (#136). Powers the chip-click
/// navigation; unattached messages have no entry and the chip becomes
/// a soft no-op on the frontend. One query, one map per turn.
fn load_teams_message_workstream_map(
    conn: &rusqlite::Connection,
    messages: &[crate::connectors::teams::TeamsMessage],
) -> std::collections::HashMap<String, String> {
    use rusqlite::OptionalExtension as _;
    let mut out: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(messages.len());
    if messages.is_empty() {
        return out;
    }
    // Per-row query keeps the SQL simple and predictable for a small
    // (≤30) input set. If we ever raise TEAMS_MESSAGE_CAP, switch to a
    // single IN-clause batch.
    for m in messages {
        let ws_id: Option<String> = conn
            .query_row(
                "SELECT workstream_id FROM workstream_signals \
                  WHERE kind = 'teams_message' AND item_id = ?1 \
                    AND manual_detached_ms IS NULL \
                  ORDER BY added_ms DESC LIMIT 1",
                rusqlite::params![m.id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(id) = ws_id {
            out.insert(m.id.clone(), id);
        }
    }
    out
}

/// Read the full body of Teams message `[T<n>]` plus a few surrounding
/// messages in the same chat for conversational context (#136). Splits
/// the app-state lookup from the rendering so the latter is unit-testable.
fn dispatch_read_teams_message(
    app: &AppHandle,
    n: usize,
    messages: &[crate::connectors::teams::TeamsMessage],
) -> ToolResult {
    if n == 0 || n > messages.len() {
        return render_teams_tool_output(n, messages, &[], &[]);
    }
    let target = &messages[n - 1];
    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let (before, after) = {
        let c = match conn_state.lock() {
            Ok(c) => c,
            Err(_) => {
                return ToolResult {
                    content: "Internal error: connection lock poisoned.".to_string(),
                    is_error: true,
                };
            }
        };
        crate::connectors::teams::list_chat_context(
            &c,
            &target.chat_id,
            target.sent_at_ms,
            3,
            1,
        )
        .unwrap_or_else(|_| (Vec::new(), Vec::new()))
    };
    render_teams_tool_output(n, messages, &before, &after)
}

/// Pure renderer for `read_teams_message` output. Out-of-range `n`
/// surfaces an `is_error: true` ToolResult; valid `n` formats the
/// target body plus surrounding context. Split from
/// `dispatch_read_teams_message` so this is unit-testable without a
/// Tauri app handle.
fn render_teams_tool_output(
    n: usize,
    messages: &[crate::connectors::teams::TeamsMessage],
    before: &[crate::connectors::teams::TeamsMessage],
    after: &[crate::connectors::teams::TeamsMessage],
) -> ToolResult {
    if n == 0 || n > messages.len() {
        return ToolResult {
            content: format!(
                "[T{n}] is out of range. Recent Teams messages has {len} entries — valid range is [T1]..[T{len}].",
                n = n,
                len = messages.len()
            ),
            is_error: true,
        };
    }
    let target = &messages[n - 1];

    let body = target
        .body_preview
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            target
                .body_html
                .as_deref()
                .map(strip_html_to_text)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "(empty body)".to_string());

    let sender = target
        .from_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .or(target.from_email.as_deref().filter(|e| !e.trim().is_empty()))
        .unwrap_or("unknown");

    let mut out = format!(
        "# [T{n}] from @{sender} ({when})\n",
        n = n,
        sender = sender,
        when = format_date(target.sent_at_ms),
    );
    if let Some(topic) = target.chat_topic.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("Chat: \"{}\"\n", topic.trim()));
    }
    out.push_str("\nBody:\n");
    out.push_str(body.trim());
    out.push_str("\n");

    if !before.is_empty() || !after.is_empty() {
        out.push_str("\nSurrounding messages in this chat (chronological):\n");
        // before is DESC (newest-first prior); reverse to chronological,
        // then this message, then after (ASC).
        let mut chrono: Vec<&crate::connectors::teams::TeamsMessage> = before.iter().rev().collect();
        chrono.extend(after.iter());
        for c in chrono {
            let c_sender = c
                .from_name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .or(c.from_email.as_deref().filter(|e| !e.trim().is_empty()))
                .unwrap_or("unknown");
            let c_preview = preview_one_line(c.body_preview.as_deref().unwrap_or(""));
            out.push_str(&format!(
                "- {when} @{sender}: {preview}\n",
                when = format_date(c.sent_at_ms),
                sender = c_sender,
                preview = c_preview,
            ));
        }
    }

    ToolResult {
        content: out,
        is_error: false,
    }
}

/// Read the full body of inbound email `[U<n>]` plus the rest of its
/// thread, chronological (#137). Body comes from `body_html` when set
/// (HTML-stripped) and falls back to `body_preview` — bodies are lazy-
/// loaded post-sync, so an unseen message may only have the preview at
/// this point. Splits the conn lookup from the formatter so the latter
/// is unit-testable via `render_email_tool_output`.
fn dispatch_read_email(
    app: &AppHandle,
    n: usize,
    messages: &[crate::connectors::email::EmailMessage],
) -> ToolResult {
    if n == 0 || n > messages.len() {
        return render_email_tool_output(n, messages, &[]);
    }
    let target = &messages[n - 1];
    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let thread = {
        let c = match conn_state.lock() {
            Ok(c) => c,
            Err(_) => {
                return ToolResult {
                    content: "Internal error: connection lock poisoned.".to_string(),
                    is_error: true,
                };
            }
        };
        crate::connectors::email::list_messages_by_thread(&c, &target.thread_id)
            .unwrap_or_default()
    };
    render_email_tool_output(n, messages, &thread)
}

/// Pure renderer for `read_email` output. Out-of-range `n` returns an
/// error result. `thread` is oldest-first (the contract of
/// `list_messages_by_thread`); the target's own body is highlighted,
/// surrounding messages are condensed to one line each so the model
/// can see whether the user already replied without burning tokens on
/// every body. Split for testability.
fn render_email_tool_output(
    n: usize,
    messages: &[crate::connectors::email::EmailMessage],
    thread: &[crate::connectors::email::EmailMessage],
) -> ToolResult {
    if n == 0 || n > messages.len() {
        return ToolResult {
            content: format!(
                "[U{n}] is out of range. Recent emails awaiting attention has {len} entries — valid range is [U1]..[U{len}].",
                n = n,
                len = messages.len()
            ),
            is_error: true,
        };
    }
    let target = &messages[n - 1];

    let body_text = target
        .body_html
        .as_deref()
        .map(strip_html_to_text)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            target
                .body_preview
                .clone()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| {
            "(body not yet fetched — preview only available; the lazy-load \
             may not have run for this message)"
                .to_string()
        });

    let sender = target
        .from_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(target.from_email.as_str());

    let subject = if target.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        target.subject.trim().to_string()
    };

    let mut out = format!(
        "# [U{n}] from {sender} <{addr}> ({when})\n",
        n = n,
        sender = sender,
        addr = target.from_email,
        when = format_date(target.sent_at_ms),
    );
    out.push_str(&format!("Subject: {subject}\n"));
    out.push_str("\nBody:\n");
    out.push_str(body_text.trim());
    out.push('\n');

    if thread.len() > 1 {
        out.push_str("\nThread (chronological, target marked with ►):\n");
        for m in thread {
            let marker = if m.id == target.id { "►" } else { "·" };
            let m_sender = m
                .from_name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(m.from_email.as_str());
            let m_preview = preview_one_line(m.body_preview.as_deref().unwrap_or(""));
            out.push_str(&format!(
                "{marker} {when} from {sender}: {preview}\n",
                marker = marker,
                when = format_date(m.sent_at_ms),
                sender = m_sender,
                preview = m_preview,
            ));
        }
    }

    ToolResult {
        content: out,
        is_error: false,
    }
}

/// Minimal HTML-to-text stripper for Teams message bodies. The Graph
/// API ships HTML with `<div>`, `<p>`, `<a>`, basic formatting; we drop
/// tags and collapse whitespace. Not a full parser — body_preview is
/// the preferred source and this is only the fallback.
fn strip_html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Collapse runs of whitespace.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_space = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !last_space {
                collapsed.push(' ');
                last_space = true;
            }
        } else {
            collapsed.push(ch);
            last_space = false;
        }
    }
    collapsed.trim().to_string()
}

/// Render the upcoming/recent meeting list as a labeled prompt
/// section. Each event becomes one line with `[E<N>]` label, title,
/// time range, attendees (max 5 + overflow), and location.
fn format_schedule_section(events: &[crate::connectors::calendar::CalendarEvent]) -> String {
    let mut s = String::new();
    s.push_str("# Schedule (last 14 days, next 14 days)\n\n");
    if events.is_empty() {
        s.push_str("_(no scheduled meetings in this window)_\n\n");
        return s;
    }
    for (i, e) in events.iter().enumerate() {
        let label = format!("E{}", i + 1);
        let when = format_dt_range(e.start_ms, e.end_ms, e.all_day);
        let attendees = format_attendee_summary(&e.attendees);
        let attendee_suffix = if attendees.is_empty() {
            String::new()
        } else {
            format!(" — {attendees}")
        };
        let location_suffix = match e.location.as_deref() {
            Some(loc) if !loc.trim().is_empty() => format!(" — {loc}"),
            _ => String::new(),
        };
        s.push_str(&format!(
            "[{label}] {} ({when}){attendee_suffix}{location_suffix}\n",
            e.title.trim()
        ));
    }
    s.push('\n');
    s
}

/// Concise attendee list for the Schedule section. Skips `is_self`,
/// caps at 5 visible names, suffixes "+N more" if more remain.
fn format_attendee_summary(
    attendees: &[crate::connectors::calendar::CalendarAttendee],
) -> String {
    let visible: Vec<&str> = attendees
        .iter()
        .filter(|a| !a.is_self)
        .filter_map(|a| {
            a.display_name
                .as_deref()
                .filter(|x| !x.is_empty())
                .or(Some(a.email.as_str()))
        })
        .take(5)
        .collect();
    let total_others = attendees.iter().filter(|a| !a.is_self).count();
    let mut out = visible.join(", ");
    if total_others > visible.len() {
        let extra = total_others - visible.len();
        if !out.is_empty() {
            out.push_str(&format!(", +{extra} more"));
        } else {
            out = format!("+{extra} attendees");
        }
    }
    out
}

/// "YYYY-MM-DD HH:MM → HH:MM" if same UTC day, "YYYY-MM-DD HH:MM →
/// YYYY-MM-DD HH:MM" if cross-day, "YYYY-MM-DD (all day)" for all-day
/// events. UTC throughout — Microsoft Graph requests UTC via the
/// `Prefer` header, and we'd rather not introduce locale-dependent
/// formatting in the prompt.
fn format_dt_range(start_ms: i64, end_ms: i64, all_day: bool) -> String {
    use chrono::{DateTime, Utc};
    let start = DateTime::<Utc>::from_timestamp(start_ms / 1000, 0);
    let end = DateTime::<Utc>::from_timestamp(end_ms / 1000, 0);
    match (start, end) {
        (Some(s), Some(e)) => {
            if all_day {
                return format!("{} (all day)", s.format("%Y-%m-%d"));
            }
            let s_date = s.format("%Y-%m-%d").to_string();
            let e_date = e.format("%Y-%m-%d").to_string();
            let s_time = s.format("%H:%M").to_string();
            let e_time = e.format("%H:%M").to_string();
            if s_date == e_date {
                format!("{s_date} {s_time} → {e_time}")
            } else {
                format!("{s_date} {s_time} → {e_date} {e_time}")
            }
        }
        _ => "(invalid timestamp)".to_string(),
    }
}

fn preview_one_line(preview: &str) -> String {
    let collapsed: String = preview
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    truncate_chars(collapsed.trim(), PER_PREVIEW_CAP)
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn format_date(modified_ms: i64) -> String {
    use chrono::{DateTime, Utc};
    let secs = modified_ms / 1000;
    let nsec = ((modified_ms % 1000) * 1_000_000) as u32;
    DateTime::<Utc>::from_timestamp(secs, nsec)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown date".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workstreams::{NoteRef, Workstream, WorkstreamDetail};
    use crate::connectors::email::EmailMessage;

    fn make_ws(id: &str, title: &str, summary: &str, last_activity: i64) -> Workstream {
        Workstream {
            id: id.to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            status: "active".to_string(),
            last_activity_ms: last_activity,
            created_ms: 0,
            updated_ms: 0,
            user_notes: None,
            archived_at_ms: None,
            reopened_at_ms: None,
            owner_member_id: None,
            members: Vec::new(),
            email_count: 0,
            event_count: 0,
            note_count: 0,
            link_count: 0,
            parent_workstream_id: None,
            external_participants: Vec::new(),
        }
    }

    /// #140: every kind registered in `workstreams::signals::registry`
    /// must have a path into the AI prompt — either a labeled section
    /// in `format_user_message` (declared in `PROMPT_SECTION_KINDS`) or
    /// a read-tool result that surfaces items of that kind (declared in
    /// `TOOL_RESOLVABLE_KINDS`). The Teams-messages coverage miss was a
    /// signal kind in the DB with no path into the model's view; this
    /// test catches the next one structurally before it ships.
    #[test]
    fn registry_coverage_every_kind_has_a_path_into_the_prompt() {
        let reg = crate::workstreams::signals::registry();
        let in_section: std::collections::HashSet<&str> =
            PROMPT_SECTION_KINDS.iter().copied().collect();
        let in_tool: std::collections::HashSet<&str> =
            TOOL_RESOLVABLE_KINDS.iter().copied().collect();

        let mut uncovered: Vec<&'static str> = Vec::new();
        for src in reg.iter_in_prompt_order() {
            let kind = src.kind();
            if !in_section.contains(kind) && !in_tool.contains(kind) {
                uncovered.push(kind);
            }
        }
        assert!(
            uncovered.is_empty(),
            "Signal kind(s) {uncovered:?} have no path into the AI prompt. \
             Add a labeled section in format_user_message (and list the kind \
             in PROMPT_SECTION_KINDS), OR register a read-tool in dispatch_tool \
             that surfaces items of that kind (and list it in TOOL_RESOLVABLE_KINDS). \
             Without either, the kind exists in workstream_signals but the model \
             cannot read it — same class of bug as the pre-#136 Teams miss."
        );
    }

    /// Companion to the registry-coverage test: a kind listed in the
    /// coverage slices that isn't registered in `signals::registry`
    /// means the slice is stale and the assertion in the sibling test
    /// is silently weakened. Fail fast on drift in either direction.
    #[test]
    fn registry_coverage_slices_have_no_stale_entries() {
        let reg = crate::workstreams::signals::registry();
        let registered: std::collections::HashSet<&str> = reg
            .iter_in_prompt_order()
            .map(|s| s.kind())
            .collect();
        let mut stale: Vec<&'static str> = Vec::new();
        for k in PROMPT_SECTION_KINDS.iter().chain(TOOL_RESOLVABLE_KINDS.iter()) {
            if !registered.contains(k) {
                stale.push(k);
            }
        }
        assert!(
            stale.is_empty(),
            "Coverage slice entries {stale:?} are not registered in \
             workstreams::signals::registry. Either the kind was removed from \
             the registry (drop it from PROMPT_SECTION_KINDS / TOOL_RESOLVABLE_KINDS) \
             or the slice has a typo."
        );
    }

    #[test]
    fn format_workstreams_section_renders_labels_and_counts() {
        let mut a = make_ws("ws_a", "Hyundai POC", "Final invoice details.", 100);
        a.email_count = 5;
        a.event_count = 1;

        let b = make_ws("ws_b", "Q3 hiring", "Sourcing two seniors.", 50);

        let team_map: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        let out = format_workstreams_section(&[a, b], &team_map);
        assert!(out.starts_with("# Workstreams\n\n"));
        assert!(out.contains("[W1] Hyundai POC"));
        assert!(out.contains("[W2] Q3 hiring"));
        assert!(out.contains("Final invoice details."));
        assert!(
            out.contains("(5 emails · 1 meetings)"),
            "expected counts pill in section, got: {out}"
        );
        // No counts when all zero — just the title-summary line.
        assert!(out.contains("[W2] Q3 hiring — Sourcing two seniors.\n"));
    }

    #[test]
    fn format_workstreams_section_truncates_long_summary() {
        let long = "X".repeat(WORKSTREAM_SUMMARY_CAP + 50);
        let w = make_ws("ws", "Title", &long, 0);
        let team_map: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        let out = format_workstreams_section(&[w], &team_map);
        // Truncated with the …  marker from truncate_chars.
        assert!(out.contains('…'), "expected truncation marker in: {out}");
        // Doesn't contain the trailing portion (50 chars past the cap).
        assert!(out.lines().any(|l| l.starts_with("[W1] Title — XXX")));
    }

    fn make_email(id: &str, subject: &str, sent_at_ms: i64) -> EmailMessage {
        EmailMessage {
            id: id.to_string(),
            connector_id: "mg:test".into(),
            external_id: id.to_string(),
            thread_id: "t1".into(),
            subject: subject.to_string(),
            from_email: "alice@example.com".into(),
            from_name: Some("Alice".into()),
            sent_at_ms,
            body_preview: Some("hello".into()),
            body_html: None,
            has_attachments: false,
            is_read: false,
            raw_etag: None,
            modified_ms: sent_at_ms,
            recipients: Vec::new(),
        }
    }

    #[test]
    fn format_workstream_detail_renders_all_sections() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_x".into(),
                title: "Hyundai POC".into(),
                summary: "Invoicing in flight.".into(),
                status: "active".into(),
                last_activity_ms: 1000,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 2,
                event_count: 0,
                note_count: 1,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![
                make_email("m1", "Re: invoice", 1000),
                make_email("m2", "Quote attached", 900),
            ],
            events: vec![],
            notes: vec![NoteRef {
                note_path: "/n/a.md".into(),
                title: "Hyundai kickoff".into(),
                modified_ms: 800,
            }],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W1", &detail, &team_map);
        assert!(out.starts_with("# [W1] Hyundai POC\n"));
        assert!(out.contains("Invoicing in flight."));
        assert!(out.contains("Recent emails (top 2 of 2)"));
        assert!(out.contains("Re: invoice"));
        assert!(out.contains("Quote attached"));
        // No events section when empty.
        assert!(!out.contains("Recent meetings"));
        assert!(out.contains("Recent notes (top 1 of 1)"));
        assert!(out.contains("Hyundai kickoff"));
    }

    #[test]
    fn format_workstream_detail_caps_emails_at_top_n() {
        let mut emails: Vec<EmailMessage> = (0..(WORKSTREAM_DETAIL_TOP_N + 3))
            .map(|i| make_email(&format!("m{i}"), &format!("Subject {i}"), 1000 - i as i64))
            .collect();
        // Ensure they're sorted desc as the loader returns.
        emails.sort_by(|a, b| b.sent_at_ms.cmp(&a.sent_at_ms));
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_y".into(),
                title: "Many emails".into(),
                summary: String::new(),
                status: "active".into(),
                last_activity_ms: 1000,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: emails.len() as u32,
                event_count: 0,
                note_count: 0,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails,
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W2", &detail, &team_map);
        let total = WORKSTREAM_DETAIL_TOP_N + 3;
        assert!(
            out.contains(&format!("Recent emails (top {} of {})", WORKSTREAM_DETAIL_TOP_N, total)),
            "got: {out}"
        );
        // First 5 subjects present; last 3 absent.
        for i in 0..WORKSTREAM_DETAIL_TOP_N {
            assert!(out.contains(&format!("Subject {i}")), "missing Subject {i}");
        }
        for i in WORKSTREAM_DETAIL_TOP_N..total {
            assert!(!out.contains(&format!("Subject {i}")), "leaked Subject {i}");
        }
    }

    #[test]
    fn format_workstreams_section_one_line_excerpt_when_user_notes_present() {
        let mut a = make_ws("ws_a", "Hyundai POC", "Final invoice details.", 100);
        a.user_notes = Some("Real deadline May 30 (calendar shows June). TJ owns this internally.".into());
        let team_map: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        let out = format_workstreams_section(&[a], &team_map);
        assert!(out.contains("[W1] Hyundai POC"));
        assert!(
            out.contains("(user notes: Real deadline May 30"),
            "expected one-line user notes excerpt, got: {out}"
        );
    }

    #[test]
    fn format_workstream_detail_includes_user_notes_block() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_n".into(),
                title: "Hyundai POC".into(),
                summary: "Invoicing in flight.".into(),
                status: "active".into(),
                last_activity_ms: 1000,
                created_ms: 0,
                updated_ms: 0,
                user_notes: Some("Real deadline May 30. New POC, not legacy contract.".into()),
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W1", &detail, &team_map);
        assert!(out.contains("# [W1] Hyundai POC"));
        // Summary still rendered.
        assert!(out.contains("Invoicing in flight."));
        // User notes block present, full text (under the cap).
        assert!(out.contains("User notes (ground truth):"));
        assert!(out.contains("Real deadline May 30. New POC, not legacy contract."));
    }

    #[test]
    fn format_workstream_detail_handles_empty_summary() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_z".into(),
                title: "Bare".into(),
                summary: String::new(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W3", &detail, &team_map);
        assert_eq!(out, "# [W3] Bare\n");
    }

    #[test]
    fn format_workstream_detail_renders_links_after_user_notes() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_links".into(),
                title: "Hyundai POC".into(),
                summary: "".into(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                user_notes: Some("Stay aligned with finance.".into()),
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 2,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: vec![
                crate::workstreams::WorkstreamLink {
                    id: "wsl_1".into(),
                    workstream_id: "ws_links".into(),
                    label: "Repo".into(),
                    url: "https://github.com/x/y".into(),
                    kind: Some("github".into()),
                    position: 0,
                    created_ms: 0,
                    summary: None,
                },
                crate::workstreams::WorkstreamLink {
                    id: "wsl_2".into(),
                    workstream_id: "ws_links".into(),
                    label: "Design doc".into(),
                    url: "https://www.notion.so/d".into(),
                    kind: None,
                    position: 1,
                    created_ms: 0,
                    summary: None,
                },
            ],
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W4", &detail, &team_map);

        assert!(out.contains("## Links"));
        assert!(out.contains("- [Repo](https://github.com/x/y) (github)"));
        assert!(
            out.contains("- [Design doc](https://www.notion.so/d)\n"),
            "no kind suffix when kind is None"
        );

        // Section ordering: User notes → Links.
        let notes_idx = out.find("User notes (ground truth)").expect("user notes");
        let links_idx = out.find("## Links").expect("links section");
        assert!(notes_idx < links_idx);
    }

    #[test]
    fn format_workstream_detail_skips_links_section_when_empty() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_no_links".into(),
                title: "Empty".into(),
                summary: "".into(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W5", &detail, &team_map);
        assert!(!out.contains("## Links"));
    }

    #[test]
    fn format_workstream_detail_appends_link_summary_when_set() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_links".into(),
                title: "Bridge".into(),
                summary: "".into(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 1,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: vec![
                crate::workstreams::WorkstreamLink {
                    id: "wsl_1".into(),
                    workstream_id: "ws_links".into(),
                    label: "Repo".into(),
                    url: "https://github.com/x/y".into(),
                    kind: Some("github".into()),
                    position: 0,
                    created_ms: 0,
                    summary: Some("A small Rust crate for X.".into()),
                },
                crate::workstreams::WorkstreamLink {
                    id: "wsl_2".into(),
                    workstream_id: "ws_links".into(),
                    label: "Spec".into(),
                    url: "https://docs.example.com".into(),
                    kind: None,
                    position: 1,
                    created_ms: 0,
                    summary: None,
                },
            ],
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W6", &detail, &team_map);
        assert!(out.contains(
            "- [Repo](https://github.com/x/y) (github) — A small Rust crate for X.\n"
        ));
        // No summary suffix on the second link.
        assert!(out.contains("- [Spec](https://docs.example.com)\n"));
    }

    #[test]
    fn format_workstream_detail_renders_external_line() {
        let mut workstream = make_ws("ws_e", "Hyundai POC", "", 0);
        workstream.external_participants = vec![
            crate::workstreams::ExternalParticipant {
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
                count: 3,
            },
            crate::workstreams::ExternalParticipant {
                email: "bob@example.com".into(),
                display_name: None,
                count: 1,
            },
        ];
        let detail = WorkstreamDetail {
            workstream,
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W6", &detail, &team_map);
        assert!(out.contains("External: Alice <alice@example.com>, bob@example.com\n"));
    }

    #[test]
    fn format_workstream_detail_skips_external_line_when_empty() {
        let workstream = make_ws("ws_e", "Hyundai POC", "", 0);
        let detail = WorkstreamDetail {
            workstream,
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W7", &detail, &team_map);
        assert!(!out.contains("External:"));
    }

    fn make_child(id: &str, title: &str, summary: &str) -> Workstream {
        make_ws(id, title, summary, 0)
    }

    #[test]
    fn format_workstream_detail_emits_children_section_after_notes() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_bridge".into(),
                title: "ELAN AI Bridge".into(),
                summary: "Umbrella for Bridge sub-threads.".into(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: vec![
                make_child("ws_talgo", "Talgo demo", "Vendor evaluation."),
                make_child("ws_comptia", "CompTIA setup", "Onboarding."),
            ],
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W8", &detail, &team_map);

        assert!(out.contains("## Children"));
        assert!(out.contains("- [ws_talgo] Talgo demo — Vendor evaluation."));
        assert!(out.contains("- [ws_comptia] CompTIA setup — Onboarding."));
    }

    #[test]
    fn format_workstream_detail_skips_children_section_when_empty() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_leaf".into(),
                title: "Leaf".into(),
                summary: "".into(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                user_notes: None,
                archived_at_ms: None,
                reopened_at_ms: None,
                owner_member_id: None,
                members: Vec::new(),
                email_count: 0,
                event_count: 0,
                note_count: 0,
                link_count: 0,
                parent_workstream_id: None,
                external_participants: Vec::new(),
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            links: Vec::new(),
            teams_messages: Vec::new(),
            children: Vec::new(),
        };
        let team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let out = format_workstream_detail("W9", &detail, &team_map);
        assert!(!out.contains("## Children"));
    }

    // ----- Teams messages section (#136) ----------------------------------

    fn make_teams_message(
        id: &str,
        chat_id: &str,
        chat_topic: Option<&str>,
        from_name: Option<&str>,
        sent_at_ms: i64,
        preview: &str,
    ) -> crate::connectors::teams::TeamsMessage {
        crate::connectors::teams::TeamsMessage {
            id: id.to_string(),
            connector_id: "mg:test".to_string(),
            external_id: id.to_string(),
            chat_id: chat_id.to_string(),
            chat_kind: "group".to_string(),
            chat_topic: chat_topic.map(|s| s.to_string()),
            sent_at_ms,
            from_aad_id: None,
            from_email: Some("from@example.com".to_string()),
            from_name: from_name.map(|s| s.to_string()),
            body_html: None,
            body_preview: Some(preview.to_string()),
            reply_to_id: None,
            modified_ms: sent_at_ms,
            raw_etag: None,
        }
    }

    #[test]
    fn format_teams_messages_section_renders_labels_and_preview() {
        let msgs = vec![
            make_teams_message("m1", "c1", Some("Operations"), Some("Heike"), 100, "hey, ping"),
            make_teams_message("m2", "c2", None, Some("Markus"), 50, "DM body"),
            make_teams_message("m3", "c1", Some("Operations"), Some("Heike"), 30, "older one"),
        ];
        let out = format_teams_messages_section(&msgs);
        assert!(out.starts_with("# Recent Teams messages (last 14 days)\n"));
        assert!(out.contains("[T1]"), "first label missing: {out}");
        assert!(out.contains("[T2]"), "second label missing: {out}");
        assert!(out.contains("[T3]"), "third label missing: {out}");
        assert!(out.contains("@Heike"), "sender missing: {out}");
        assert!(out.contains("@Markus"), "second sender missing: {out}");
        assert!(out.contains("in \"Operations\""), "chat topic missing: {out}");
        assert!(out.contains("hey, ping"), "preview missing: {out}");
        // Topic-less row should NOT carry an `in "..."` clause.
        let dm_line = out.lines().find(|l| l.starts_with("[T2]")).unwrap_or("");
        assert!(
            !dm_line.contains(" in \""),
            "topic-less DM row should not have `in \"...\"`: {dm_line}"
        );
    }

    #[test]
    fn format_user_message_includes_teams_section_when_messages_exist() {
        let directory = vec![DirectoryEntry {
            note_path: "n1".to_string(),
            bundle_id: "n1".to_string(),
            title: "Note A".to_string(),
            modified_ms: 100,
            preview: "p".to_string(),
        }];
        let retrieved: std::collections::HashSet<String> = std::collections::HashSet::new();
        let profiles: Vec<(crate::team::TeamMember, String)> = Vec::new();
        let schedule: Vec<crate::connectors::calendar::CalendarEvent> = Vec::new();
        let workstreams: Vec<Workstream> = Vec::new();
        let msgs = vec![make_teams_message(
            "m1",
            "c1",
            Some("Operations"),
            Some("Heike"),
            100,
            "ping",
        )];
        let out = format_user_message(
            "q?", &directory, &retrieved, &profiles, &schedule, &workstreams, &msgs, &[],
        );
        assert!(
            out.contains("# Recent Teams messages"),
            "expected Teams section, got:\n{out}"
        );
        assert!(out.contains("[T1] @Heike"), "expected T1 row: {out}");
    }

    #[test]
    fn format_user_message_omits_teams_section_when_empty() {
        let directory = vec![DirectoryEntry {
            note_path: "n1".to_string(),
            bundle_id: "n1".to_string(),
            title: "Note A".to_string(),
            modified_ms: 100,
            preview: "p".to_string(),
        }];
        let out = format_user_message(
            "q?",
            &directory,
            &std::collections::HashSet::new(),
            &[],
            &[],
            &[],
            &[],
            &[],
        );
        assert!(
            !out.contains("# Recent Teams messages"),
            "should omit header when no messages: {out}"
        );
    }

    #[test]
    fn render_teams_tool_output_returns_body_for_valid_n() {
        let msgs = vec![
            make_teams_message("m1", "c1", Some("Op"), Some("Heike"), 100, "first"),
            make_teams_message("m2", "c1", Some("Op"), Some("Heike"), 200, "TARGET"),
        ];
        let result = render_teams_tool_output(2, &msgs, &[], &[]);
        assert!(!result.is_error, "expected ok result");
        assert!(result.content.contains("[T2]"), "label missing: {}", result.content);
        assert!(result.content.contains("@Heike"), "sender missing: {}", result.content);
        assert!(result.content.contains("TARGET"), "body missing: {}", result.content);
    }

    #[test]
    fn render_teams_tool_output_errors_on_out_of_range() {
        let msgs = vec![make_teams_message(
            "m1", "c1", Some("Op"), Some("Heike"), 100, "only",
        )];
        let result = render_teams_tool_output(99, &msgs, &[], &[]);
        assert!(result.is_error, "expected error result");
        assert!(
            result.content.contains("[T99]") && result.content.contains("out of range"),
            "error string should mention label + range: {}",
            result.content
        );
    }

    #[test]
    fn render_teams_tool_output_renders_surrounding_context_chronologically() {
        let target = make_teams_message("m2", "c1", Some("Op"), Some("Heike"), 200, "TARGET");
        // `before` arrives DESC (the SQL query orders newest-first).
        let before = vec![
            make_teams_message("m1b", "c1", Some("Op"), Some("Tom"), 150, "right before"),
            make_teams_message("m0", "c1", Some("Op"), Some("Heike"), 50, "way before"),
        ];
        let after = vec![make_teams_message(
            "m3", "c1", Some("Op"), Some("Tom"), 250, "ack",
        )];
        let result = render_teams_tool_output(1, &[target], &before, &after);
        assert!(!result.is_error);
        // Chronological order: way before → right before → after.
        let pos_way = result.content.find("way before").unwrap();
        let pos_right = result.content.find("right before").unwrap();
        let pos_ack = result.content.find("ack").unwrap();
        assert!(pos_way < pos_right, "before-context order wrong: {}", result.content);
        assert!(pos_right < pos_ack, "after-context order wrong: {}", result.content);
    }

    #[test]
    fn teams_chip_title_handles_missing_sender_and_preview() {
        let mut m = make_teams_message("m1", "c1", None, Some("Heike"), 100, "");
        m.body_preview = None;
        // Sender present, no preview → "@Heike".
        assert_eq!(teams_chip_title(&m), "@Heike");
        // No sender name AND no email → falls back to "unknown".
        let mut bare = make_teams_message("m2", "c2", None, None, 100, "hi");
        bare.from_email = None;
        assert!(teams_chip_title(&bare).starts_with("@unknown"));
    }

    #[test]
    fn strip_html_to_text_basic() {
        let html = "<div>Hello <b>world</b><br/>How are <i>you</i>?</div>";
        let out = strip_html_to_text(html);
        assert_eq!(out, "Hello world How are you ?");
    }

    // ----- Prompt-inspector dumps (#134) ----------------------------------

    fn dump_test_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); \
             INSERT INTO meta(key, value) VALUES ('schema_version', '35');",
        )
        .unwrap();
        conn.execute_batch(include_str!("migrations/036_prompt_dumps.sql"))
            .unwrap();
        conn.execute_batch(include_str!("migrations/037_prompt_dumps_telemetry.sql"))
            .unwrap();
        conn.execute_batch(include_str!("migrations/038_prompt_cache_tokens.sql"))
            .unwrap();
        // `list_chat_turn_metrics` joins `chat_messages`; create a
        // minimal schema (no FK, simpler than running migration 035)
        // so the LEFT JOIN has a target even when no rows are seeded.
        conn.execute_batch(
            "CREATE TABLE chat_messages (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                sources_json TEXT,
                tool_calls_json TEXT,
                turn_id TEXT,
                created_ms INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn make_source(kind: AskSourceKind, label: &str, title: &str) -> AskSource {
        AskSource {
            kind,
            label: label.to_string(),
            title: title.to_string(),
            modified_ms: 100,
            note_path: None,
            bundle_id: None,
            event_id: None,
            workstream_id: None,
            teams_message_id: None,
            email_id: None,
        }
    }

    fn make_dispatch(name: &str, content: &str, is_error: bool) -> DispatchRecord {
        DispatchRecord {
            tool_name: name.to_string(),
            input: serde_json::json!({"n": 1}),
            content: content.to_string(),
            is_error,
            duration_ms: 42,
        }
    }

    #[test]
    fn write_prompt_dump_round_trips() {
        let conn = dump_test_db();
        let sources = vec![make_source(AskSourceKind::Note, "1", "Note A")];
        let dispatches = vec![make_dispatch("read_note", "body of note", false)];
        write_prompt_dump(
            &conn,
            "turn_x",
            "the prompt body",
            &sources,
            &dispatches,
            1234,
            9_000,
            "what's up",
            Some(500),
            Some(120),
            Some(15_000),
            Some(2_000),
        )
        .unwrap();

        let dump = read_prompt_dump(&conn, "turn_x").unwrap().expect("row");
        assert_eq!(dump.turn_id, "turn_x");
        assert_eq!(dump.prompt, "the prompt body");
        assert_eq!(dump.latency_ms, 1234);
        assert_eq!(dump.created_ms, 9_000);
        assert_eq!(dump.query, "what's up");
        assert_eq!(dump.tokens_in, Some(500));
        assert_eq!(dump.tokens_out, Some(120));
        assert_eq!(dump.cache_creation_tokens, Some(15_000));
        assert_eq!(dump.cache_read_tokens, Some(2_000));
        assert!(dump.system_prompt.contains("personal notes"), "system_prompt captured");
        assert!(dump.tool_names.iter().any(|n| n == "read_teams_message"), "tool names list");
        assert_eq!(dump.sources[0]["label"].as_str(), Some("1"));
        assert_eq!(
            dump.dispatches[0]["tool_name"].as_str(),
            Some("read_note")
        );
        assert_eq!(
            dump.dispatches[0]["content"].as_str(),
            Some("body of note")
        );
    }

    #[test]
    fn write_prompt_dump_overwrites_on_retry() {
        let conn = dump_test_db();
        let sources_a = vec![make_source(AskSourceKind::Note, "1", "First")];
        let sources_b = vec![make_source(AskSourceKind::Note, "2", "Second")];
        write_prompt_dump(
            &conn, "turn_x", "first attempt", &sources_a, &[], 100, 1_000, "q1",
            None, None, None, None,
        )
        .unwrap();
        write_prompt_dump(
            &conn, "turn_x", "second attempt", &sources_b, &[], 200, 2_000, "q2",
            Some(900), Some(50), None, None,
        )
        .unwrap();
        let dump = read_prompt_dump(&conn, "turn_x").unwrap().expect("row");
        assert_eq!(dump.prompt, "second attempt");
        assert_eq!(dump.latency_ms, 200);
        assert_eq!(dump.sources[0]["label"].as_str(), Some("2"));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM prompt_dumps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "ON CONFLICT keeps a single row per turn_id");
    }

    #[test]
    fn read_prompt_dump_returns_none_for_missing_turn() {
        let conn = dump_test_db();
        assert!(read_prompt_dump(&conn, "no_such_turn").unwrap().is_none());
    }

    #[test]
    fn tool_names_const_matches_definitions() {
        // Guards against drift: every name in TOOL_NAMES must appear
        // as a tool in tool_definitions(), and vice versa. The
        // inspector lists TOOL_NAMES as "what the model could have
        // called"; if the lists diverge the inspector lies.
        let defs = tool_definitions();
        let defined: std::collections::HashSet<String> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        let listed: std::collections::HashSet<String> =
            TOOL_NAMES.iter().map(|n| n.to_string()).collect();
        assert_eq!(defined, listed, "TOOL_NAMES out of sync with tool_definitions()");
    }

    // ----- #135 telemetry -------------------------------------------------

    #[test]
    fn extract_citations_picks_up_all_label_shapes() {
        let text = "First note [1] then event [E12] then workstream [W3] then teams [T7].";
        assert_eq!(extract_citations(text), vec!["1", "E12", "W3", "T7"]);
    }

    #[test]
    fn extract_citations_dedupes_in_first_appearance_order() {
        let text = "Cite [1] again [E2] and again [1] and [E2] and finally [W9].";
        assert_eq!(extract_citations(text), vec!["1", "E2", "W9"]);
    }

    #[test]
    fn extract_citations_skips_malformed_brackets() {
        // No digits, too many digits, missing closing bracket, lowercase prefix.
        let text = "[] [1234] [E12 [w9] [T999]";
        assert_eq!(extract_citations(text), vec!["T999"]);
    }

    /// Seed a `chat_messages` row associated with a prompt dump's
    /// turn_id so the metrics join produces a complete row.
    fn seed_chat_message_for_metric(
        conn: &rusqlite::Connection,
        turn_id: &str,
        text: &str,
    ) {
        conn.execute(
            "INSERT INTO chat_messages(id, conversation_id, role, text, turn_id, created_ms) \
             VALUES (?1, 'conv_1', 'assistant', ?2, ?3, 1000)",
            rusqlite::params![format!("msg_{turn_id}"), text, turn_id],
        )
        .unwrap();
    }

    #[test]
    fn list_chat_turn_metrics_joins_and_orders_correctly() {
        let conn = dump_test_db();
        // Two dumps at different times + matching chat messages.
        let sources = vec![make_source(AskSourceKind::Note, "1", "A")];
        write_prompt_dump(
            &conn, "turn_a", "p1", &sources, &[], 50, 1_000, "q-a",
            Some(100), Some(20), None, None,
        )
        .unwrap();
        write_prompt_dump(
            &conn, "turn_b", "p2", &sources, &[], 75, 2_000, "q-b",
            Some(200), Some(30), None, None,
        )
        .unwrap();
        seed_chat_message_for_metric(&conn, "turn_a", "see [1]");
        seed_chat_message_for_metric(&conn, "turn_b", "no citations here");

        let metrics = read_chat_turn_metrics(&conn, 10).unwrap();
        assert_eq!(metrics.len(), 2);
        // DESC by created_ms.
        assert_eq!(metrics[0].turn_id, "turn_b");
        assert_eq!(metrics[1].turn_id, "turn_a");
        assert_eq!(metrics[1].citations, vec!["1"]);
        assert!(metrics[0].citations.is_empty());
        assert_eq!(metrics[0].assistant_text_chars, "no citations here".len() as i64);
        assert_eq!(metrics[0].query, "q-b");
        assert_eq!(metrics[0].tokens_in, Some(200));
    }

    #[test]
    fn list_chat_turn_metrics_handles_missing_chat_message() {
        let conn = dump_test_db();
        write_prompt_dump(
            &conn, "orphan", "p", &[], &[], 10, 1_000, "q",
            Some(5), Some(2), None, None,
        )
        .unwrap();
        // No matching chat_messages row → assistant_text_chars = 0,
        // citations = [], but the dump fields still come through.
        let metrics = read_chat_turn_metrics(&conn, 10).unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].turn_id, "orphan");
        assert_eq!(metrics[0].assistant_text_chars, 0);
        assert!(metrics[0].citations.is_empty());
        assert_eq!(metrics[0].query, "q");
    }

    #[test]
    fn list_chat_turn_metrics_counts_sources_and_dispatches() {
        let conn = dump_test_db();
        let sources = vec![
            make_source(AskSourceKind::Note, "1", "A"),
            make_source(AskSourceKind::Note, "2", "B"),
            make_source(AskSourceKind::Event, "E1", "X"),
            make_source(AskSourceKind::TeamsMessage, "T1", "Y"),
        ];
        let dispatches = vec![
            make_dispatch("read_note", "body", false),
            make_dispatch("read_event_details", "evt", true),  // error!
        ];
        write_prompt_dump(
            &conn, "turn_z", "p", &sources, &dispatches, 100, 1_000, "q",
            None, None, None, None,
        )
        .unwrap();
        let metrics = read_chat_turn_metrics(&conn, 10).unwrap();
        assert_eq!(metrics[0].sources_total, 4);
        assert_eq!(metrics[0].sources_by_kind["note"], 2);
        assert_eq!(metrics[0].sources_by_kind["event"], 1);
        assert_eq!(metrics[0].sources_by_kind["teams_message"], 1);
        assert_eq!(metrics[0].tool_call_count, 2);
        assert!(metrics[0].had_error_dispatch);
    }

    // ----- #142 prompt caching --------------------------------------------

    /// Mirrors the SSE message_start parser's view of the usage payload.
    /// Returned tuple: (input, output, cache_creation, cache_read). Keeps
    /// the parsing logic testable without needing a real HTTP roundtrip.
    fn parse_usage_for_test(usage: &serde_json::Value) -> (i64, i64, i64, i64) {
        let input = usage
            .get("input_tokens")
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        let creation = usage
            .get("cache_creation_input_tokens")
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        let read = usage
            .get("cache_read_input_tokens")
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        (input, output, creation, read)
    }

    #[test]
    fn usage_parse_captures_all_four_fields_when_present() {
        let usage = serde_json::json!({
            "input_tokens": 234,
            "output_tokens": 567,
            "cache_creation_input_tokens": 15_000,
            "cache_read_input_tokens": 2_500,
        });
        let (input, output, creation, read) = parse_usage_for_test(&usage);
        assert_eq!(input, 234);
        assert_eq!(output, 567);
        assert_eq!(creation, 15_000);
        assert_eq!(read, 2_500);
    }

    #[test]
    fn usage_parse_handles_absent_cache_fields_as_zero() {
        // On a cache miss Anthropic omits the cache_* fields entirely.
        // Confirmed against the docs: absent (not zero).
        let usage = serde_json::json!({
            "input_tokens": 17_000,
            "output_tokens": 412,
        });
        let (_input, _output, creation, read) = parse_usage_for_test(&usage);
        assert_eq!(creation, 0);
        assert_eq!(read, 0);
    }

    #[test]
    fn split_at_question_marker_when_present() {
        let s = "# Notes directory\n\n[1] foo\n\n# Question\n\nDoes she need something?";
        let (context, question) = split_at_question_marker(s);
        assert_eq!(context, "# Notes directory\n\n[1] foo\n\n");
        assert_eq!(question, "# Question\n\nDoes she need something?");
    }

    #[test]
    fn split_at_question_marker_falls_through_when_absent() {
        let s = "no marker here";
        let (context, question) = split_at_question_marker(s);
        assert_eq!(context, "no marker here");
        assert!(question.is_empty());
    }

    #[test]
    fn split_at_question_marker_uses_rfind_for_user_pasted_markers() {
        // If a user pastes "# Question" inside their query, we still
        // want the LAST occurrence to win (the real section header).
        let s = "# Question\n\nuser said \"# Question\\n\\n\" in their text\n\n# Question\n\nreal one";
        let (_context, question) = split_at_question_marker(s);
        assert!(question.ends_with("real one"), "rfind picks the last marker: {}", question);
    }

    #[test]
    fn content_block_text_with_cache_control_serializes_field() {
        let block = ContentBlock::Text {
            text: "hi".into(),
            cache_control: Some(CacheControl { kind: "ephemeral" }),
        };
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains("\"cache_control\":{\"type\":\"ephemeral\"}"), "got: {s}");
    }

    #[test]
    fn content_block_text_without_cache_control_omits_field() {
        let block = ContentBlock::Text {
            text: "hi".into(),
            cache_control: None,
        };
        let s = serde_json::to_string(&block).unwrap();
        assert!(!s.contains("cache_control"), "field should be omitted: {s}");
    }

    #[test]
    fn content_block_tool_result_supports_cache_control() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tool_1".into(),
            content: "result".into(),
            is_error: false,
            cache_control: Some(CacheControl { kind: "ephemeral" }),
        };
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains("\"cache_control\":{\"type\":\"ephemeral\"}"), "got: {s}");
        // is_error=false is skipped per the existing serde attr.
        assert!(!s.contains("is_error"), "is_error=false should be omitted: {s}");
    }

    // ----- Unread emails section + tool (#137) ----------------------------

    fn make_email_for_test(
        id: &str,
        thread_id: &str,
        from_email: &str,
        from_name: Option<&str>,
        subject: &str,
        sent_at_ms: i64,
        preview: &str,
    ) -> crate::connectors::email::EmailMessage {
        crate::connectors::email::EmailMessage {
            id: id.to_string(),
            connector_id: "mg:test".to_string(),
            external_id: id.to_string(),
            thread_id: thread_id.to_string(),
            subject: subject.to_string(),
            from_email: from_email.to_string(),
            from_name: from_name.map(|s| s.to_string()),
            sent_at_ms,
            body_preview: Some(preview.to_string()),
            body_html: None,
            has_attachments: false,
            is_read: false,
            raw_etag: None,
            modified_ms: sent_at_ms,
            recipients: Vec::new(),
        }
    }

    #[test]
    fn format_unread_emails_section_renders_labels_subjects_and_previews() {
        let msgs = vec![
            make_email_for_test(
                "e1",
                "t1",
                "heike@x.io",
                Some("Heike Epple"),
                "Workspace Migration",
                100,
                "needs to move by friday",
            ),
            make_email_for_test(
                "e2",
                "t2",
                "ext@vendor.com",
                None,
                "Quote request",
                50,
                "attached",
            ),
        ];
        let mut team_emails: std::collections::HashSet<String> = std::collections::HashSet::new();
        team_emails.insert("heike@x.io".to_string());
        let out = format_unread_emails_section(&msgs, &team_emails);
        assert!(
            out.starts_with("# Recent emails awaiting attention (last 14 days,"),
            "section header missing: {out}"
        );
        assert!(out.contains("[U1] from Heike Epple (team)"), "U1 team marker: {out}");
        assert!(out.contains("\"Workspace Migration\""), "subject missing: {out}");
        assert!(out.contains("needs to move by friday"), "preview missing: {out}");
        // Non-team sender — no `(team)` marker.
        assert!(out.contains("[U2] from ext@vendor.com ("), "U2 fallback sender: {out}");
        assert!(!out.contains("[U2] from ext@vendor.com (team)"), "no team for U2");
    }

    #[test]
    fn format_unread_emails_section_handles_missing_subject_and_preview() {
        let msgs = vec![make_email_for_test(
            "e1", "t1", "x@y.io", Some("X"), "", 100, "",
        )];
        let team_emails: std::collections::HashSet<String> = std::collections::HashSet::new();
        let out = format_unread_emails_section(&msgs, &team_emails);
        // Subject + preview clauses elided when both empty.
        let line = out.lines().find(|l| l.starts_with("[U1]")).unwrap_or("");
        assert!(line.starts_with("[U1] from X ("), "expected sender prefix: {line}");
        assert!(!line.contains("\"\""), "should not emit empty subject quotes: {line}");
    }

    #[test]
    fn render_email_tool_output_returns_body_for_valid_n() {
        let msgs = vec![
            make_email_for_test("a", "t1", "alice@x.io", Some("Alice"), "First", 100, "p1"),
            make_email_for_test("b", "t2", "bob@x.io", Some("Bob"), "TARGET", 200, "p2"),
        ];
        let result = render_email_tool_output(2, &msgs, &[]);
        assert!(!result.is_error, "expected ok result");
        assert!(result.content.contains("[U2]"), "label: {}", result.content);
        assert!(result.content.contains("Subject: TARGET"), "subject: {}", result.content);
        assert!(result.content.contains("p2"), "body (preview fallback): {}", result.content);
    }

    #[test]
    fn render_email_tool_output_errors_on_out_of_range() {
        let msgs = vec![make_email_for_test(
            "a", "t1", "alice@x.io", Some("Alice"), "Hi", 100, "p1",
        )];
        let r = render_email_tool_output(0, &msgs, &[]);
        assert!(r.is_error, "n=0 should error");
        let r = render_email_tool_output(5, &msgs, &[]);
        assert!(r.is_error, "out-of-range n should error");
        assert!(r.content.contains("[U5]"), "error message references label: {}", r.content);
    }

    #[test]
    fn render_email_tool_output_includes_thread_with_target_marker() {
        let msgs = vec![make_email_for_test(
            "b", "t1", "bob@x.io", Some("Bob"), "Re: Plan", 200, "current",
        )];
        let thread = vec![
            make_email_for_test("a", "t1", "tj@x.io", Some("TJ"), "Plan", 100, "first"),
            make_email_for_test("b", "t1", "bob@x.io", Some("Bob"), "Re: Plan", 200, "current"),
            make_email_for_test("c", "t1", "tj@x.io", Some("TJ"), "Re: Plan", 300, "ack"),
        ];
        let r = render_email_tool_output(1, &msgs, &thread);
        assert!(!r.is_error);
        assert!(
            r.content.contains("Thread (chronological"),
            "thread block missing: {}",
            r.content
        );
        assert!(r.content.contains("► "), "target marker missing: {}", r.content);
        assert!(r.content.contains("from TJ"), "thread sender missing: {}", r.content);
    }

    #[test]
    fn email_chip_title_falls_back_to_email_when_name_missing() {
        let m = make_email_for_test("e1", "t1", "x@y.io", None, "Hello", 1, "p");
        assert_eq!(email_chip_title(&m), "@x@y.io: Hello");
        let m2 = make_email_for_test("e2", "t2", "a@b.io", Some("Alice"), "", 1, "");
        assert_eq!(email_chip_title(&m2), "@Alice");
    }

    // ----- Event series tool + summary (#128) ---------------------------

    fn make_event_for_series_test(
        id: &str,
        title: &str,
        start_ms: i64,
        series_master_id: Option<&str>,
        attendees: Vec<(&str, &str, bool)>, // (email, name, is_self)
    ) -> crate::connectors::calendar::CalendarEvent {
        crate::connectors::calendar::CalendarEvent {
            id: id.to_string(),
            connector_id: "mg:test".to_string(),
            external_id: id.to_string(),
            title: title.to_string(),
            start_ms,
            end_ms: start_ms + 30 * 60 * 1000,
            all_day: false,
            location: None,
            description: None,
            source_calendar: None,
            status: None,
            raw_etag: None,
            modified_ms: start_ms,
            linked_note_id: None,
            series_master_id: series_master_id.map(|s| s.to_string()),
            attendees: attendees
                .into_iter()
                .map(|(email, name, is_self)| crate::connectors::calendar::CalendarAttendee {
                    email: email.to_string(),
                    display_name: Some(name.to_string()),
                    response_status: None,
                    is_self,
                    is_organizer: false,
                    team_member_id: None,
                })
                .collect(),
        }
    }

    #[test]
    fn format_series_summary_counts_steady_members_and_window() {
        // 4 occurrences. Alice attends all 4, Bob attends 3, Eve attends 1.
        // Steady cutoff at 50% (= 2/4); Alice + Bob qualify, Eve does not.
        let occs = vec![
            make_event_for_series_test(
                "o1",
                "Standup",
                1_000,
                Some("m1"),
                vec![("alice@x.io", "Alice", false), ("bob@x.io", "Bob", false)],
            ),
            make_event_for_series_test(
                "o2",
                "Standup",
                2_000,
                Some("m1"),
                vec![("alice@x.io", "Alice", false), ("bob@x.io", "Bob", false)],
            ),
            make_event_for_series_test(
                "o3",
                "Standup",
                3_000,
                Some("m1"),
                vec![("alice@x.io", "Alice", false), ("eve@x.io", "Eve", false)],
            ),
            make_event_for_series_test(
                "o4",
                "Standup",
                4_000,
                Some("m1"),
                vec![("alice@x.io", "Alice", false), ("bob@x.io", "Bob", false)],
            ),
        ];
        let s = format_series_summary(&occs);
        assert!(s.contains("Total occurrences known: 4"), "total: {s}");
        assert!(s.contains("Steady members"), "section header: {s}");
        assert!(s.contains("Alice"), "Alice steady: {s}");
        assert!(s.contains("(4/4)"), "Alice count: {s}");
        assert!(s.contains("Bob"), "Bob steady: {s}");
        assert!(s.contains("(3/4)"), "Bob count: {s}");
        assert!(!s.contains("Eve"), "Eve below cutoff: {s}");
    }

    #[test]
    fn format_series_summary_excludes_self_attendee() {
        let occs = vec![make_event_for_series_test(
            "o1",
            "1:1",
            1_000,
            Some("m1"),
            vec![("me@x.io", "Self", true), ("h@x.io", "Heike", false)],
        )];
        let s = format_series_summary(&occs);
        assert!(s.contains("Heike"), "Heike present: {s}");
        assert!(!s.contains("Self"), "self excluded: {s}");
    }

    #[test]
    fn render_event_series_output_emits_per_occurrence_attendees() {
        let anchor = make_event_for_series_test(
            "o2",
            "Standup",
            2_000,
            Some("m1"),
            vec![("alice@x.io", "Alice", false)],
        );
        let series = vec![
            make_event_for_series_test(
                "o1",
                "Standup",
                1_000,
                Some("m1"),
                vec![("alice@x.io", "Alice", false)],
            ),
            anchor.clone(),
        ];
        let r = render_event_series_output(3, &anchor, &series);
        assert!(!r.is_error, "ok result");
        assert!(r.content.contains("# Series for [E3] Standup"), "anchor header: {}", r.content);
        assert!(r.content.contains("Total known occurrences: 2"), "total: {}", r.content);
        assert!(r.content.contains("## Occurrences"), "occurrences block: {}", r.content);
        // Each occurrence renders its attendee list.
        assert!(r.content.contains("Alice <alice@x.io>"), "attendee: {}", r.content);
    }

    #[test]
    fn render_event_series_output_handles_empty_series_softly() {
        let anchor = make_event_for_series_test("o1", "Solo", 1_000, Some("m1"), vec![]);
        let r = render_event_series_output(1, &anchor, &[]);
        // Not an error — the caller asked but the series is empty. We
        // tell the model so it can fall back to read_event_details.
        assert!(!r.is_error, "soft fallthrough: {}", r.content);
        assert!(
            r.content.contains("no occurrences are stored"),
            "guidance present: {}",
            r.content
        );
    }
}
