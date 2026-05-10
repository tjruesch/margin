//! The cluster pass: snapshot recent signals, ask Claude to cluster
//! them into named workstreams, persist the result.
//!
//! Single-shot (non-streaming) Claude call. JSON-only output. Strict
//! parsing — unknown label refs are dropped, malformed responses bail.
//!
//! Failure semantics: `last_clustered_ms` is **only** updated on
//! success. Anthropic 5xx, network blips, malformed JSON all fall
//! through to the next 6h tick or manual refresh; existing rows are
//! left untouched.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use super::persist::{self, SynthesizedAction, SynthesizedWorkstream};
use super::{ClusterReport, NoteRef, Workstream};
use crate::anthropic::{ANTHROPIC_VERSION, DEFAULT_MODEL, ENDPOINT};
use crate::connectors::{calendar, email, microsoft_graph, oauth};
use crate::index;
use crate::keychain;
use crate::team;

/// 6 hours. Boot tick + manual Refresh are no-ops if a successful pass
/// landed within this window.
const CLUSTER_TTL_MS: i64 = 6 * 3600 * 1000;

const EMAIL_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const EVENT_WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const EVENT_WINDOW_FORWARD_MS: i64 = 14 * 24 * 3600 * 1000;
const NOTE_WINDOW_BACK_MS: i64 = 30 * 24 * 3600 * 1000;

/// Cap on lazy body fetches per pass. Each is a Graph round-trip, so
/// 100 keeps the pre-call work bounded for users with very busy inboxes.
const MAX_BODY_FETCHES: usize = 100;

const MAX_TOKENS: u32 = 8192;

/// Cap on user_notes length when included in the synthesizer prompt
/// (#77). DB has no cap; this only protects the token budget.
const USER_NOTES_PROMPT_CAP: usize = 4000;

/// Cap on archived workstreams listed in the prompt (#78). Sorted by
/// archived_at_ms desc (most recent first) so older threads fall off.
/// Same default as the active cap.
const ARCHIVED_WORKSTREAM_CAP: usize = 30;

const SYSTEM_PROMPT: &str = "You are a workstream synthesizer. Given a user's recent emails, calendar events, \
and notes, group them into 3-15 active workstreams: ongoing efforts the user is participating in \
(projects, hiring loops, vendor evaluations, support escalations, etc.).

Stickiness: the \"Existing workstreams (active)\" section lists workstream ids already in the \
database. When new items naturally extend one of those, REUSE its id verbatim. Spawn a new \
workstream only when no existing one is a clean fit.

Some existing workstreams may carry an indented \"Notes:\" line — these are user-authored ground truth. \
Treat them as authoritative: prefer them when reconciling new evidence, never write a summary that \
contradicts them. If the notes describe scope, ownership, deadlines, or identity that conflicts \
with what the recent items suggest, the notes win.

Some workstreams are listed in a separate \"Archived workstreams\" section. Those are off-limits \
for clustering: do NOT roll new items into them, do NOT cite them. ONLY reuse an archived id when \
the new evidence unambiguously continues that thread (same project, same people, clear continuation \
of the work). When you do resurrect an archived workstream, set \"status\": \"active\" in its \
response object so the system flips its state. Casual subject overlap or shared participants are \
NOT enough — only resurrect when the items are clearly the next chapter of the same thread.

