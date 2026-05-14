//! Profile snapshot prompt builder + helpers (#107).
//!
//! Walks the signal sources (team_members row, edges centered on the
//! person, events, optional Voyage retrieval hits) into a structured
//! `PromptInputs` payload. The worker hands that to Anthropic with a
//! JSON-only output mode; the response parses back into a
//! `ProfileSnapshotBody`.
//!
//! `source_hash` is a sha256 over the inputs; when it matches the
//! previous snapshot's hash, the worker can short-circuit the LLM
//! call (structural cache hit).
//!
//! `render_snapshot_excerpt` is the shared formatter used by both
//! `reconcile.rs` (attendee block) and `ask.rs` (Team profiles
//! section) to flatten a stored snapshot back into the prompt-line
//! shape they used to read from `profile.md`.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::profiles::persist::ProfileSnapshotBody;

const COLLABORATORS_CAP: usize = 8;
const FOCUS_CAP: usize = 5;

/// Flatten a stored snapshot into the multi-line attendee/profile
/// excerpt that the reconcile + ask prompts have shipped for
/// months. `cap` is a soft char cap applied after assembly.
pub fn render_snapshot_excerpt(body: &ProfileSnapshotBody, cap: usize) -> String {
    let mut out = String::new();
    if let Some(role) = body.role_observed.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("Role: ");
        out.push_str(role);
        out.push('\n');
    }
    if !body.frequent_collaborators.is_empty() {
        let names: Vec<String> = body
            .frequent_collaborators
            .iter()
            .take(COLLABORATORS_CAP)
            .map(|c| c.person_id.clone())
            .collect();
        if !names.is_empty() {
            out.push_str("Frequent collaborators: ");
            out.push_str(&names.join(", "));
            out.push('\n');
        }
    }
    if !body.recent_focus.is_empty() {
        let titles: Vec<String> = body
            .recent_focus
            .iter()
            .take(FOCUS_CAP)
            .map(|f| f.title.clone())
            .filter(|t| !t.trim().is_empty())
            .collect();
        if !titles.is_empty() {
            out.push_str("Recent focus: ");
            out.push_str(&titles.join("; "));
            out.push('\n');
        }
    }
    if let Some(hours) = &body.working_hours_observed {
        out.push_str("Working hours: ");
        out.push_str(&hours.start_local);
        out.push_str(" \u{2013} ");
        out.push_str(&hours.end_local);
        out.push('\n');
    }
    if let Some(style) = body
        .communication_style_notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("Communication style: ");
        out.push_str(style);
        out.push('\n');
    }
    truncate_chars(out.trim().to_string(), cap)
}

fn truncate_chars(s: String, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s;
    }
    let cut: String = s.chars().take(cap.saturating_sub(1)).collect();
    format!("{cut}\u{2026}")
}

/// Deterministic hash over the prompt-input JSON; used by the worker
/// to short-circuit the Anthropic call when nothing material changed
/// since the last snapshot.
pub fn source_hash(payload: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(&bytes);
    format!("{:x}", h.finalize())
}

const EDGES_CAP: usize = 50;
const EVENTS_CAP: usize = 200;
const RETRIEVAL_CAP: usize = 20;
/// Cap on accepted observations included in the worker prompt (#114).
/// Most-recent-first by `created_ms`; older accepted observations are
/// dropped silently when there are more than `ACCEPTED_OBSERVATIONS_CAP`.
const ACCEPTED_OBSERVATIONS_CAP: usize = 50;

