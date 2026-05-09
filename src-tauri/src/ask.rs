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

use crate::{index::DirectoryEntry, keychain};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const ANTHROPIC_VERSION: &str = "2023-06-01";
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
    /// to render an inline pill ("Reading [3] 'All-hands April'…").
    ToolUseStart {
        turn_id: String,
        tool_id: String,
        name: String,
        target_n: u32,
        target_title: String,
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

/// One source the model can cite as `[N]`. Indexes the full notes
/// directory (not just the retrieved set) so the model's allowed
/// citation surface matches what's actually in its prompt. The UI
/// renders chips only for `[N]`s the model actually emits in its
/// answer — passing the whole directory keeps the mapping consistent
/// without flooding the chip strip.
#[derive(Serialize, Clone)]
pub struct AskSource {
    pub index: u32,
    pub note_path: String,
    pub bundle_id: String,
    pub title: String,
    pub modified_ms: i64,
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
notes, hand-typed notes, transcripts) and their team profiles.

The user's message contains three sections:

1. **Notes directory** — every non-archived note, labeled `[1]`, `[2]`, etc., with title, date, and \
a short preview. This is the master index; you may cite *any* `[N]` from this directory.

2. **Top candidates** — a subset of the directory whose full bodies have been loaded for deep \
context. The same `[N]` labels apply — these are the same notes, just expanded. When citing details \
that came from a body, cite the directory `[N]`.

3. **Team profiles** — short bios for each colleague: display name, aliases, role, profile text. \
Use these to interpret references to people in the notes (e.g. \"Heike\" maps to a known team \
member). You may cite directly attributable claims from a profile by the person's name in prose; \
profiles aren't `[N]`-citable — only notes are.

You also have two tools for digging deeper into the directory:
- **`read_note(n)`** — returns the full markdown body of directory entry `[n]`. Use when a preview \
hints at relevance but you need the body to answer.
- **`read_transcript(n)`** — returns the meeting transcript text for `[n]`, if it has audio. Use \
when the question is likely about something said in a meeting but not captured in the typed body.

Use tools sparingly — most questions can be answered from the directory + top candidates already in \
context. Don't speculate; call a tool if you genuinely need the content. Up to 6 tool calls per \
question; after that you must answer with what you have.

Rules:
- Answer in natural prose. Be specific and concise — 1-4 short paragraphs unless the question asks \
for a list.
- Cite sources inline with `[N]` immediately after each claim that came from a note. Multiple \
citations: `[1][3]`. Never make up citation numbers — only use directory labels you actually \
received.
- For \"when did we first…\" questions, identify the *earliest* dated note that matches and cite it.
- If neither the notes nor the profiles contain the answer, say so clearly. Don't speculate.
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

    // Pull the all-notes directory + retrieval set + team roster in
    // one lock. Profile.md content is read off-lock below.
    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let (directory, retrieved_paths, team) = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        let directory = crate::index::list_directory(&c, DIRECTORY_CAP)
            .map_err(|e| e.to_string())?;
        let hits = crate::index::retrieve_for_ask(&c, &query, RETRIEVAL_K)
            .map_err(|e| e.to_string())?;
        let retrieved_paths: std::collections::HashSet<String> =
            hits.iter().map(|h| h.note_path.clone()).collect();
        let team = crate::team::list_team_members_raw(&c).unwrap_or_default();
        (directory, retrieved_paths, team)
    };

    // Build the citation surface: every directory entry gets a 1-based
    // [N] label.
    let mut sources: Vec<AskSource> = Vec::with_capacity(directory.len());
    for (i, e) in directory.iter().enumerate() {
        sources.push(AskSource {
            index: (i + 1) as u32,
            note_path: e.note_path.clone(),
            bundle_id: e.bundle_id.clone(),
            title: e.title.clone(),
            modified_ms: e.modified_ms,
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
            let target_title = directory
                .get(target_n.saturating_sub(1) as usize)
                .map(|e| e.title.clone())
                .unwrap_or_default();

            let _ = app.emit(
                "ai-stream",
                StreamEvent::ToolUseStart {
                    turn_id: turn_id.to_string(),
                    tool_id: tc.id.clone(),
                    name: tc.name.clone(),
                    target_n,
                    target_title: target_title.clone(),
                },
            );

            let result = dispatch_tool(&tc.name, &tc.input, directory);

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
    name: &str,
    input: &serde_json::Value,
    directory: &[DirectoryEntry],
) -> ToolResult {
    let n = match input.get("n").and_then(|v| v.as_u64()) {
        Some(v) if v >= 1 => v as usize,
        _ => {
            return ToolResult {
                content: "Tool input missing or invalid required field 'n' (must be a 1-based directory index).".to_string(),
                is_error: true,
            };
        }
    };
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
            content: format!("Unknown tool: {name}. Available tools: read_note, read_transcript."),
            is_error: true,
        },
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

    s.push_str("# Question\n\n");
    s.push_str(query.trim());
    s
}

fn preview_one_line(preview: &str) -> String {
    let collapsed: String = preview
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    truncate_chars(collapsed.trim(), PER_PREVIEW_CAP)
}

fn format_date(modified_ms: i64) -> String {
    use chrono::{DateTime, Utc};
    let secs = modified_ms / 1000;
    let nsec = ((modified_ms % 1000) * 1_000_000) as u32;
    DateTime::<Utc>::from_timestamp(secs, nsec)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown date".to_string())
}
