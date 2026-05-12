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
use super::signals::{self, SignalRegistry, SnapshotItem, SnapshotPayload};
use super::{ClusterReport, Workstream};
use crate::anthropic::{ANTHROPIC_VERSION, DEFAULT_MODEL, ENDPOINT};
use crate::connectors::{email, microsoft_graph, oauth};
use crate::keychain;
use crate::team;

/// 6 hours. Boot tick + manual Refresh are no-ops if a successful pass
/// landed within this window.
const CLUSTER_TTL_MS: i64 = 6 * 3600 * 1000;

/// Cap on lazy body fetches per pass. Each is a Graph round-trip, so
/// 100 keeps the pre-call work bounded for users with very busy inboxes.
/// Cross-cutting (not source-owned) — applies to email body backfill
/// triggered before the snapshot is rendered.
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
database. STRONGLY prefer attaching new items to an existing workstream when there is any \
reasonable fit — REUSE its id verbatim. Spawn a new workstream only when no existing one fits at \
all (genuinely different project, people, scope). Do NOT create near-duplicate workstreams that \
differ only in phrasing from an existing one. When an existing workstream has no signals attached \
yet (only title + summary, no \"Notes:\" line populated by recent activity), it is a user-created \
umbrella waiting to collect evidence: pull matching emails/events/notes into it instead of \
spawning a new sibling.

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
a source by its label. Skip items that are already done. Do NOT emit an item whose only content is \
attending, joining, or being present at a meeting/call/event (e.g. \"Attend the Talgo demo\", \
\"Join the kickoff call\", \"Be at standup on Friday\"). Mere participation is not an action item — \
emit one only when there is a concrete deliverable, decision, or follow-up the user must produce. \
Set \"owner_label\" to a team label (e.g. \"T1\") from the \"Team\" section when the source \
clearly assigns the work to that person; omit or leave null when the owner is ambiguous or is the \
user themselves.

Dedup against existing open actions: each existing workstream may list its open actions under \
\"Open actions (already tracked)\". Treat that list as the source of truth — do NOT emit a new \
action when an existing one already covers the same concrete TODO, even if your phrasing would \
differ (e.g. existing \"Follow up with Kern & Sohn on Bridge onboarding\" already covers a new \
\"Follow up with katharina@kern-sohn.com on Bridge intro next steps\"). When in doubt, omit; the \
user can always add or edit actions manually. Several recent items about the same effort should \
contribute AT MOST one new action, not one per source.

Hierarchy: the \"Existing workstreams (active)\" section may show parents at the top level with \
children indented underneath (rendered with a \"↳\" marker). When a NEW workstream cleanly extends \
an umbrella effort already in the list (e.g. \"ELAN AI Bridge\" with sub-threads like \"Talgo demo\" \
or \"CompTIA setup\"), set \"parent_id\" to the umbrella's id. Otherwise leave \"parent_id\" null. \
The hierarchy is flat — max 2 levels — so do NOT set \"parent_id\" to a workstream that's already \
indented under another. \"parent_id\" only takes effect on newly-spawned workstreams; it is ignored \
for existing ids (the user owns parent assignment for established workstreams).

Output: a strict JSON array. No prose. No markdown fences. No keys other than the schema below.

Schema:
[
  {
    \"id\": \"<existing workstream id or null>\",
    \"status\": \"active\" | null,
    \"parent_id\": \"<existing parent workstream id or null>\",
    \"title\": \"...\",
    \"summary\": \"...\",
    \"members\": { \"emails\": [\"M1\", \"M2\"], \"events\": [\"E3\"], \"notes\": [\"N1\"] },
    \"actions\": [
      { \"text\": \"...\", \"due_ms\": null, \"source_kind\": \"email\", \"source_label\": \"M2\", \"owner_label\": \"T1\" }
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

    // Chain the edge synthesizer (#103): fresh workstream_signals from
    // this pass should immediately produce INCLUDES + CO_ATTENDED + new
    // MENTIONED edges. Edge synth has its own lock + TTL, so this is a
    // safe fire-and-forget. Failures are logged, not propagated.
    if let Err(e) = crate::edges::synthesizer::maybe_run(app, false).await {
        eprintln!("[edges] post-cluster synth failed: {e}");
    }

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
    // Workstream lists + per-source snapshots come from the same lock
    // window so the prompt sees a coherent point-in-time view. Each
    // source declares its own window+cap (see `signals::Signal`); the
    // synthesizer just iterates the registry.
    let registry = signals::registry();
    let (existing_active, existing_archived, mut snapshots, team, open_actions_by_ws) = {
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

        // Per-source snapshots, keyed by kind. Order is the registry's
        // prompt-section order — the build_user_message loop relies on
        // it; storing as a Vec preserves that.
        let mut snapshots: Vec<(&'static str, Vec<SnapshotItem>)> = Vec::new();
        for src in registry.iter_in_prompt_order() {
            let items = src
                .snapshot(&c, now_ms, src.default_window(), src.default_cap())
                .map_err(|e| format!("{kind} snapshot: {e}", kind = src.kind()))?;
            snapshots.push((src.kind(), items));
        }
        let team = team::list_team_members_raw(&c).unwrap_or_default();
        // Existing open actions per workstream — fed into the prompt so
        // the LLM can dedupe against them instead of re-emitting near
        // duplicates every pass (#101).
        let open_actions = persist::list_open_action_texts_grouped(&c)
            .unwrap_or_default();
        (active, archived, snapshots, team, open_actions)
    };
    let team_by_id: std::collections::HashMap<String, String> = team
        .iter()
        .map(|m| (m.id.clone(), m.display_name.clone()))
        .collect();

    let total_items: usize = snapshots.iter().map(|(_, v)| v.len()).sum();
    if total_items == 0 {
        eprintln!("[workstreams] no recent items to cluster; skipping");
        return Ok(ClusterReport {
            state: "skipped".into(),
            last_clustered_ms: now_ms,
            ..Default::default()
        });
    }

    // ---- 2. Lazy-fetch missing email bodies -----------------------------
    // Email is the only source today that needs a pre-render async
    // network step. Bound the work to MAX_BODY_FETCHES per pass; older
    // messages stay preview-only. Errors per message are logged and
    // skipped — a body fetch failure must not abort the cluster pass.
    backfill_email_bodies(app, conn_state, &mut snapshots).await;

    // ---- 3. Build prompt, label maps ------------------------------------
    let (user_message, label_maps) = build_user_message(
        &existing_active,
        &existing_archived,
        &snapshots,
        registry,
        &team_by_id,
        &open_actions_by_ws,
    );
    let items_clustered = total_items as u32;

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

/// Pre-render step that mutates the email snapshot in place: lazy-fetch
/// missing `body_html` for up to `MAX_BODY_FETCHES` messages so the
/// cluster prompt has rich excerpts. Other sources are left untouched.
/// Failures are logged and the message is left preview-only — they
/// must not abort the cluster pass.
async fn backfill_email_bodies(
    app: &AppHandle,
    conn_state: &std::sync::Mutex<rusqlite::Connection>,
    snapshots: &mut [(&'static str, Vec<SnapshotItem>)],
) {
    let Some((_, items)) = snapshots.iter_mut().find(|(k, _)| *k == "email") else {
        return;
    };
    let mut fetched = 0usize;
    for item in items.iter_mut() {
        if fetched >= MAX_BODY_FETCHES {
            break;
        }
        let m = match &mut item.payload {
            SnapshotPayload::Email(m) => m,
            _ => continue,
        };
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

/// Maps from Claude-facing labels (M1, E1, N1, …) back to canonical
/// IDs, keyed by source `kind` ("email", "event", "note", …). Built
/// from the registry so adding a new source extends this map by one
/// entry without any synthesizer-side surgery. The synthetic kind
/// `"team"` carries team-member labels (T1, T2, …) used for action
/// `owner_label` resolution.
struct LabelMaps {
    by_kind: HashMap<&'static str, HashMap<String, String>>,
}

impl LabelMaps {
    fn empty() -> Self {
        Self {
            by_kind: HashMap::new(),
        }
    }
    fn lookup(&self, kind: &str, label: &str) -> Option<&String> {
        self.by_kind.get(kind).and_then(|m| m.get(label))
    }
}

fn build_user_message(
    existing_active: &[Workstream],
    existing_archived: &[Workstream],
    snapshots: &[(&'static str, Vec<SnapshotItem>)],
    registry: &SignalRegistry,
    team_by_id: &std::collections::HashMap<String, String>,
    open_actions_by_ws: &std::collections::HashMap<String, Vec<String>>,
) -> (String, LabelMaps) {
    let mut s = String::new();
    s.push_str("# Existing workstreams (active)\n\n");
    if existing_active.is_empty() {
        s.push_str("(none)\n");
    } else {
        // Render parents and standalones at the top level in the order
        // the list builder returned (last_activity DESC). Each parent
        // is followed by its children indented with `↳` so Claude sees
        // the umbrella structure when proposing parent_id (#89).
        let mut children_by_parent: std::collections::HashMap<&str, Vec<&Workstream>> =
            std::collections::HashMap::new();
        for w in existing_active {
            if let Some(p) = w.parent_workstream_id.as_deref() {
                children_by_parent.entry(p).or_default().push(w);
            }
        }
        for w in existing_active {
            if w.parent_workstream_id.is_some() {
                continue;
            }
            format_existing_workstream_entry(
                &mut s,
                w,
                team_by_id,
                open_actions_by_ws.get(&w.id),
                false,
            );
            if let Some(children) = children_by_parent.get(w.id.as_str()) {
                for child in children {
                    format_existing_workstream_entry(
                        &mut s,
                        child,
                        team_by_id,
                        open_actions_by_ws.get(&child.id),
                        true,
                    );
                }
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

    let mut maps = LabelMaps::empty();

    // Team section. Labels T1, T2, … resolve to team_members.id so the
    // LLM can assign an `owner_label` per action. Sorted by display
    // name for stable labels across runs.
    if !team_by_id.is_empty() {
        let mut team_pairs: Vec<(&String, &String)> = team_by_id.iter().collect();
        team_pairs.sort_by(|a, b| a.1.cmp(b.1));
        s.push_str("\n# Team\n\n");
        let team_map = maps.by_kind.entry("team").or_default();
        for (i, (id, name)) in team_pairs.iter().enumerate() {
            let label = format!("T{}", i + 1);
            team_map.insert(label.clone(), (*id).clone());
            s.push_str(&format!("[{label}] {name}\n"));
        }
    }

    let mut prefixes: Vec<&'static str> = Vec::new();
    // One section per registered source, in registry order. Adding a
    // 4th source (e.g. GitHub) is a pure-add — no churn here.
    for (kind, items) in snapshots {
        let src = match registry.get(kind) {
            Some(s) => s,
            None => {
                eprintln!("[workstreams] snapshot kind {kind} has no registered Signal");
                continue;
            }
        };
        prefixes.push(src.label_prefix());
        s.push_str(&format!("\n# {}\n\n", src.prompt_section_title()));
        if items.is_empty() {
            s.push_str("(none)\n");
            continue;
        }
        let kind_map = maps.by_kind.entry(*kind).or_default();
        for (i, item) in items.iter().enumerate() {
            let label = format!("{}{}", src.label_prefix(), i + 1);
            kind_map.insert(label.clone(), item.id.clone());
            s.push_str(&src.format(&label, item));
        }
    }

    let label_glob = if prefixes.is_empty() {
        String::new()
    } else {
        prefixes
            .iter()
            .map(|p| format!("{p}*"))
            .collect::<Vec<_>>()
            .join("/")
    };
    s.push_str(&format!(
        "\n# Instructions\n\nReturn a JSON array matching the schema in the system prompt. \
         Reuse an existing workstream id when the new items extend it; spawn a new one (id: null) \
         only when no existing fit. Each action's source_label MUST be one of the labels above \
         ({label_glob}). Output JSON only — no prose, no fences.\n"
    ));

    (s, maps)
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

/// Render one existing-workstream entry in the cluster-pass prompt.
/// Top-level workstreams render flush left; children render indented
/// with a "↳" marker so Claude sees the parent → child structure (#89).
/// The same per-row continuation lines (Owner / Members / Notes) are
/// emitted for both, just under a deeper indent for children.
/// Max open actions rendered per workstream in the prompt (#101).
/// Keeps token usage bounded while giving the LLM enough context to
/// dedupe against most real-world workstreams.
const OPEN_ACTIONS_PER_WORKSTREAM_CAP: usize = 12;
/// Per-line cap for an open action's text in the prompt. Long action
/// bodies get a trailing ellipsis.
const OPEN_ACTION_LINE_CAP: usize = 200;

fn format_existing_workstream_entry(
    s: &mut String,
    w: &Workstream,
    team_by_id: &std::collections::HashMap<String, String>,
    open_actions: Option<&Vec<String>>,
    is_child: bool,
) {
    let head_prefix = if is_child { "   ↳ " } else { "" };
    let cont_prefix = if is_child { "      " } else { "   " };
    s.push_str(&format!(
        "{head_prefix}[{id}] {title} — {summary} (active)\n",
        id = w.id,
        title = w.title,
        summary = summarize_one_line(&w.summary)
    ));
    if let Some(owner_id) = w.owner_member_id.as_deref() {
        if let Some(name) = team_by_id.get(owner_id) {
            s.push_str(&format!("{cont_prefix}Owner: {name}\n"));
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
                "{cont_prefix}Members: {names}{suffix}\n",
                names = names.join(", "),
                suffix = suffix
            ));
        }
    }
    if let Some(notes) = w.user_notes.as_deref().filter(|s| !s.trim().is_empty()) {
        let collapsed = collapse_ws(notes);
        let truncated = truncate_chars(&collapsed, USER_NOTES_PROMPT_CAP);
        s.push_str(&format!(
            "{cont_prefix}Notes (user-authored, ground truth): {truncated}\n"
        ));
    }
    // Existing open actions — rendered so the LLM can skip near
    // duplicates instead of re-emitting them every pass (#101).
    if let Some(actions) = open_actions {
        if !actions.is_empty() {
            s.push_str(&format!("{cont_prefix}Open actions (already tracked):\n"));
            let shown = actions.iter().take(OPEN_ACTIONS_PER_WORKSTREAM_CAP);
            for a in shown {
                let collapsed = collapse_ws(a);
                let truncated = truncate_chars(&collapsed, OPEN_ACTION_LINE_CAP);
                s.push_str(&format!("{cont_prefix}  - {truncated}\n"));
            }
            if actions.len() > OPEN_ACTIONS_PER_WORKSTREAM_CAP {
                s.push_str(&format!(
                    "{cont_prefix}  (+{} more not shown)\n",
                    actions.len() - OPEN_ACTIONS_PER_WORKSTREAM_CAP
                ));
            }
        }
    }
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

fn format_iso_date(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
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
    /// Optional parent workstream id (#89). Synthesizer's write path
    /// validates against the 2-level cap before persisting; invalid
    /// values are dropped to NULL. Only honored on insert — existing
    /// workstreams' parent is user-only authority.
    #[serde(default)]
    parent_id: Option<String>,
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
    /// Optional team label (T1, T2, …) the LLM picked for the action's
    /// owner. Resolved at parse time via label_maps.by_kind["team"];
    /// unknown labels are dropped silently with a log line. None means
    /// "no owner / the user themselves" — surfaces as NULL assignee.
    #[serde(default)]
    owner_label: Option<String>,
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

        let parent_id = raw
            .parent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "null")
            .map(|s| s.to_string());

        let members = raw.members.unwrap_or(RawMembers {
            emails: Vec::new(),
            events: Vec::new(),
            notes: Vec::new(),
        });

        let member_emails = map_labels(&members.emails, label_maps, "email");
        let member_events = map_labels(&members.events, label_maps, "event");
        let member_notes = map_labels(&members.notes, label_maps, "note");

        let actions = raw
            .actions
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| {
                let text = match a.text.as_deref().map(str::trim) {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => return None,
                };
                if is_mere_participation(&text) {
                    eprintln!(
                        "[workstreams] dropping participation-only action: {text}"
                    );
                    return None;
                }
                let kind = a.source_kind.unwrap_or_default();
                let label = a.source_label.unwrap_or_default();
                let source_id = label_maps.lookup(&kind, &label).cloned();
                let source_id = match source_id {
                    Some(s) => s,
                    None => {
                        eprintln!(
                            "[workstreams] dropping action with unknown label {label} kind {kind}: {text}"
                        );
                        return None;
                    }
                };
                let assignee_id = a
                    .owner_label
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "null")
                    .and_then(|label| {
                        let resolved = label_maps.lookup("team", label).cloned();
                        if resolved.is_none() {
                            eprintln!(
                                "[workstreams] dropping unknown owner_label {label} for action: {text}"
                            );
                        }
                        resolved
                    });
                Some(SynthesizedAction {
                    text,
                    due_ms: a.due_ms,
                    source_kind: kind,
                    source_id,
                    assignee_id,
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
            parent_id,
        });
    }
    out
}

/// Safety net for the LLM ignoring the "no mere participation" rule
/// in the prompt: drops items whose entire content is just attending
/// or joining a meeting/call/event. Matches phrasings like
/// "Attend the demo", "Join standup Friday", "Be present at kickoff",
/// "Show up to the review". Anything with a comma, semicolon, or
/// follow-up verb after the participation phrase escapes the filter —
/// those are no longer "merely" participation.
fn is_mere_participation(text: &str) -> bool {
    let s = text.trim().trim_end_matches(['.', '!', '?']).to_lowercase();
    if s.contains(',') || s.contains(';') || s.contains(" and ") {
        return false;
    }
    const LEAD: &[&str] = &[
        "attend",
        "join",
        "be at",
        "be present at",
        "be present for",
        "be on",
        "go to",
        "show up to",
        "show up for",
        "participate in",
        "sit in on",
        "dial in to",
        "dial into",
    ];
    LEAD.iter().any(|p| s.starts_with(p))
}

fn map_labels(
    labels: &[String],
    maps: &LabelMaps,
    kind: &str,
) -> Vec<String> {
    let mut out = Vec::with_capacity(labels.len());
    let mut seen = HashSet::new();
    for label in labels {
        match maps.lookup(kind, label.trim()) {
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
        let mut by_kind: HashMap<&'static str, HashMap<String, String>> = HashMap::new();
        let mut emails = HashMap::new();
        emails.insert("M1".to_string(), "mg:test::msg-1".to_string());
        emails.insert("M2".to_string(), "mg:test::msg-2".to_string());
        by_kind.insert("email", emails);
        let mut events = HashMap::new();
        events.insert("E1".to_string(), "mg:test::ev-1".to_string());
        by_kind.insert("event", events);
        let mut notes = HashMap::new();
        notes.insert("N1".to_string(), "/notes/a.md".to_string());
        by_kind.insert("note", notes);
        let mut team = HashMap::new();
        team.insert("T1".to_string(), "tm_alice".to_string());
        team.insert("T2".to_string(), "tm_bob".to_string());
        by_kind.insert("team", team);
        LabelMaps { by_kind }
    }

    #[test]
    fn parse_synthesizer_response_resolves_owner_label() {
        let raw = r#"[
            {
                "title": "WS",
                "summary": "",
                "members": { "emails": ["M1"], "events": [], "notes": [] },
                "actions": [
                    { "text": "Send recap", "source_kind": "email", "source_label": "M1", "owner_label": "T1" },
                    { "text": "Other", "source_kind": "email", "source_label": "M1", "owner_label": "T99" },
                    { "text": "Unowned", "source_kind": "email", "source_label": "M1" }
                ]
            }
        ]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        let actions = &parsed[0].actions;
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].assignee_id.as_deref(), Some("tm_alice"));
        assert!(
            actions[1].assignee_id.is_none(),
            "unknown owner_label resolves to None instead of dropping the action",
        );
        assert!(actions[2].assignee_id.is_none());
    }

    #[test]
    fn parse_synthesizer_response_drops_mere_participation() {
        let raw = r#"[
            {
                "title": "WS",
                "summary": "",
                "members": { "emails": ["M1"], "events": ["E1"], "notes": [] },
                "actions": [
                    { "text": "Attend the Talgo demo", "source_kind": "event", "source_label": "E1" },
                    { "text": "Join the kickoff call", "source_kind": "event", "source_label": "E1" },
                    { "text": "Send recap after the demo", "source_kind": "event", "source_label": "E1" },
                    { "text": "Attend the demo and present slides", "source_kind": "event", "source_label": "E1" }
                ]
            }
        ]"#;
        let parsed = parse_synthesizer_response(raw, &label_maps());
        assert_eq!(parsed.len(), 1);
        let texts: Vec<&str> = parsed[0]
            .actions
            .iter()
            .map(|a| a.text.as_str())
            .collect();
        assert!(
            !texts.iter().any(|t| *t == "Attend the Talgo demo"),
            "pure participation must be filtered",
        );
        assert!(
            !texts.iter().any(|t| *t == "Join the kickoff call"),
            "pure participation must be filtered",
        );
        assert!(
            texts.iter().any(|t| *t == "Send recap after the demo"),
            "non-participation actions must survive",
        );
        assert!(
            texts.iter().any(|t| *t == "Attend the demo and present slides"),
            "items with a follow-up clause escape the filter",
        );
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

    // ----- Prompt parity (#86) -------------------------------------------
    //
    // build_user_message is now driven by the registry. Lock the byte-
    // exact section structure so a future "add a 4th source" PR doesn't
    // accidentally reflow the email/event/note rendering. One item per
    // section keeps the assertion readable; the per-source format()
    // logic is covered in `signals` tests.

    fn make_email() -> SnapshotItem {
        let m = email::EmailMessage {
            id: "mg:test::m1".into(),
            connector_id: "mg:test".into(),
            external_id: "m1".into(),
            thread_id: "t".into(),
            subject: "Hi".into(),
            from_email: "alice@x.io".into(),
            from_name: Some("Alice".into()),
            sent_at_ms: 1_700_000_000_000,
            body_preview: Some("Body text".into()),
            body_html: None,
            has_attachments: false,
            is_read: true,
            raw_etag: None,
            modified_ms: 1_700_000_000_000,
            recipients: Vec::new(),
        };
        SnapshotItem {
            id: m.id.clone(),
            payload: SnapshotPayload::Email(m),
        }
    }

    fn make_event() -> SnapshotItem {
        let e = crate::connectors::calendar::CalendarEvent {
            id: "mg:test::e1".into(),
            connector_id: "mg:test".into(),
            external_id: "e1".into(),
            title: "Sync".into(),
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
            attendees: Vec::new(),
        };
        SnapshotItem {
            id: e.id.clone(),
            payload: SnapshotPayload::Event(e),
        }
    }

    fn make_note() -> SnapshotItem {
        let n = super::super::NoteRef {
            note_path: "/notes/a.md".into(),
            title: "Plan".into(),
            modified_ms: 1_700_000_000_000,
        };
        SnapshotItem {
            id: n.note_path.clone(),
            payload: SnapshotPayload::Note(n),
        }
    }

    #[test]
    fn build_user_message_matches_locked_layout() {
        let registry = signals::registry();
        let snapshots: Vec<(&'static str, Vec<SnapshotItem>)> = vec![
            ("email", vec![make_email()]),
            ("event", vec![make_event()]),
            ("note", vec![make_note()]),
        ];
        let team: HashMap<String, String> = HashMap::new();
        let (prompt, maps) = build_user_message(&[], &[], &snapshots, registry, &team, &HashMap::new());

        let expected = concat!(
            "# Existing workstreams (active)\n\n",
            "(none)\n",
            "\n# Recent emails (last 14 days)\n\n",
            "[M1] 2023-11-14 — From: Alice <alice@x.io> — Subject: Hi\n",
            "    Body text\n\n",
            "\n# Recent calendar events (window: -14d .. +14d)\n\n",
            "[E1] 2023-11-14 22:13 — Sync\n",
            "\n# Recent notes (last 30 days)\n\n",
            "[N1] 2023-11-14 — Plan\n",
            "\n# Instructions\n\n",
            "Return a JSON array matching the schema in the system prompt. ",
            "Reuse an existing workstream id when the new items extend it; spawn a new one (id: null) ",
            "only when no existing fit. Each action's source_label MUST be one of the labels above ",
            "(M*/E*/N*). Output JSON only — no prose, no fences.\n",
        );

        assert_eq!(prompt, expected, "prompt layout drifted");

        // Label maps were built keyed by kind.
        assert_eq!(
            maps.lookup("email", "M1").map(String::as_str),
            Some("mg:test::m1")
        );
        assert_eq!(
            maps.lookup("event", "E1").map(String::as_str),
            Some("mg:test::e1")
        );
        assert_eq!(
            maps.lookup("note", "N1").map(String::as_str),
            Some("/notes/a.md")
        );
        // Cross-kind lookup must miss (event label can't appear under email).
        assert!(maps.lookup("email", "E1").is_none());
    }

    #[test]
    fn build_user_message_renders_none_for_empty_section() {
        let registry = signals::registry();
        let snapshots: Vec<(&'static str, Vec<SnapshotItem>)> = vec![
            ("email", vec![]),
            ("event", vec![]),
            ("note", vec![]),
        ];
        let team: HashMap<String, String> = HashMap::new();
        let (prompt, maps) = build_user_message(&[], &[], &snapshots, registry, &team, &HashMap::new());
        assert!(prompt.contains("\n# Recent emails (last 14 days)\n\n(none)\n"));
        assert!(prompt.contains("\n# Recent calendar events (window: -14d .. +14d)\n\n(none)\n"));
        assert!(prompt.contains("\n# Recent notes (last 30 days)\n\n(none)\n"));
        assert!(maps.by_kind.is_empty(), "no items, no labels recorded");
    }
}