Titles: short, specific, proper-noun-leaning (\"Hyundai POC review\", \"Q3 sourcing\", \
\"Bridge integration\") — not generic (\"Project work\", \"Various meetings\").

Summaries: 1-3 sentences. State what's happening and what's next, not what it is in the abstract.

Action items: extract concrete TODOs the user owes (or owns) per workstream. Each must reference \
a source by its label. Skip items that are already done.

Output: a strict JSON array. No prose. No markdown fences. No keys other than the schema below.

Schema:
[
  {
    \"id\": \"<existing workstream id or null>\",
    \"status\": \"active\" | null,
    \"title\": \"...\",
    \"summary\": \"...\",
    \"members\": { \"emails\": [\"M1\", \"M2\"], \"events\": [\"E3\"], \"notes\": [\"N1\"] },
    \"actions\": [
      { \"text\": \"...\", \"due_ms\": null, \"source_kind\": \"email\", \"source_label\": \"M2\" }
    ]
  }
]";

// ----- Public entry point --------------------------------------------------

pub async fn maybe_cluster(app: &AppHandle, force: bool) -> Result<ClusterReport, String> {
    let lock = super::cluster_lock();
    let _guard = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("[workstreams] another cluster pass is in flight; skipping");
            return Ok(ClusterReport {
                state: "skipped".into(),
                ..Default::default()
            });
        }
    };

    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let now_ms = current_unix_ms();

    let last = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        persist::last_clustered_ms(&c).map_err(|e| e.to_string())?
    };

    if !force && now_ms.saturating_sub(last) < CLUSTER_TTL_MS {
        eprintln!("[workstreams] cluster pass skipped (last {last}, now {now_ms})");
        emit_status(app, "skipped", None);
        return Ok(ClusterReport {
            state: "skipped".into(),
            last_clustered_ms: last,
            ..Default::default()
        });
    }

    emit_status(app, "clustering", None);

    let report = match run_cluster_pass(app, &conn_state, now_ms).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[workstreams] cluster pass failed: {e}");
            emit_status(app, "errored", Some(e.clone()));
            return Err(e);
        }
    };

    {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        persist::set_last_clustered_ms(&c, now_ms).map_err(|e| e.to_string())?;
    }

    emit_status(app, "synced", Some(format_report_summary(&report)));
    Ok(report)
}

