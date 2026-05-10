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
/// Per-category top-N when expanding a workstream via `read_workstream`
/// (emails / events / notes returned per call).
const WORKSTREAM_DETAIL_TOP_N: usize = 5;
/// Per-workstream summary cap when listed in the prompt section.
const WORKSTREAM_SUMMARY_CAP: usize = 200;

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
    /// Set when `kind == Workstream`. Frontend dispatches a
    /// `margin:open-workstream` event with this id on chip click (#72).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workstream_id: Option<String>,
}

/// One past turn in the conversation, threaded back to the model.
/// Frontend only ever stores text content; we wrap it in a single
/// text content block when composing the API request.
#[derive(Deserialize, Clone)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

const SYSTEM_PROMPT: &str = "You are answering questions about the user's personal notes (meeting \
notes, hand-typed notes, transcripts), their team profiles, and their calendar.

The user's message contains up to five sections:

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

5. **Workstreams** — synthesized clusters of related emails, meetings, and notes labeled `[W1]`, \
`[W2]`, etc. Each entry has a title, one-line summary, and item counts. Workstreams are the right \
citation when the answer IS the ongoing thread itself (\"how's the Hyundai POC going?\"); when \
citing a *specific* item within a workstream, prefer the underlying `[N]` / `[E<N>]` label if one \
is available.

You have four tools for digging deeper:
- **`read_note(n)`** — returns the full markdown body of directory entry `[n]`. Use when a preview \
hints at relevance but you need the body to answer.
- **`read_transcript(n)`** — returns the meeting transcript text for `[n]`, if it has audio. Use \
when the question is likely about something said in a meeting but not captured in the typed body.
- **`read_event_details(n)`** — returns the full attendee list, description, location, and exact \
times for event `[E<n>]`. Use when answering questions about meeting participants or content. \
Pass the integer after the `E` as `n` (e.g. for `[E3]` call `read_event_details(3)`).
- **`read_workstream(n)`** — returns the workstream's full summary, all open + recently completed \
actions, and the most recent emails / events / notes that belong to it. Use for status questions \
(\"what's happening with X?\"). Pass the integer after the `W` as `n` (e.g. for `[W2]` call \
`read_workstream(2)`).

Use tools sparingly — most questions can be answered from the directory + top candidates + schedule \
already in context. Don't speculate; call a tool if you genuinely need the content. Up to 6 tool \
calls per question; after that you must answer with what you have.

Rules:
- Answer in natural prose. Be specific and concise — 1-4 short paragraphs unless the question asks \
for a list.
- Cite sources inline with `[N]` (notes), `[E<N>]` (events), or `[W<N>]` (workstreams) \
immediately after each claim that came from one. Multiple citations: `[1][3]` or `[E1][E2]` or \
`[W2][N1]` or any mix. Never make up citation labels — only use ones you actually received.
- For \"when did we first…\" questions, identify the *earliest* dated note that matches and cite it.
- If neither the notes nor the profiles nor the schedule contain the answer, say so clearly. \
Don't speculate.
- Don't pad with caveats or restate the question. Open with the answer.
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
    // schedule window + active workstreams in one lock. Profile.md
    // content is read off-lock below.
    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let now_ms = current_unix_ms();
    let (directory, retrieved_paths, team, schedule, workstreams) = {
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
        (directory, retrieved_paths, team, schedule, workstreams)
    };

    // Build the citation surface: every directory entry gets a 1-based
    // [N] label, every schedule entry gets an [E<N>] label, every
    // workstream gets a [W<N>] label.
    let mut sources: Vec<AskSource> =
        Vec::with_capacity(directory.len() + schedule.len() + workstreams.len());
    for (i, e) in directory.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::Note,
            label: (i + 1).to_string(),
            note_path: Some(e.note_path.clone()),
            bundle_id: Some(e.bundle_id.clone()),
            event_id: None,
            workstream_id: None,
            title: e.title.clone(),
            modified_ms: e.modified_ms,
        });
    }
    for (i, e) in schedule.iter().enumerate() {
        sources.push(AskSource {
            kind: AskSourceKind::Event,
            label: format!("E{}", i + 1),
            note_path: e.linked_note_path.clone(),
            bundle_id: None,
            event_id: Some(e.id.clone()),
            workstream_id: None,
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
            title: w.title.clone(),
            modified_ms: w.last_activity_ms,
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

    // Read profile.md contents off-lock (small file IO, no need to
    // hold the SQLite mutex). Failure on any one profile degrades to
    // an empty excerpt — the model still has the display_name + aliases.
    let mut profile_excerpts: Vec<(crate::team::TeamMember, String)> =
        Vec::with_capacity(team.len());
    for m in team {
        let body = match tokio::fs::read_to_string(&m.profile_md_path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[ask] read profile {} failed: {e}",
                    m.profile_md_path
                );
                String::new()
            }
        };
        let excerpt = truncate_chars(body.trim(), PER_PROFILE_CAP);
        profile_excerpts.push((m, excerpt));
    }

    let user_message = format_user_message(
        &query,
        &directory,
        &retrieved_paths,
        &profile_excerpts,
        &schedule,
        &workstreams,
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
                content: vec![ContentBlock::Text { text: h.content }],
            });
        }
    }
    messages.push(ApiMessage {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: user_message,
        }],
    });

    let model = model.as_deref().unwrap_or(DEFAULT_MODEL).to_string();

    // Spawn the network + tool-use loop. Errors emit an `error` event
    // and exit; success emits deltas + a final `done` event.
    let app_bg = app.clone();
    let turn_id_bg = turn_id.clone();
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