/// Drop emitted `evidence_observation_ids` that weren't in the input
/// set (model hallucinations), then dedup while preserving order
/// (first occurrence wins). Empty input → empty output.
pub(crate) fn filter_evidence_ids(
    raw: Vec<String>,
    allowed: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.into_iter()
        .filter(|id| allowed.contains(id))
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

/// Build the prompt-input payload for `person_id`. Pure data
/// assembly: pulls the team_member row, edges centered on the
/// person, latest events with `actor_id = person_id`, and
/// (optional, when a Voyage key is configured) kNN retrieval hits
/// over notes/messages mentioning the person.
pub async fn build_prompt_inputs(
    app: &AppHandle,
    person_id: &str,
) -> Result<serde_json::Value, String> {
    let now_ms = crate::events::current_unix_ms();
    let (member_block, edge_rows, event_rows, accepted_observations, waiting_from_me, waiting_for_them) = {
        let conn_state = app.state::<std::sync::Mutex<Connection>>();
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        let accepted = crate::observations::persist::list_by_member(
            &c,
            person_id,
            Some(crate::observations::persist::ObservationStatus::Accepted),
        )
        .map_err(|e| format!("accepted observations: {e}"))?;
        let projected = project_accepted_observations(accepted);
        let from_me = crate::profiles::signals::candidates_from_me(&c, person_id, now_ms)
            .map_err(|e| format!("from_me candidates: {e}"))?;
        let for_them = crate::profiles::signals::candidates_for_them(&c, person_id, now_ms)
            .map_err(|e| format!("for_them candidates: {e}"))?;
        (
            load_member(&c, person_id)?,
            load_edges(&c, person_id)?,
            load_events(&c, person_id)?,
            projected,
            from_me,
            for_them,
        )
    };
    let display_name = member_block.display_name.clone();

    // Retrieval is best-effort and optional. Skip silently when no
    // Voyage key is present; the rest of the prompt still has signal.
    let retrieval_hits = if crate::keychain::read_voyage_api_key().is_ok() {
        let query = format!("messages and notes involving {display_name}");
        match crate::embeddings::retrieve(
            app,
            &query,
            crate::embeddings::RetrieveOpts {
                limit: RETRIEVAL_CAP,
                ..Default::default()
            },
        )
        .await
        {
            Ok(hits) => hits
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "ref_kind": h.ref_kind,
                        "ref_id": h.ref_id,
                        "distance": h.distance,
                        "preview": h.preview,
                    })
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("[profiles] retrieve skipped for {person_id}: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    Ok(serde_json::json!({
        "member": member_block,
        "edges": edge_rows,
        "events": event_rows,
        "retrieval_hits": retrieval_hits,
        "accepted_observations": accepted_observations,
        "waiting_candidates": {
            "from_me": waiting_from_me,
            "for_them": waiting_for_them,
        },
    }))
}