async fn run_cluster_pass(
    app: &AppHandle,
    conn_state: &std::sync::Mutex<rusqlite::Connection>,
    now_ms: i64,
) -> Result<ClusterReport, String> {
    // ---- 0. Orphan-signals cleanup (#85) --------------------------------
    // The signals pivot uses soft FKs to email_messages / calendar_events.
    // When upstream items get deleted (calendar window-roll, message
    // expiry, etc.) we keep the pivot tidy here once per cluster pass.
    // Non-fatal — orphans only degrade hydrate behavior, not safety.
    {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        if let Err(e) = persist::cleanup_orphan_signals(&c) {
            eprintln!("[workstreams] orphan signals cleanup failed: {e}");
        }
    }

    // ---- 1. Snapshot inputs in a single lock window ---------------------
    let (existing_active, existing_archived, mut emails, events, notes, team) = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        // Pull both active and archived (snoozed excluded — they're
        // hidden from synthesis) and partition into separate sections
        // for the prompt. Active workstreams are clusterable; archived
        // ones are off-limits unless Claude explicitly resurrects.
        let mut active: Vec<crate::workstreams::Workstream> = Vec::new();
        let mut archived: Vec<crate::workstreams::Workstream> = Vec::new();
        for (w, is_archived) in persist::list_workstreams_for_synthesis(&c)
            .map_err(|e| e.to_string())?
        {
            if is_archived {
                archived.push(w);
            } else {
                active.push(w);
            }
        }
        archived.truncate(ARCHIVED_WORKSTREAM_CAP);
        let emails = email::list_messages_in_range(
            &c,
            now_ms - EMAIL_WINDOW_BACK_MS,
            now_ms + 24 * 3600 * 1000,
            None,
            500,
        )
        .map_err(|e| e.to_string())?;
        let events = calendar::list_events_in_range(
            &c,
            now_ms - EVENT_WINDOW_BACK_MS,
            now_ms + EVENT_WINDOW_FORWARD_MS,
            None,
        )
        .map_err(|e| e.to_string())?;
        let directory = index::list_directory(&c, 200).map_err(|e| e.to_string())?;
        let notes_cutoff = now_ms - NOTE_WINDOW_BACK_MS;
        let notes: Vec<NoteRef> = directory
            .into_iter()
            .filter(|d| d.modified_ms >= notes_cutoff)
            .map(|d| NoteRef {
                note_path: d.note_path,
                title: d.title,
                modified_ms: d.modified_ms,
            })
            .collect();
        let team = team::list_team_members_raw(&c).unwrap_or_default();
        (active, archived, emails, events, notes, team)
    };
    let team_by_id: std::collections::HashMap<String, String> = team
        .iter()
        .map(|m| (m.id.clone(), m.display_name.clone()))
        .collect();

    if emails.is_empty() && events.is_empty() && notes.is_empty() {
        eprintln!("[workstreams] no recent items to cluster; skipping");
        return Ok(ClusterReport {
            state: "skipped".into(),
            last_clustered_ms: now_ms,
            ..Default::default()
        });
    }

    // ---- 2. Lazy-fetch missing email bodies -----------------------------
    // Bound the work to MAX_BODY_FETCHES per pass; older messages stay
    // preview-only. Errors per message are logged and skipped — a body
    // fetch failure must not abort the cluster pass.
    let mut fetched = 0usize;
    for m in emails.iter_mut() {
        if fetched >= MAX_BODY_FETCHES {
            break;
        }
        if m.body_html.is_some() {
            continue;
        }
        let kind = m
            .connector_id
            .split_once(':')
            .map(|(k, _)| k.to_string())
            .unwrap_or_else(|| "microsoft_graph".to_string());
        let connector_id = m.connector_id.clone();
        let external_id = m.external_id.clone();
        let body_result = oauth::with_valid_token(app, &connector_id, &kind, |access| async move {
            microsoft_graph::fetch_message_body(&access, &external_id).await
        })
        .await;
        match body_result {
            Ok(Some(body)) => {
                {
                    let c = match conn_state.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("[workstreams] conn lock for body persist: {e}");
                            continue;
                        }
                    };
                    if let Err(e) = email::set_message_body_html(&c, &m.id, &body) {
                        eprintln!("[workstreams] persist body for {}: {e}", m.id);
                    }
                }
                m.body_html = Some(body);
                fetched += 1;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[workstreams] body fetch failed for {}: {e}", m.id);
            }
        }
    }

    // ---- 3. Build prompt, label maps ------------------------------------
    let (user_message, label_maps) = build_user_message(
        &existing_active,
        &existing_archived,
        &emails,
        &events,
        &notes,
        &team_by_id,
    );
    let items_clustered = (emails.len() + events.len() + notes.len()) as u32;

    // ---- 4. Single-shot Claude call -------------------------------------
    let api_key = keychain::read_anthropic_api_key().map_err(|_| {
        "Anthropic API key not configured — open Settings → AI to add one".to_string()
    })?;
    let response_text = call_anthropic(&api_key, DEFAULT_MODEL, &user_message).await?;

    // ---- 5. Parse JSON --------------------------------------------------
    let synthesized = parse_synthesizer_response(&response_text, &label_maps);

    // ---- 6. Persist in a single transaction -----------------------------
    let mut report = ClusterReport {
        state: "synced".into(),
        model: DEFAULT_MODEL.to_string(),
        items_clustered,
        last_clustered_ms: now_ms,
        ..Default::default()
    };

    {
        let mut c = conn_state.lock().map_err(|e| e.to_string())?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        for ws in &synthesized {
            // For records that reference an existing workstream id we
            // need to know its current status to decide whether to
            // resurrect, refresh-as-active, or skip entirely (#78).
            let pre_status: Option<String> = match ws.id.as_deref() {
                Some(id) if !id.is_empty() => persist::lookup_pre_status(&tx, id)
                    .map_err(|e| format!("lookup pre-status: {e}"))?,
                _ => None,
            };

            if pre_status.as_deref() == Some("archived") {
                if ws.status.as_deref() == Some("active") {
                    let id = ws.id.as_deref().unwrap_or_default();
                    if persist::resurrect_if_archived(&tx, id, now_ms)
                        .map_err(|e| format!("resurrect: {e}"))?
                    {
                        report.workstreams_reopened += 1;
                    }
                    // Fall through to write_workstream — it will refresh
                    // title/summary/pivots/actions on the now-active row.
                } else {
                    eprintln!(
                        "[workstreams] Claude referenced archived id {} without status='active'; skipping",
                        ws.id.as_deref().unwrap_or("?")
                    );
                    continue;
                }
            }

            let counts = persist::write_workstream(&tx, ws, now_ms)
                .map_err(|e| format!("write workstream: {e}"))?;
            if counts.workstream_added {
                report.workstreams_added += 1;
            } else {
                report.workstreams_updated += 1;
            }
            report.actions_added += counts.actions_added;
            report.actions_updated += counts.actions_updated;
        }
        tx.commit().map_err(|e| e.to_string())?;
    }

    Ok(report)
}