#[derive(Serialize)]
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
/// via serde.
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
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
    },
}

fn is_false(b: &bool) -> bool {
    !*b
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
) -> Result<(), String> {
    let tools = tool_definitions();

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

        if pass.pending_tool_calls.is_empty() {
            // No tool calls — the model is done with this turn.
            let _ = app.emit(
                "ai-stream",
                StreamEvent::Done {
                    turn_id: turn_id.to_string(),
                },
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

        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(pass.pending_tool_calls.len());
        for tc in pass.pending_tool_calls {
            let target_n = tc
                .input
                .get("n")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let idx = target_n.saturating_sub(1) as usize;
            let (target_title, target_label, target_kind) = match tc.name.as_str() {
                "read_event_details" => (
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

            let result = dispatch_tool(app, &tc.name, &tc.input, directory, schedule, workstreams);

            let _ = app.emit(
                "ai-stream",
                StreamEvent::ToolUseDone {
                    turn_id: turn_id.to_string(),
                    tool_id: tc.id.clone(),
                    ok: !result.is_error,
                },
            );

            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: tc.id,
                content: result.content,
                is_error: result.is_error,
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
    let _ = stream_pass(app, turn_id, api_key, &final_body).await?;
    let _ = app.emit(
        "ai-stream",
        StreamEvent::Done {
            turn_id: turn_id.to_string(),
        },
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
            "name": "read_workstream",
            "description": "Read the full details of a workstream by its label [W<N>]. Returns the summary, all open and recently completed actions, and the most recent emails / events / notes that belong to this workstream. Use when the user is asking about ongoing work, status updates, or 'what's happening with X'. NOTE: the `n` argument is the integer after the `W` (e.g. for `[W3]` pass `n: 3`).",
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
        }
    ])
}

/// Result of one streaming round-trip.
struct PassResult {
    assistant_blocks: Vec<ContentBlock>,
    pending_tool_calls: Vec<PendingToolCall>,
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
                "message_delta" => {
                    // Carries stop_reason on the final delta. We don't
                    // need to act on it here — pending_tool_calls being
                    // non-empty is what drives the outer loop's next
                    // iteration.
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
                _ => {} // message_start, ping, etc.
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
                    assistant_blocks.push(ContentBlock::Text { text });
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
) -> ToolResult {
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
        return dispatch_read_event_details(n, schedule);
    }
    // read_workstream indexes the workstreams slice; needs the conn
    // mutex to load the joined detail.
    if name == "read_workstream" {
        return dispatch_read_workstream(app, n, workstreams);
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
/// can quote from. Includes the summary, all actions (open + recently
/// completed), and the top-N most recent items per category.
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

    let detail = {
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
        match crate::workstreams::persist::get_workstream_detail(&c, &ws.id) {
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
        }
    };

    ToolResult {
        content: format_workstream_detail(&label, &detail),
        is_error: false,
    }
}

fn format_workstream_detail(
    label: &str,
    detail: &crate::workstreams::WorkstreamDetail,
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

    // Actions: open first, then recently done.
    if !detail.actions.is_empty() {
        let open: Vec<&crate::workstreams::WorkstreamAction> =
            detail.actions.iter().filter(|a| !a.done).collect();
        let done: Vec<&crate::workstreams::WorkstreamAction> =
            detail.actions.iter().filter(|a| a.done).collect();
        s.push_str(&format!(
            "\nActions ({open_n} open, {done_n} done):\n",
            open_n = open.len(),
            done_n = done.len()
        ));
        for a in &open {
            s.push_str(&format!(
                "- [ ] {text} (from {kind})\n",
                text = a.text.trim(),
                kind = a.source_kind
            ));
        }
        for a in done.iter().take(WORKSTREAM_DETAIL_TOP_N) {
            s.push_str(&format!(
                "- [x] {text} (from {kind})\n",
                text = a.text.trim(),
                kind = a.source_kind
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

    s
}

/// Format a single calendar event into a structured text block the
/// model can quote from. Includes attendees with response statuses,
/// the linked-note pointer (if any), and a truncated description.
fn dispatch_read_event_details(
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
            .linked_note_path
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
                format!(" (aliases: {})", m.aliases.join(", "))
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

    if !workstreams.is_empty() {
        s.push_str(&format_workstreams_section(workstreams));
    }

    s.push_str("# Question\n\n");
    s.push_str(query.trim());
    s
}

/// Render synthesized workstreams as a labeled prompt section.
/// Each workstream becomes one line with its `[W<N>]` label, title,
/// one-line summary, and item counts. Empty input emits nothing — the
/// caller decides whether to skip the section entirely.
fn format_workstreams_section(workstreams: &[crate::workstreams::Workstream]) -> String {
    let mut s = String::new();
    s.push_str("# Workstreams\n\n");
    for (i, w) in workstreams.iter().enumerate() {
        let label = format!("W{}", i + 1);
        let summary = workstream_one_line_summary(&w.summary);
        let mut counts: Vec<String> = Vec::new();
        if w.open_action_count > 0 {
            counts.push(format!("{} open", w.open_action_count));
        }
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
    use crate::workstreams::{
        NoteRef, Workstream, WorkstreamAction, WorkstreamDetail,
    };
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
            email_count: 0,
            event_count: 0,
            note_count: 0,
            open_action_count: 0,
        }
    }

    #[test]
    fn format_workstreams_section_renders_labels_and_counts() {
        let mut a = make_ws("ws_a", "Hyundai POC", "Final invoice details.", 100);
        a.open_action_count = 2;
        a.email_count = 5;
        a.event_count = 1;

        let b = make_ws("ws_b", "Q3 hiring", "Sourcing two seniors.", 50);

        let out = format_workstreams_section(&[a, b]);
        assert!(out.starts_with("# Workstreams\n\n"));
        assert!(out.contains("[W1] Hyundai POC"));
        assert!(out.contains("[W2] Q3 hiring"));
        assert!(out.contains("Final invoice details."));
        assert!(
            out.contains("(2 open · 5 emails · 1 meetings)"),
            "expected counts pill in section, got: {out}"
        );
        // No counts when all zero — just the title-summary line.
        assert!(out.contains("[W2] Q3 hiring — Sourcing two seniors.\n"));
    }

    #[test]
    fn format_workstreams_section_truncates_long_summary() {
        let long = "X".repeat(WORKSTREAM_SUMMARY_CAP + 50);
        let w = make_ws("ws", "Title", &long, 0);
        let out = format_workstreams_section(&[w]);
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

    fn make_action(text: &str, done: bool) -> WorkstreamAction {
        WorkstreamAction {
            id: format!("wsa_{text}"),
            workstream_id: "ws_x".into(),
            text: text.into(),
            due_ms: None,
            source_kind: "email".into(),
            source_id: "msg".into(),
            done,
            created_ms: 0,
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
                email_count: 2,
                event_count: 0,
                note_count: 1,
                open_action_count: 1,
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
            actions: vec![make_action("Reply to invoice", false), make_action("Send quote", true)],
        };
        let out = format_workstream_detail("W1", &detail);
        assert!(out.starts_with("# [W1] Hyundai POC\n"));
        assert!(out.contains("Invoicing in flight."));
        assert!(out.contains("Actions (1 open, 1 done)"));
        assert!(out.contains("- [ ] Reply to invoice (from email)"));
        assert!(out.contains("- [x] Send quote (from email)"));
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
                email_count: emails.len() as u32,
                event_count: 0,
                note_count: 0,
                open_action_count: 0,
            },
            emails,
            events: vec![],
            notes: vec![],
            actions: vec![],
        };
        let out = format_workstream_detail("W2", &detail);
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
    fn format_workstream_detail_handles_empty_summary_and_actions() {
        let detail = WorkstreamDetail {
            workstream: Workstream {
                id: "ws_z".into(),
                title: "Bare".into(),
                summary: String::new(),
                status: "active".into(),
                last_activity_ms: 0,
                created_ms: 0,
                updated_ms: 0,
                email_count: 0,
                event_count: 0,
                note_count: 0,
                open_action_count: 0,
            },
            emails: vec![],
            events: vec![],
            notes: vec![],
            actions: vec![],
        };
        let out = format_workstream_detail("W3", &detail);
        assert_eq!(out, "# [W3] Bare\n");
    }
}