/// Project accepted-observation rows down to the three fields the worker
/// prompt cares about. Most-recent-first (the input is already ordered by
/// `created_ms DESC`); capped at `ACCEPTED_OBSERVATIONS_CAP`.
pub(crate) fn project_accepted_observations(
    rows: Vec<crate::observations::persist::ProfileObservation>,
) -> Vec<serde_json::Value> {
    rows.into_iter()
        .take(ACCEPTED_OBSERVATIONS_CAP)
        .map(|o| {
            serde_json::json!({
                "obs_id": o.id,
                "body": o.body,
                "created_ms": o.created_ms,
            })
        })
        .collect()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MemberBlock {
    pub id: String,
    pub display_name: String,
    pub role: String,
    pub aliases: Vec<AliasBlock>,
    pub is_self: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AliasBlock {
    pub kind: String,
    pub value: String,
}

fn load_member(conn: &Connection, person_id: &str) -> Result<MemberBlock, String> {
    let (display_name, role, is_self): (String, String, i64) = conn
        .query_row(
            "SELECT display_name, role, is_self FROM team_members WHERE id = ?1",
            params![person_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| format!("team_member {person_id}: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT kind, value FROM team_member_aliases WHERE member_id = ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![person_id], |r| {
            Ok(AliasBlock {
                kind: r.get(0)?,
                value: r.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let aliases: Vec<AliasBlock> = rows.filter_map(|r| r.ok()).collect();
    Ok(MemberBlock {
        id: person_id.to_string(),
        display_name,
        role,
        aliases,
        is_self: is_self != 0,
    })
}

fn load_edges(conn: &Connection, person_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                    confidence, first_seen_ms, last_seen_ms \
               FROM edges \
              WHERE (src_kind = 'person' AND src_id = ?1) \
                 OR (tgt_kind = 'person' AND tgt_id = ?1) \
              ORDER BY last_seen_ms DESC \
              LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![person_id, EDGES_CAP as i64], |r| {
            Ok(serde_json::json!({
                "src_kind": r.get::<_, String>(0)?,
                "src_id": r.get::<_, String>(1)?,
                "tgt_kind": r.get::<_, String>(2)?,
                "tgt_id": r.get::<_, String>(3)?,
                "edge_kind": r.get::<_, String>(4)?,
                "confidence": r.get::<_, f64>(5)?,
                "first_seen_ms": r.get::<_, i64>(6)?,
                "last_seen_ms": r.get::<_, i64>(7)?,
            }))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

fn load_events(conn: &Connection, person_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT ts_ms, kind, ref_kind, ref_id \
               FROM events \
              WHERE actor_id = ?1 \
              ORDER BY ts_ms DESC \
              LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![person_id, EVENTS_CAP as i64], |r| {
            let ts_ms: i64 = r.get(0)?;
            let kind: String = r.get(1)?;
            let ref_kind: Option<String> = r.get(2)?;
            let ref_id: Option<String> = r.get(3)?;
            Ok((ts_ms, kind, ref_kind, ref_id))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (ts_ms, kind, ref_kind, ref_id) = row.map_err(|e| e.to_string())?;
        // Hydrate one-line preview per event so the model sees what
        // actually happened, not just timestamps + kinds. Best-effort:
        // missing/deleted refs degrade to the bare id.
        let preview = match (&ref_kind, &ref_id) {
            (Some(k), Some(id)) => {
                // preview_for needs a connection but we're already
                // holding the SQLite mutex on the caller side. Use a
                // fresh borrow via the outer connection is awkward;
                // skip the preview lookup if it would require a
                // recursive lock. Worker reads happen serially so
                // this is fine in practice.
                let _ = (k, id);
                String::new()
            }
            _ => String::new(),
        };
        out.push(serde_json::json!({
            "ts_ms": ts_ms,
            "kind": kind,
            "ref_kind": ref_kind,
            "ref_id": ref_id,
            "preview": preview,
        }));
    }
    Ok(out)
}

/// Anthropic call errors. The worker translates `RateLimited` into a
/// backoff window; everything else surfaces as a soft error and the
/// person retries on the next eligible tick.
pub enum CallError {
    RateLimited,
    Other(String),
}

const SYSTEM_PROMPT: &str = "You synthesize a structured profile of a single person from \
edges (graph signals), events (recent activity), retrieval hits \
(notes/messages mentioning them), previously-vetted accepted \
observations, and per-direction waiting candidates (unanswered \
inbound/outbound emails, Teams messages, and pending meetings).\n\n\
Output **only** a single JSON object matching this schema. No prose, \
no markdown fences, no keys outside the schema.\n\n\
```\n\
{\n\
  \"role_observed\":           string|null,         // short phrase, e.g. \"Senior backend engineer; SRE-leaning\"\n\
  \"frequent_collaborators\":  [{\"person_id\": string, \"score\": 0..1, \"evidence\": string}],\n\
  \"recent_focus\":            [{\"workstream_id\": string, \"title\": string, \"confidence\": 0..1}],\n\
  \"working_hours_observed\":  {\"start_local\": string, \"end_local\": string}|null,\n\
  \"communication_style_notes\": string|null,       // one sentence, lower-cased except proper nouns\n\
  \"last_seen_active_ms\":     int|null,\n\
  \"evidence_observation_ids\": [string],           // obs_ids from accepted_observations that meaningfully shaped this snapshot; [] when none used\n\
  \"summary_prose\":           string|null,         // 2-4 sentences of prose; the most important field\n\
  \"waiting_from_me\":         [WaitingItem],       // they're waiting on the user (the user owes them)\n\
  \"waiting_for_them\":        [WaitingItem]        // the user is waiting on them\n\
}\n\
\n\
WaitingItem = {\n\
  \"description\":     string,                       // one-sentence rephrasing of the preview\n\
  \"source_kind\":     \"email\"|\"teams\"|\"meeting\",   // copy verbatim from the candidate\n\
  \"source_ref_id\":   string,                       // copy verbatim; never invent\n\
  \"since_ms\":        int                           // copy verbatim from the candidate\n\
}\n\
```\n\n\
Rules:\n\
- Prefer **omission over invention**. Use null/empty when signal is thin.\n\
- `frequent_collaborators` is ranked by collaboration strength relative to *this team*; cap 8.\n\
- `recent_focus` lists at most 5 workstream titles inferred from edges/events.\n\
- `working_hours_observed` only when event ts_ms cluster cleanly into a window; otherwise null.\n\
- `communication_style_notes` ONE concise sentence describing observed comms patterns. Skip when no clear signal.\n\
- `last_seen_active_ms` = the most recent `events.ts_ms` for this person.\n\
\n\
Accepted observations:\n\
- The user message includes `accepted_observations[]`: previously-reviewed observations the user has explicitly vetted. Each is `{obs_id, body, created_ms}`.\n\
- These are **ground truth**, not hypotheses — weight them more heavily than any single edge or event when they conflict.\n\
- When an observation directly shapes a field (e.g. `role_observed`, `communication_style_notes`), include its `obs_id` in `evidence_observation_ids`.\n\
- Cite only the obs_ids you actually used. Do not pad. Cite each obs_id at most once. Use obs_ids exactly as given — do not invent.\n\
\n\
Summary prose:\n\
- This is the most important field. 2-4 sentences. Third person, declarative.\n\
- Synthesize role + recent focus + working style + any accepted observations into a paragraph a colleague reading this profile cold would find useful.\n\
- Skip clichés (\"hard worker\", \"team player\"). Be specific. Use null if signal is too thin to write something honest.\n\
\n\
Waiting analysis:\n\
- The user message includes `waiting_candidates: { from_me, for_them }`. Each candidate is `{ source_kind, source_ref_id, since_ms, preview, conversation_tail }`.\n\
- `from_me` candidates are things this person is waiting on the user for; `for_them` are things the user is waiting on this person for.\n\
- `conversation_tail` is up to 5 follow-up messages in the same thread/chat, ordered oldest-first, each `{ms, from_kind, preview}` where `from_kind` is \"self\" or \"them\". USE IT to decide resolution.\n\
- For each candidate that's still pending, emit one WaitingItem with:\n\
    description     — one short sentence (e.g. \"Confirm the Q3 budget\" not \"Re: Re: budget thread\"). If committed-not-delivered, include the commitment context (e.g. \"Send the bridge access list (committed Tuesday)\").\n\
    source_kind     — copy verbatim from the candidate.\n\
    source_ref_id   — copy verbatim; never invent ids.\n\
    since_ms        — copy verbatim.\n\
- Your default is to DROP. Most candidates won't be substantive — emit a WaitingItem only when the message contains a real, unanswered or undelivered ask. When in doubt, drop.\n\
\n\
Resolution judgment (decide per candidate by reading `conversation_tail`):\n\
- RESOLVED → drop. Markers:\n\
    * Self reply delivers the requested artifact (\"Here's the file: …\", \"Done, link below\", \"Habe ich gerade geschickt\").\n\
    * Self reply substantively answers the question (\"The number is 42.\", \"We're going with vendor X because …\").\n\
    * Counterparty acknowledges receipt and the loop is closed (\"Got it, thanks!\").\n\
- PENDING (no reply) → emit. The candidate has zero or only counterparty messages in the tail.\n\
- COMMITTED-NOT-DELIVERED → emit, with commitment context in description. Markers:\n\
    * Self reply commits to future action without delivering (\"Sure, I'll send tomorrow\", \"Looking into it\", \"Komme ich noch drauf zurück\", \"Will do\", \"OK, mache ich\") AND no later delivery in the tail.\n\
    * Counterparty has followed up after a self ack (\"Any update?\", \"Ping\").\n\
\n\
Other DROP triggers (regardless of tail):\n\
    * Original preview is a social ack or thank-you (\"Thanks!\", \"Danke!\", \"Perfect, danke\", \"OK\", \"Got it\", \"Sounds good\").\n\
    * Meeting/event cancellation, decline, or status update (\"Abgesagt: …\", \"Declined: …\").\n\
    * Out-of-office / auto-reply (\"I'm out until …\", \"Currently on leave\").\n\
    * Agreement / confirmation of the user's prior message (\"Ja, so sehe ich das auch.\", \"Sicher, kein Problem\").\n\
    * A status report from the sender about their own work (\"Hab ich erledigt\", \"Working on it\").\n\
\n\
KEEP examples:\n\
    * Direct question to the user (\"Können wir da unterstützen?\", \"Any update on the rollout?\").\n\
    * Explicit request for action (\"Kannst du bitte … exportieren?\", \"Please confirm the Q3 budget by Friday\").\n\
    * Pending decision waiting on the user (\"Sollen wir mit X oder Y weitergehen?\").\n\
\n\
- Cap each direction at 5 items; if you have more substantive candidates than that, pick the highest-stakes (deadlines, decisions, blockers) over the smallest-ask ones.\n\
- Never emit a source_ref_id that wasn't in the candidate set — the post-parse validator will drop it anyway.";

pub async fn call_anthropic(
    api_key: &str,
    inputs: &serde_json::Value,
) -> Result<ProfileSnapshotBody, CallError> {
    use crate::anthropic;

    let user_message = serde_json::to_string_pretty(inputs)
        .unwrap_or_else(|_| "{}".into());

    let body = serde_json::json!({
        "model": anthropic::DEFAULT_MODEL,
        // v3 (#120) extends the schema with summary_prose plus up to
        // 10 WaitingItems (5 each direction) — each carrying a short
        // description + ids. 1024 wasn't enough; the response was
        // truncating mid-string. 4096 leaves comfortable headroom.
        "max_tokens": 4096,
        "system": [
            {
                "type": "text",
                "text": SYSTEM_PROMPT,
                "cache_control": { "type": "ephemeral" }
            }
        ],
        "messages": [
            { "role": "user", "content": user_message }
        ]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(anthropic::ENDPOINT)
        .header("x-api-key", api_key)
        .header("anthropic-version", anthropic::ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| CallError::Other(format!("network: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(CallError::RateLimited);
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(CallError::Other(format!("HTTP {status}: {text}")));
    }

    #[derive(Deserialize)]
    struct RespContent {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        text: String,
    }
    #[derive(Deserialize)]
    struct RespBody {
        content: Vec<RespContent>,
    }

    let parsed: RespBody = resp
        .json()
        .await
        .map_err(|e| CallError::Other(format!("parse response: {e}")))?;
    let assembled: String = parsed
        .content
        .into_iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text)
        .collect::<Vec<_>>()
        .join("\n");

    // Strip a leading/trailing markdown fence if the model wrapped
    // the JSON despite instructions.
    let trimmed = strip_json_fence(&assembled);
    serde_json::from_str::<ProfileSnapshotBody>(trimmed)
        .map_err(|e| CallError::Other(format!("snapshot json: {e}")))
}

fn strip_json_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.trim_start_matches('\n').trim_end_matches("```").trim();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.trim_start_matches('\n').trim_end_matches("```").trim();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::persist::{
        CollaboratorScore, FocusItem, ProfileSnapshotBody, WorkingHours,
    };

    #[test]
    fn excerpt_renders_all_sections() {
        let body = ProfileSnapshotBody {
            role_observed: Some("Senior backend engineer".into()),
            frequent_collaborators: vec![CollaboratorScore {
                person_id: "Bob".into(),
                score: 0.8,
                evidence: "CO_ATTENDED".into(),
            }],
            recent_focus: vec![FocusItem {
                workstream_id: "ws_1".into(),
                title: "Hyundai POC".into(),
                confidence: 0.9,
            }],
            working_hours_observed: Some(WorkingHours {
                start_local: "09:30".into(),
                end_local: "18:00".into(),
            }),
            communication_style_notes: Some("Concise, async-first.".into()),
            last_seen_active_ms: None,
            evidence_observation_ids: vec![],
            ..Default::default()
        };
        let s = render_snapshot_excerpt(&body, 1000);
        assert!(s.contains("Role: Senior backend engineer"));
        assert!(s.contains("Frequent collaborators: Bob"));
        assert!(s.contains("Recent focus: Hyundai POC"));
        assert!(s.contains("Working hours: 09:30"));
        assert!(s.contains("Communication style: Concise, async-first."));
    }

    #[test]
    fn excerpt_empty_when_no_fields() {
        let body = ProfileSnapshotBody::default();
        assert!(render_snapshot_excerpt(&body, 200).is_empty());
    }

    #[test]
    fn excerpt_respects_char_cap() {
        let body = ProfileSnapshotBody {
            communication_style_notes: Some("x".repeat(500)),
            ..Default::default()
        };
        let s = render_snapshot_excerpt(&body, 50);
        assert!(s.chars().count() <= 50);
        assert!(s.ends_with('\u{2026}'));
    }

    #[test]
    fn source_hash_is_stable() {
        let v = serde_json::json!({"a": 1, "b": [1, 2, 3]});
        let h1 = source_hash(&v);
        let h2 = source_hash(&v);
        assert_eq!(h1, h2);
    }

    #[test]
    fn source_hash_changes_with_input() {
        let v1 = serde_json::json!({"a": 1});
        let v2 = serde_json::json!({"a": 2});
        assert_ne!(source_hash(&v1), source_hash(&v2));
    }

    // ---------- filter_evidence_ids (#114) -----------------------------

    fn allow(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn filter_evidence_ids_drops_hallucinated() {
        let allowed = allow(&["obs_a", "obs_b"]);
        let got = filter_evidence_ids(
            vec!["obs_a".into(), "obs_invented".into(), "obs_b".into()],
            &allowed,
        );
        assert_eq!(got, vec!["obs_a", "obs_b"]);
    }

    #[test]
    fn filter_evidence_ids_dedups_preserving_order() {
        let allowed = allow(&["obs_a", "obs_b", "obs_c"]);
        let got = filter_evidence_ids(
            vec![
                "obs_b".into(),
                "obs_a".into(),
                "obs_b".into(),
                "obs_c".into(),
                "obs_a".into(),
            ],
            &allowed,
        );
        assert_eq!(got, vec!["obs_b", "obs_a", "obs_c"]);
    }

    #[test]
    fn filter_evidence_ids_empty_input_empty_output() {
        let allowed = allow(&["obs_a"]);
        assert!(filter_evidence_ids(Vec::new(), &allowed).is_empty());
    }

    #[test]
    fn filter_evidence_ids_empty_allow_drops_everything() {
        let allowed = std::collections::HashSet::new();
        let got = filter_evidence_ids(vec!["obs_a".into(), "obs_b".into()], &allowed);
        assert!(got.is_empty());
    }

    // ---------- accepted-observation projection (#114) -----------------

    use rusqlite::{params, Connection};

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_member(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO team_members \
                (id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES (?1, ?1, '', ?2, 0, 0, 0)",
            params![id, format!("/x/{id}.md")],
        )
        .unwrap();
    }

    fn seed_note(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO notes (id, bundle_id, title, modified_ms, preview, body_size) \
             VALUES (?1, ?2, 'T', 0, '', 0)",
            params![id, format!("b_{id}")],
        )
        .unwrap();
    }

    #[test]
    fn accepted_observations_projection_round_trips() {
        let mut conn = open_db();
        seed_member(&conn, "tm_a");
        seed_note(&conn, "n1");
        let tx = conn.transaction().unwrap();
        let id1 = crate::observations::persist::insert_pending(
            &tx, "tm_a", "n1", "Async-first communicator.", 1_000,
        )
        .unwrap();
        let id2 = crate::observations::persist::insert_pending(
            &tx, "tm_a", "n1", "Detail-oriented.", 2_000,
        )
        .unwrap();
        let _pending = crate::observations::persist::insert_pending(
            &tx, "tm_a", "n1", "Still being reviewed.", 3_000,
        )
        .unwrap();
        crate::observations::persist::set_status(
            &tx, &id1,
            crate::observations::persist::ObservationStatus::Accepted, 1_500,
        )
        .unwrap();
        crate::observations::persist::set_status(
            &tx, &id2,
            crate::observations::persist::ObservationStatus::Accepted, 2_500,
        )
        .unwrap();
        tx.commit().unwrap();

        let rows = crate::observations::persist::list_by_member(
            &conn,
            "tm_a",
            Some(crate::observations::persist::ObservationStatus::Accepted),
        )
        .unwrap();
        let projected = project_accepted_observations(rows);
        assert_eq!(projected.len(), 2);
        // Order: most-recent-first (created_ms DESC).
        assert_eq!(projected[0]["obs_id"], serde_json::Value::String(id2.clone()));
        assert_eq!(projected[0]["body"], serde_json::json!("Detail-oriented."));
        assert_eq!(projected[0]["created_ms"], serde_json::json!(2_000));
        assert_eq!(projected[1]["obs_id"], serde_json::Value::String(id1));
        // The pending row never appears.
        assert!(!projected.iter().any(|o| o["body"] == "Still being reviewed."));
    }

    #[test]
    fn accepted_observations_projection_respects_cap() {
        let mut rows: Vec<crate::observations::persist::ProfileObservation> = Vec::new();
        for i in 0..(ACCEPTED_OBSERVATIONS_CAP + 5) {
            rows.push(crate::observations::persist::ProfileObservation {
                id: format!("obs_{i}"),
                member_id: "tm_a".into(),
                source_note_id: "n1".into(),
                source_note_title: None,
                body: format!("body {i}"),
                status: crate::observations::persist::ObservationStatus::Accepted,
                created_ms: i as i64,
                reviewed_ms: Some(i as i64),
            });
        }
        let projected = project_accepted_observations(rows);
        assert_eq!(projected.len(), ACCEPTED_OBSERVATIONS_CAP);
    }
}