// ----- Anthropic call ------------------------------------------------------

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

async fn call_anthropic(api_key: &str, model: &str, user_message: &str) -> Result<String, String> {
    let body = ApiRequest {
        model,
        max_tokens: MAX_TOKENS,
        stream: false,
        system: SYSTEM_PROMPT,
        messages: vec![ApiMessage {
            role: "user",
            content: user_message,
        }],
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("client init: {e}"))?;
    let resp = client
        .post(ENDPOINT)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let raw = resp.text().await.unwrap_or_default();
        return Err(match status.as_u16() {
            401 => format!("Invalid Anthropic API key — check Settings → AI ({raw})"),
            429 => "Rate limited by Anthropic — try again shortly".to_string(),
            _ => format!("Anthropic returned {status}: {raw}"),
        });
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("anthropic response parse: {e}"))?;
    // Concatenate all `text` content blocks (typically just one in
    // non-streaming). Tool use blocks are not requested for synthesis,
    // so we ignore unrelated block types.
    let text = json
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    if text.is_empty() {
        return Err("anthropic returned no text content".into());
    }
    Ok(text)
}

// ----- Prompt building -----------------------------------------------------

/// Maps from Claude-facing labels (M1, E1, N1) back to canonical IDs.
struct LabelMaps {
    emails: HashMap<String, String>,    // "M1" → "<connector>:<external>"
    events: HashMap<String, String>,    // "E1" → calendar_events.id
    notes: HashMap<String, String>,     // "N1" → note_path
}

fn build_user_message(
    existing_active: &[Workstream],
    existing_archived: &[Workstream],
    emails: &[email::EmailMessage],
    events: &[calendar::CalendarEvent],
    notes: &[NoteRef],
    team_by_id: &std::collections::HashMap<String, String>,
) -> (String, LabelMaps) {
    let mut s = String::new();
    s.push_str("# Existing workstreams (active)\n\n");
    if existing_active.is_empty() {
        s.push_str("(none)\n");
    } else {
        for w in existing_active {
            s.push_str(&format!(
                "[{}] {} — {} (active)\n",
                w.id,
                w.title,
                summarize_one_line(&w.summary)
            ));
            if let Some(owner_id) = w.owner_member_id.as_deref() {
                if let Some(name) = team_by_id.get(owner_id) {
                    s.push_str(&format!("   Owner: {name}\n"));
                }
            }
            if !w.members.is_empty() {
                let names: Vec<&str> = w
                    .members
                    .iter()
                    .filter_map(|id| team_by_id.get(id).map(String::as_str))
                    .take(8)
                    .collect();
                if !names.is_empty() {
                    let suffix = if w.members.len() > names.len() {
                        format!(" (+{} more)", w.members.len() - names.len())
                    } else {
                        String::new()
                    };
                    s.push_str(&format!(
                        "   Members: {names}{suffix}\n",
                        names = names.join(", "),
                        suffix = suffix
                    ));
                }
            }
            if let Some(notes) = w.user_notes.as_deref().filter(|s| !s.trim().is_empty()) {
                let collapsed = collapse_ws(notes);
                let truncated = truncate_chars(&collapsed, USER_NOTES_PROMPT_CAP);
                s.push_str(&format!(
                    "   Notes (user-authored, ground truth): {truncated}\n"
                ));
            }
        }
    }

    if !existing_archived.is_empty() {
        s.push_str(
            "\n# Archived workstreams (do not resurrect on casual overlap)\n\n",
        );
        for w in existing_archived {
            let archived_label = w
                .archived_at_ms
                .map(format_iso_date)
                .unwrap_or_else(|| "unknown".to_string());
            s.push_str(&format!(
                "[{}] {} — {} (archived {})\n",
                w.id,
                w.title,
                summarize_one_line(&w.summary),
                archived_label,
            ));
            // Deliberately do NOT render user_notes for archived
            // workstreams — they're inactive context. If Claude
            // resurrects, the next pass will see the active version
            // (with notes) again.
        }
    }

    let mut email_map = HashMap::new();
    s.push_str("\n# Recent emails (last 14 days)\n\n");
    if emails.is_empty() {
        s.push_str("(none)\n");
    } else {
        for (i, m) in emails.iter().enumerate() {
            let label = format!("M{}", i + 1);
            email_map.insert(label.clone(), m.id.clone());
            let date = format_iso_date(m.sent_at_ms);
            let from = format_from(&m.from_email, m.from_name.as_deref());
            let body = email_body_excerpt(m);
            s.push_str(&format!(
                "[{label}] {date} — From: {from} — Subject: {subject}\n{body}\n\n",
                subject = m.subject,
            ));
        }
    }

    let mut event_map = HashMap::new();
    s.push_str("\n# Recent calendar events (window: -14d .. +14d)\n\n");
    if events.is_empty() {
        s.push_str("(none)\n");
    } else {
        for (i, e) in events.iter().enumerate() {
            let label = format!("E{}", i + 1);
            event_map.insert(label.clone(), e.id.clone());
            let when = format_iso_datetime(e.start_ms);
            let attendees: Vec<&str> =
                e.attendees.iter().take(8).map(|a| a.email.as_str()).collect();
            let attendees_str = if attendees.is_empty() {
                String::new()
            } else {
                format!(" — Attendees: {}", attendees.join(", "))
            };
            s.push_str(&format!(
                "[{label}] {when} — {title}{attendees_str}\n",
                title = e.title
            ));
        }
    }

    let mut note_map = HashMap::new();
    s.push_str("\n# Recent notes (last 30 days)\n\n");
    if notes.is_empty() {
        s.push_str("(none)\n");
    } else {
        for (i, n) in notes.iter().enumerate() {
            let label = format!("N{}", i + 1);
            note_map.insert(label.clone(), n.note_path.clone());
            let date = format_iso_date(n.modified_ms);
            s.push_str(&format!(
                "[{label}] {date} — {title}\n",
                title = n.title
            ));
        }
    }

    s.push_str(
        "\n# Instructions\n\nReturn a JSON array matching the schema in the system prompt. \
         Reuse an existing workstream id when the new items extend it; spawn a new one (id: null) \
         only when no existing fit. Each action's source_label MUST be one of the labels above \
         (M*/E*/N*). Output JSON only — no prose, no fences.\n",
    );

    (
        s,
        LabelMaps {
            emails: email_map,
            events: event_map,
            notes: note_map,
        },
    )
}

fn email_body_excerpt(m: &email::EmailMessage) -> String {
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

fn summarize_one_line(s: &str) -> String {
    let collapsed = collapse_ws(s);
    if collapsed.chars().count() <= 200 {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(200).collect();
        format!("{}…", truncated)
    }
}

/// Char-aware truncation with a `…` suffix when over the cap. Preserves
/// UTF-8 boundaries.
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let truncated: String = s.chars().take(cap).collect();
    format!("{truncated}…")
}

fn format_from(email: &str, name: Option<&str>) -> String {
    match name {
        Some(n) if !n.is_empty() => format!("{n} <{email}>"),
        _ => email.to_string(),
    }
}

fn format_iso_date(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_iso_datetime(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ----- Response parsing ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawWorkstream {
    #[serde(default)]
    id: Option<String>,
    /// Optional status hint (#78). Claude sets `"active"` to resurrect
    /// a previously-archived workstream. Other values ignored.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    members: Option<RawMembers>,
    #[serde(default)]
    actions: Option<Vec<RawAction>>,
}

#[derive(Debug, Deserialize)]
struct RawMembers {
    #[serde(default)]
    emails: Vec<String>,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawAction {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    due_ms: Option<i64>,
    #[serde(default)]
    source_kind: Option<String>,
    #[serde(default)]
    source_label: Option<String>,
}

fn parse_synthesizer_response(
    raw: &str,
    label_maps: &LabelMaps,
) -> Vec<SynthesizedWorkstream> {
    let stripped = strip_json_fences(raw);
    let parsed: Vec<RawWorkstream> = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(e) => {
            let preview: String = stripped.chars().take(500).collect();
            eprintln!("[workstreams] response parse failed: {e}; preview: {preview}");
            return Vec::new();
        }
    };

    let mut out = Vec::with_capacity(parsed.len());
    let mut existing_ids: HashSet<String> = HashSet::new();
    for raw in parsed {
        let title = match raw.title.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                eprintln!("[workstreams] dropping workstream with empty title");
                continue;
            }
        };
        let summary = raw
            .summary
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let id = raw
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "null")
            .map(|s| s.to_string());
        if let Some(ref existing) = id {
            if !existing_ids.insert(existing.clone()) {
                eprintln!("[workstreams] duplicate workstream id {existing} in response");
            }
        }
        let status = raw
            .status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "null")
            .map(|s| s.to_string());

        let members = raw.members.unwrap_or(RawMembers {
            emails: Vec::new(),
            events: Vec::new(),
            notes: Vec::new(),
        });

        let member_emails =
            map_labels(&members.emails, &label_maps.emails, "email");
        let member_events =
            map_labels(&members.events, &label_maps.events, "event");
        let member_notes = map_labels(&members.notes, &label_maps.notes, "note");

        let actions = raw
            .actions
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| {
                let text = match a.text.as_deref().map(str::trim) {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => return None,
                };
                let kind = a.source_kind.unwrap_or_default();
                let label = a.source_label.unwrap_or_default();
                let source_id = match kind.as_str() {
                    "email" => label_maps.emails.get(&label).cloned(),
                    "event" => label_maps.events.get(&label).cloned(),
                    "note" => label_maps.notes.get(&label).cloned(),
                    _ => None,
                };
                let source_id = match source_id {
                    Some(s) => s,
                    None => {
                        eprintln!(
                            "[workstreams] dropping action with unknown label {label} kind {kind}: {text}"
                        );
                        return None;
                    }
                };
                Some(SynthesizedAction {
                    text,
                    due_ms: a.due_ms,
                    source_kind: kind,
                    source_id,
                })
            })
            .collect();

        out.push(SynthesizedWorkstream {
            id,
            title,
            summary,
            member_emails,
            member_events,
            member_notes,
            actions,
            status,
        });
    }
    out
}

fn map_labels(
    labels: &[String],
    map: &HashMap<String, String>,
    kind: &str,
) -> Vec<String> {
    let mut out = Vec::with_capacity(labels.len());
    let mut seen = HashSet::new();
    for label in labels {
        match map.get(label.trim()) {
            Some(id) => {
                if seen.insert(id.clone()) {
                    out.push(id.clone());
                }
            }
            None => {
                eprintln!("[workstreams] dropping unknown {kind} label {label}");
            }
        }
    }
    out
}

fn strip_json_fences(s: &str) -> String {
    let trimmed = s.trim();
    let without_fences = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_start();
    let without_close = without_fences.strip_suffix("```").unwrap_or(without_fences);
    without_close.trim().to_string()
}

// ----- Status events -------------------------------------------------------

#[derive(Serialize, Clone)]
struct StatusEvent<'a> {
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn emit_status(app: &AppHandle, state: &str, message: Option<String>) {
    let _ = app.emit("workstream-status", StatusEvent { state, message });
}

fn format_report_summary(r: &ClusterReport) -> String {
    format!(
        "+{}/~{} workstreams, +{}/~{} actions, {} items clustered",
        r.workstreams_added,
        r.workstreams_updated,
        r.actions_added,
        r.actions_updated,
        r.items_clustered
    )
}

fn current_unix_ms() -> i64 {
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

    fn label_maps() -> LabelMaps {
        let mut emails = HashMap::new();
        emails.insert("M1".into(), "mg:test::msg-1".to_string());
        emails.insert("M2".into(), "mg:test::msg-2".to_string());
        let mut events = HashMap::new();
        events.insert("E1".into(), "mg:test::ev-1".to_string());
        let mut notes = HashMap::new();
        notes.insert("N1".into(), "/notes/a.md".to_string());
        LabelMaps { emails, events, notes }
    }

    #[test]
    fn parse_synthesizer_response_handles_raw_json() {
        let raw = r#"[
            {
                "id": null,
                "title": "Hyundai POC",
                "summary": "Final invoice + dismissals.",
                "members": { "emails": ["M1","M2"], "events": ["E1"], "notes": ["N1"] },
                "actions": [
                    { "text": "Reply to invoice", "due_ms": null, "source_kind": "email", "source_label": "M1" }
                ]
            }
        ]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        let ws = &parsed[0];
        assert_eq!(ws.title, "Hyundai POC");
        assert_eq!(ws.member_emails, vec!["mg:test::msg-1", "mg:test::msg-2"]);
        assert_eq!(ws.member_events, vec!["mg:test::ev-1"]);
        assert_eq!(ws.member_notes, vec!["/notes/a.md"]);
        assert_eq!(ws.actions.len(), 1);
        assert_eq!(ws.actions[0].source_id, "mg:test::msg-1");
    }

    #[test]
    fn parse_synthesizer_response_handles_optional_json_fences() {
        let raw = r#"```json
        [{"title": "X", "summary": "y"}]
        ```"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "X");

        let raw2 = "```\n[{\"title\":\"Y\",\"summary\":\"z\"}]\n```";
        let parsed2 = parse_synthesizer_response(raw2, &label_maps());
        assert_eq!(parsed2.len(), 1);
        assert_eq!(parsed2[0].title, "Y");
    }

    #[test]
    fn parse_synthesizer_response_drops_unknown_label_refs() {
        let raw = r#"[
            {
                "title": "Mixed",
                "summary": "",
                "members": { "emails": ["M1","M99"], "events": [], "notes": ["NX"] },
                "actions": [
                    { "text": "ok", "source_kind": "email", "source_label": "M1" },
                    { "text": "drop", "source_kind": "email", "source_label": "M99" }
                ]
            }
        ]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        // M1 keeps, M99 dropped.
        assert_eq!(parsed[0].member_emails, vec!["mg:test::msg-1"]);
        assert!(parsed[0].member_notes.is_empty(), "NX not in label map");
        // Action whose source_label was unknown is dropped.
        assert_eq!(parsed[0].actions.len(), 1);
        assert_eq!(parsed[0].actions[0].text, "ok");
    }

    #[test]
    fn parse_synthesizer_response_drops_empty_titles() {
        let raw = r#"[
            { "title": "  ", "summary": "" },
            { "title": "Real", "summary": "" }
        ]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Real");
    }

    #[test]
    fn parse_synthesizer_response_returns_empty_on_malformed_json() {
        let raw = "not json at all {{{ ";
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert!(parsed.is_empty());
    }

    #[test]
    fn strip_html_removes_tags_and_keeps_text() {
        assert_eq!(strip_html("<p>hello <b>world</b></p>").trim(), "hello  world");
    }

    #[test]
    fn collapse_ws_squashes_runs() {
        assert_eq!(collapse_ws("  a   b\n\nc  "), "a b c");
    }

    #[test]
    fn parse_synthesizer_response_captures_status_field() {
        let raw = r#"[
            {"id": "ws_arc", "status": "active", "title": "Resurrected", "summary": ""},
            {"id": null, "title": "New", "summary": ""}
        ]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].status.as_deref(), Some("active"));
        assert_eq!(parsed[1].status, None);
    }

    #[test]
    fn parse_synthesizer_response_treats_missing_status_as_none() {
        let raw = r#"[{"id": "ws_x", "title": "T", "summary": ""}]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].status, None);
    }
}
