use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    keychain,
    team::TeamMember,
    transcribe::{AudioSource, Transcript},
};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 8192;
const EFFORT: &str = "medium";

/// Granola-style reconciliation prompt: merge user-typed hand-notes with the
/// transcript into a single doc that has reconciled sections at the top and
/// the raw inputs preserved as appendices.
///
/// ~3K tokens — clears Sonnet 4.6's 2048-token caching minimum so the
/// system block hits cache after the first call.
const SYSTEM_PROMPT: &str = "You are reconciling a user's hand-written meeting notes with a transcript of \
the same meeting. The user's notes capture what they personally found important; \
the transcript captures everything that was said. Produce a single Markdown \
document that combines them.

## Required output structure

Use these exact `##` headings, in order, then a `---` divider, then preserve \
the raw inputs:

```
# {title}

## Summary
2-4 sentences. Plain prose.

## Key decisions
- Bullet list. Decisive verbs. _None._ if nothing decided.

## Action items
- [ ] Owner — task, when an owner is named.
- [ ] task, when no owner.
- _None._ if no actions.

## Open questions
- [?] Question, when no specific person owes the answer.
- [?] Sarah — Confirm the migration runs before the freeze.
- _None._ if nothing.

---

## Notes
{verbatim user hand-notes — preserve formatting, headings, lists}
```

The four reconciled sections at the top are *your* synthesis. The `## Notes` \
block below the divider is reference material — preserve it verbatim. The raw \
transcript is stored separately by the app; **do not** include a `## Transcript` \
section, the transcript text, or any verbatim quotes longer than a phrase.

## Reconciliation rules

- **Prioritize the user's notes** when they conflict with the transcript. The \
user's notes are the source of truth on what mattered to them; the transcript \
is broader but noisier.
- **Use the transcript to fill in details** the user didn't capture: names, \
exact decisions, action item owners, deadlines, technical specifics.
- **Preserve the user's notes verbatim** under `## Notes` — do not edit, \
reformat, or 'improve' them. They go in as-is.
- **Do NOT echo the transcript** into the document. The user has it stored \
separately and can view it on demand. Distill it into the four reconciled \
sections; never paste it.
- **If the user's notes are empty**, produce the four sections from the \
transcript alone. Skip the `## Notes` section content but keep the heading and \
write `_(no hand-notes were taken)_`.
- **If the transcript is empty or unintelligible**, lean entirely on the \
user's notes. Note the absence under `## Open questions`.
- **First H1 line**: emit a `# {title}` line at the very top. Use the title \
provided in the user message; if none, infer one from the content.

## Style guidelines

- **Summary**: 2-4 sentences of plain prose, third person, declarative. \
Capture the meeting's purpose and outcome. Skip filler.
- **Key decisions**: a bullet list of decisions actually reached (something \
committed to or rejected, not merely discussed). Decisive verbs (chose, will, \
approved, rejected, deferred). One short sentence each.
- **Action items**: checkbox bullets with owner where named. Format: \
`- [ ] {Owner} — {action}` or `- [ ] {action}` if owner unclear. Imperative form. \
Include deadlines only when stated.
- **Open questions**: questions left unanswered, blockers, deferred topics. \
Phrased as questions. Format as `- [?] {Asked-of} — {question}` or \
`- [?] {question}` when no specific person owes the answer — the `[?]` \
marker is what the app parses into the Open Questions surface.
- **Names**: preserve as written in the user's notes; check the transcript \
when the user wrote initials or partial names. If unclear, omit owner rather \
than guess.
- **Technical jargon, product names, code identifiers**: preserve exactly. Do \
not translate or paraphrase.
- **Numbers, dates, deadlines**: include verbatim when stated.
- **Length**: prefer compact. Three concrete bullets beat ten vague ones. The \
reader is busy.

## Examples

### Example 1 — user took focused notes; transcript fills in details

**User's notes:**

```
Sprint review

- Charlie burned the build twice
- Need to lock the API contract this week
- Sarah taking SDK pass
```

**Transcript excerpt:**

```
Tom: Quick sprint review. Charlie, the build pipeline was red twice this week —
what happened? Charlie: I forgot to bump the version constant. Won't happen
again. Tom: Cool. The big one: we need to lock the v2 API by Friday so the SDK
team can start. Cursor pagination, not offset. Sarah: I can take the SDK pass
once the spec is up. Tom: Wednesday for the spec.
```

**Reconciled output:**

```
# Sprint review

## Summary

The team locked in cursor-based pagination for v2 of the API and assigned the \
spec and SDK passes. Charlie acknowledged a recurring build break caused by a \
forgotten version bump. The contract must be finalized by Friday to unblock \
SDK work.

## Key decisions

- v2 API will use cursor-based pagination (not offset).
- API contract must be locked by Friday.

## Action items

- [ ] Tom — write the spec by Wednesday.
- [ ] Sarah — pass on the SDK once the spec is stable.
- [ ] Charlie — bump the version constant when changing the API.

## Open questions

_None._

---

## Notes

Sprint review

- Charlie burned the build twice
- Need to lock the API contract this week
- Sarah taking SDK pass
```

### Example 2 — empty hand-notes, transcript-only

**User's notes:** (empty)

**Transcript excerpt:** (a 5-min voice memo about onboarding ideas)

**Reconciled output:**

```
# Onboarding hypothesis brainstorm

## Summary

A solo brainstorm exploring why ~40% of post-signup users churn before \
returning. Working hypothesis: the empty workspace is too sparse to drive \
activation. Proposed fix is auto-creating a sample project on first login, \
pending data validation and product-philosophy review.

## Key decisions

_None._

## Action items

- [ ] Ask Lin whether the data confirms the empty state is the activation issue.

## Open questions

- [?] Does auto-creating a sample project violate the empty-canvas philosophy \
enough to outweigh the activation lift?
- [?] Lin — Do we have the data to prove the empty state is the cause of \
the 40% drop?

---

## Notes

_(no hand-notes were taken)_
```

## Final reminder

Produce the entire output: `# {title}` line, then four `##` reconciled \
sections, then `---`, then `## Notes` with the user's notes verbatim (or the \
empty-notes placeholder). Stop there. Never emit a `## Transcript` section \
and never paste the transcript text into the document. No preamble, no code \
fences around the whole document, no commentary.";

/// Third cached system block (#48). Defines how the model should weigh the
/// per-meeting `## Attendees` section in the user message and the
/// `[mic]` / `[system]` channel hints in the transcript. Independent cache
/// breakpoint from `SYSTEM_PROMPT` so this policy can evolve without busting
/// the larger prompt's cache.
const ATTENDEE_POLICY_PROMPT: &str = "## Attendees and channel hints

The user message may include an `## Attendees` section listing the people who attended this meeting. When attributing actions, decisions, or quoted statements:

- Use canonical display names from that list. Never invent names not in the list.
- Aliases are informal alternatives a speaker may use; prefer the canonical display name in your output.
- `(You)` next to a name marks the user reading this output. First-person statements (\"I'll handle X\") attributed to that person should still use their canonical display name in action items, not the literal word \"you\".
- When the speaker is ambiguous or the action's owner is unclear, leave the owner blank rather than guess. `- [ ] task` (no owner) is always preferable to a wrong attribution.

Channel hints `[mic]` / `[system]` are audio-capture metadata, not identity claims:

- `[mic]` is audio captured by the user's microphone. Usually carries the user's voice, but can include speaker bleed (no headphones), in-person colleagues, or shared-room audio.
- `[system]` is audio captured from system audio output. Usually carries remote participants' voices, but can include playback echo of the user's own voice.
- Treat channel as one signal alongside the attendee list and conversation content. An unambiguous content signal (\"Tom: …\") overrides the channel.
- When content is ambiguous, weight `[mic]` toward the user (if Self is among attendees) and `[system]` toward remote attendees.

Action items in your output MUST use canonical display names from the attendee list, or be left unowned (`- [ ] task`) when ownership is unclear.";

/// Side-channel policy for AI-suggested per-attendee observations (#52).
/// Carries its own ephemeral cache breakpoint so it can evolve without
/// busting the `SYSTEM_PROMPT` / glossary / attendee-policy caches.
/// The post-processor strips the marker block from the output before
/// returning the markdown to the caller — the user never sees it.
const OBSERVATIONS_POLICY_PROMPT: &str = "## Optional: per-attendee observations (side-channel)

If the meeting reveals new signal about how an attendee works (priorities, working style, communication preferences, expertise areas), append a single trailing block AFTER the markdown body, between these exact markers:

<!-- MARGIN_OBSERVATIONS_START -->
[
  { \"member_id\": \"tm_xxx\", \"body\": \"Prefers async; replies in tight bullets.\" }
]
<!-- MARGIN_OBSERVATIONS_END -->

Rules:
- `member_id` MUST match the backtick-quoted id from the `## Attendees` block. Never invent ids.
- `body` is one short sentence, declarative, third-person. No quotes, no hedging.
- At most one observation per attendee per meeting. Skip attendees with nothing new to add. Zero observations is fine — omit the entire block.
- Do NOT observe the user themselves (the `(You)` attendee).
- Only emit signal that would help a colleague reading the profile months later. Skip transient or meeting-specific notes (those belong in action items).
- The markers MUST be on their own lines. Nothing follows the end marker.";

/// Cap on profile-body chars copied into the `## Attendees` section per
/// attendee. ~600 chars × ~5 attendees ≈ 3K chars per reconcile — small
/// next to the transcript and well within budget.
const PROFILE_EXCERPT_CHARS: usize = 600;

const OBSERVATIONS_START_MARKER: &str = "<!-- MARGIN_OBSERVATIONS_START -->";
const OBSERVATIONS_END_MARKER: &str = "<!-- MARGIN_OBSERVATIONS_END -->";

/// Format the transcript for the reconcile user message. Rendering rules:
///
/// - Speaker + source present  →  `Speaker N [src]: text`
/// - Speaker only              →  `Speaker N: text`            (today's behaviour)
/// - Source only               →  `[src] text`                  (#47 channel hint)
/// - Neither, on every segment →  fall back to `full_text.trim()` (legacy)
///
/// Consecutive segments sharing the same `(speaker, source)` tuple are joined
/// into one paragraph so the model doesn't see redundant tag repetition. A
/// channel flip starts a new paragraph even when the speaker hasn't changed.
///
/// `[src]` is a channel hint, never an identity claim — see #47/#48 for the
/// policy the system prompt installs around it.
fn format_transcript(t: &Transcript) -> String {
    let any_labeled = t
        .segments
        .iter()
        .any(|s| s.speaker.is_some() || s.source.is_some());
    if !any_labeled {
        return t.full_text.trim().to_string();
    }

    let mut out = String::with_capacity(t.full_text.len() + t.segments.len() * 12);
    let mut prev_key: Option<(Option<u32>, Option<AudioSource>)> = None;
    for seg in &t.segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        let key = (seg.speaker, seg.source);
        if Some(key) == prev_key {
            out.push(' ');
            out.push_str(text);
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        if let Some(label) = format_segment_label(seg.speaker, seg.source) {
            out.push_str(&label);
            out.push_str(": ");
        }
        out.push_str(text);
        prev_key = Some(key);
    }
    out
}

fn source_tag(source: AudioSource) -> &'static str {
    match source {
        AudioSource::Mic => "mic",
        AudioSource::System => "system",
    }
}

fn format_segment_label(speaker: Option<u32>, source: Option<AudioSource>) -> Option<String> {
    match (speaker, source) {
        (Some(n), Some(src)) => Some(format!("Speaker {n} [{}]", source_tag(src))),
        (Some(n), None) => Some(format!("Speaker {n}")),
        (None, Some(src)) => Some(format!("[{}]", source_tag(src))),
        (None, None) => None,
    }
}

/// Take the first `max` chars of `body` at a whitespace boundary, after
/// stripping a leading `# {display_name}` H1 if present (the bootstrap stub
/// duplicates the name; no value to the model). Returns the trimmed result
/// with a trailing `…` only when truncation actually happened. Empty string
/// if there's nothing useful left.
fn excerpt_profile(body: &str, display_name: &str, max: usize) -> String {
    let trimmed = body.trim_start();
    let after_h1 = strip_matching_h1(trimmed, display_name);
    let cleaned = after_h1.trim();
    if cleaned.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = cleaned.chars().collect();
    if chars.len() <= max {
        return cleaned.to_string();
    }
    // Snap the cut backward to the last whitespace within `max` so we don't
    // truncate mid-word.
    let mut cut = max;
    while cut > 0 && !chars[cut - 1].is_whitespace() {
        cut -= 1;
    }
    if cut == 0 {
        // No whitespace found — take the hard cap and append the ellipsis.
        cut = max;
    }
    let head: String = chars[..cut].iter().collect::<String>().trim_end().to_string();
    format!("{head}…")
}

fn strip_matching_h1<'a>(body: &'a str, display_name: &str) -> &'a str {
    let line_end = body.find('\n').unwrap_or(body.len());
    let first_line = body[..line_end].trim();
    let expected = format!("# {display_name}");
    if first_line.eq_ignore_ascii_case(&expected) {
        // Skip the line and any blank lines that follow.
        let rest = &body[line_end..];
        rest.trim_start_matches(|c: char| c == '\n' || c == '\r' || c == ' ' || c == '\t')
    } else {
        body
    }
}

/// Render a single attendee block. `excerpt` is the (already truncated)
/// profile body — empty string when the attendee has no profile content.
fn format_attendee_entry(member: &TeamMember, excerpt: &str) -> String {
    let mut s = String::new();
    s.push_str("- **");
    s.push_str(&member.display_name);
    s.push_str("** `");
    s.push_str(&member.id);
    s.push('`');
    if member.is_self {
        s.push_str(" (You)");
    }
    let role = member.role.trim();
    if !role.is_empty() {
        s.push_str(" — ");
        s.push_str(role);
    }
    let alias_list: Vec<&str> = member
        .aliases
        .iter()
        .map(|a| a.value.trim())
        .filter(|a| !a.is_empty())
        .collect();
    if !alias_list.is_empty() {
        s.push_str("\n  Aliases: ");
        s.push_str(&alias_list.join(", "));
    }
    if !excerpt.is_empty() {
        s.push_str("\n  Background: ");
        s.push_str(excerpt);
    }
    s
}

/// Build the full `## Attendees` user-message section. Returns None when
/// `entries` is empty so the caller can omit the heading entirely.
fn format_attendees_section(entries: &[(TeamMember, String)]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from("## Attendees\n\n");
    for (i, (m, excerpt)) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format_attendee_entry(m, excerpt));
    }
    Some(out)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedObservation {
    member_id: String,
    body: String,
}

/// Split the reconcile response into the cleaned markdown body + parsed
/// observations (#52). The model is asked to append a JSON array between
/// `OBSERVATIONS_START_MARKER` and `OBSERVATIONS_END_MARKER`.
///
/// Strict tolerance:
/// - No start marker → markdown returned untouched, empty Vec.
/// - Markers present but malformed JSON between them → markdown stripped
///   anyway (we never want raw markers leaking into the saved note),
///   empty Vec.
/// - Items missing `member_id`/`body` fields are dropped silently.
/// - End marker absent → everything after the start marker is dropped,
///   no observations parsed.
fn strip_observations_block(raw: &str) -> (String, Vec<ParsedObservation>) {
    let Some(start_idx) = raw.find(OBSERVATIONS_START_MARKER) else {
        return (raw.to_string(), Vec::new());
    };
    let body = raw[..start_idx].trim_end().to_string();
    let after_start = &raw[start_idx + OBSERVATIONS_START_MARKER.len()..];
    let Some(end_off) = after_start.find(OBSERVATIONS_END_MARKER) else {
        // Start marker without end → strip from start onward, parse nothing.
        return (body, Vec::new());
    };
    let json_slice = after_start[..end_off].trim();

    let parsed: serde_json::Value = match serde_json::from_str(json_slice) {
        Ok(v) => v,
        Err(_) => return (body, Vec::new()),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return (body, Vec::new()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(member_id) = item.get("member_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(body_str) = item.get("body").and_then(|v| v.as_str()) else {
            continue;
        };
        let trimmed = body_str.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(ParsedObservation {
            member_id: member_id.to_string(),
            body: trimmed.to_string(),
        });
    }
    (body, out)
}

/// Find the `## Action items` block in `body` and split it off (#144).
/// Returns `(stripped_body, action_lines)` where `action_lines` is the
/// raw lines inside the block — the caller filters them through
/// `parse_action_line` to drop anything that isn't a real `- [ ]` item.
///
/// Block boundary: from the `## Action items` heading line (exclusive)
/// to the next `## `-prefixed line or EOF. The heading line itself is
/// removed; runs of >=3 consecutive newlines created by the cut are
/// collapsed back to a single blank line so the stripped body has no
/// awkward double gaps.
///
/// Only the first occurrence is processed. If the model ever emits two
/// `## Action items` headings the second survives in the body; the
/// caller's logging will surface that as a tail of un-persisted items.
pub(crate) fn split_action_items_block(body: &str) -> (String, Vec<String>) {
    const HEADING: &str = "## Action items";
    let lines: Vec<&str> = body.split('\n').collect();
    let start = lines.iter().position(|l| l.trim_end() == HEADING);
    let Some(start) = start else {
        return (body.to_string(), Vec::new());
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(i, l)| l.trim_start().starts_with("## ").then_some(i))
        .unwrap_or(lines.len());

    let action_lines: Vec<String> = lines[start + 1..end]
        .iter()
        .map(|l| l.to_string())
        .collect();

    let mut kept = Vec::with_capacity(lines.len() - (end - start));
    kept.extend(lines[..start].iter().copied());
    kept.extend(lines[end..].iter().copied());
    let joined = kept.join("\n");

    // Collapse 3+ consecutive newlines (a blank line on each side of
    // the cut) down to 2.
    let mut stripped = String::with_capacity(joined.len());
    let mut newlines = 0usize;
    for ch in joined.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                stripped.push(ch);
            }
        } else {
            newlines = 0;
            stripped.push(ch);
        }
    }

    (stripped, action_lines)
}

/// Extract the reconciled `## Action items` block from `body`, persist
/// each line as an `origin_kind='reconcile'` row, and return the body
/// with the block stripped (#144).
///
/// Idempotency: stable row id (`action_id(note_id, text)`) +
/// `ON CONFLICT(id) DO NOTHING` means re-running reconcile on the same
/// meeting preserves `done`, `manual_override`, and any user-assigned
/// `assignee_id` on rows whose text didn't change. New text → new row.
/// Stale rows from prior runs are intentionally left alone — sweep is
/// deferred to Phase 2 (the deletion log).
fn extract_and_persist_action_items(
    conn_state: &std::sync::Mutex<rusqlite::Connection>,
    note_id: &str,
    body: &str,
) -> Result<(String, usize), String> {
    let (stripped, raw_lines) = split_action_items_block(body);
    if raw_lines.is_empty() {
        return Ok((body.to_string(), 0));
    }

    let mut c = conn_state.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let now_ms = crate::events::current_unix_ms();

    let members = crate::team::list_team_members_raw(&tx)?;
    let resolver = crate::team::OwnerResolver::from_members(&members);
    let self_id: Option<String> = tx
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;

    // Snapshot existing reconcile-origin ids so `action_created`
    // events fire only for genuinely new rows on re-reconciles.
    let existing: HashSet<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT id FROM actions \
                  WHERE origin_kind = 'reconcile' AND origin_note_id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![note_id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut count = 0usize;
    {
        let mut stmt = tx
            .prepare_cached(
                "INSERT INTO actions \
                    (id, origin_kind, origin_note_id, origin_line, text, done, \
                     created_ms, due_ms, assignee_id) \
                 VALUES (?1, 'reconcile', ?2, NULL, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO NOTHING",
            )
            .map_err(|e| e.to_string())?;
        for raw in &raw_lines {
            let trimmed = raw.trim_start();
            let Some((text, done, due_ms)) = crate::notes::parse_action_line(trimmed) else {
                continue;
            };
            let id = crate::notes::action_id(note_id, &text);
            let assignee_id = crate::notes::extract_owner_candidate(&text)
                .and_then(|c| resolver.resolve(&c));
            stmt.execute(params![
                id,
                note_id,
                text,
                done as i64,
                now_ms,
                due_ms,
                assignee_id,
            ])
            .map_err(|e| e.to_string())?;
            if !existing.contains(&id) {
                let actor = assignee_id.as_deref().or(self_id.as_deref());
                let payload = serde_json::json!({
                    "text": text,
                    "note_id": note_id,
                });
                crate::events::emit(
                    &tx,
                    now_ms,
                    "action_created",
                    actor,
                    "action",
                    &id,
                    &payload,
                )
                .map_err(|e| e.to_string())?;
            }
            count += 1;
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok((stripped, count))
}

/// Format the user's glossary as a small system-block addendum that nudges
/// Claude to preserve domain spellings. Returns None for an empty glossary
/// so the caller can skip pushing the block.
fn format_glossary_block(glossary: &[String]) -> Option<String> {
    let terms: Vec<&str> = glossary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(format!(
        "Domain terms in this meeting (preserve exact spelling — do not auto-correct \
         or substitute similar-sounding words): {}.",
        terms.join(", ")
    ))
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    cache_control: CacheControl,
}

#[derive(Serialize)]
struct ReqMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: &'static str,
}

#[derive(Serialize)]
struct ReqBody<'a> {
    model: &'a str,
    max_tokens: u32,
    thinking: ThinkingConfig,
    output_config: OutputConfig,
    system: Vec<SystemBlock<'a>>,
    messages: Vec<ReqMessage<'a>>,
}

#[derive(Deserialize)]
struct RespContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Debug)]
struct RespUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
}

#[derive(Deserialize)]
struct RespBody {
    content: Vec<RespContent>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<RespUsage>,
}

#[derive(Deserialize)]
struct ApiErrorInner {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: ApiErrorInner,
}

#[tauri::command]
pub async fn reconcile_notes(
    app: AppHandle,
    hand_notes: String,
    transcript_path: String,
    title: String,
    model: Option<String>,
    glossary: Vec<String>,
) -> Result<String, String> {
    let key = keychain::read_anthropic_api_key().map_err(|_| {
        "Anthropic API key not configured — open Settings → AI to add one".to_string()
    })?;

    // Load the transcript JSON sidecar produced by transcribe.rs.
    let _ = Path::new(&transcript_path); // path validity check is done by file read
    let bytes = tokio::fs::read(&transcript_path)
        .await
        .map_err(|e| format!("read transcript: {e}"))?;
    let mut transcript: Transcript =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse transcript: {e}"))?;

    let _ = app.emit("reconcile-progress", "started");

    let resolved_title = if title.trim().is_empty() {
        "Untitled note".to_string()
    } else {
        title.trim().to_string()
    };

    // Fetch the attendee list for this meeting (#48). Derive the note path
    // from the transcript path — they're siblings under the bundle dir.
    let note_path: Option<String> = Path::new(&transcript_path)
        .parent()
        .map(|p| p.join(crate::notes::NOTE_FILENAME).to_string_lossy().into_owned());

    let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
    let attendees: Vec<TeamMember> = if let Some(np) = note_path.as_deref() {
        match conn_state.lock() {
            Ok(c) => crate::team::list_meeting_attendees(&c, np).unwrap_or_else(|e| {
                eprintln!("[reconcile] list_meeting_attendees failed: {e}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // Pull the latest profile snapshot per attendee (#107) and flatten
    // each into the multi-line excerpt the prompt has shipped for
    // months. Missing snapshots (worker hasn't run yet) degrade to an
    // empty excerpt for that member — the rest of the prompt is
    // unaffected. The legacy `profile.md` disk reads went away in
    // #107; the column on team_members survives but unread.
    let snapshot_map = if attendees.is_empty() {
        std::collections::HashMap::new()
    } else {
        match conn_state.lock() {
            Ok(c) => {
                let ids: Vec<&str> = attendees.iter().map(|m| m.id.as_str()).collect();
                crate::profiles::persist::get_latest_map(&c, &ids).unwrap_or_else(|e| {
                    eprintln!("[reconcile] get_latest_map failed: {e}");
                    std::collections::HashMap::new()
                })
            }
            Err(_) => std::collections::HashMap::new(),
        }
    };
    let mut entries: Vec<(TeamMember, String)> = Vec::with_capacity(attendees.len());
    for m in attendees {
        let excerpt = snapshot_map
            .get(&m.id)
            .map(|snap| {
                crate::profiles::prompt::render_snapshot_excerpt(
                    &snap.body,
                    PROFILE_EXCERPT_CHARS,
                )
            })
            .unwrap_or_default();
        entries.push((m, excerpt));
    }
    let attendees_section = format_attendees_section(&entries);

    // Hand-notes formatting: pass through as-is. The model is told to keep
    // them verbatim under ## Notes.
    let transcript_body = format_transcript(&transcript);
    let mut user_message = format!("Title: {}", resolved_title);
    if let Some(section) = attendees_section.as_deref() {
        user_message.push_str("\n\n");
        user_message.push_str(section);
    }
    user_message.push_str("\n\n## My notes\n\n");
    user_message.push_str(hand_notes.trim());
    user_message.push_str("\n\n## Transcript\n\n");
    user_message.push_str(transcript_body.trim());

    let model = model.as_deref().unwrap_or(DEFAULT_MODEL);

    // Glossary is appended as a second system block AFTER the cached
    // SYSTEM_PROMPT prefix. That keeps the (much larger) SYSTEM_PROMPT
    // breakpoint stable across glossary edits — and the glossary itself
    // gets its own breakpoint, so when the user doesn't change it, the
    // suffix hits cache too.
    let glossary_text = format_glossary_block(&glossary);
    let mut system = vec![SystemBlock {
        kind: "text",
        text: SYSTEM_PROMPT,
        cache_control: CacheControl { kind: "ephemeral" },
    }];
    if let Some(text) = glossary_text.as_deref() {
        system.push(SystemBlock {
            kind: "text",
            text,
            cache_control: CacheControl { kind: "ephemeral" },
        });
    }
    // Attendee-attribution + channel-hint policy (#48). Installed on every
    // call regardless of whether attendees are attached — the channel-hint
    // half is still relevant when the transcript carries [mic]/[system]
    // tags from #47.
    system.push(SystemBlock {
        kind: "text",
        text: ATTENDEE_POLICY_PROMPT,
        cache_control: CacheControl { kind: "ephemeral" },
    });
    // Side-channel observations policy (#52). Only installed when the
    // call has attendees attached — without an `## Attendees` block the
    // model has no valid ids to reference.
    if !entries.is_empty() {
        system.push(SystemBlock {
            kind: "text",
            text: OBSERVATIONS_POLICY_PROMPT,
            cache_control: CacheControl { kind: "ephemeral" },
        });
    }

    let body = ReqBody {
        model,
        max_tokens: MAX_TOKENS,
        thinking: ThinkingConfig { kind: "adaptive" },
        output_config: OutputConfig { effort: EFFORT },
        system,
        messages: vec![ReqMessage {
            role: "user",
            content: user_message,
        }],
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(ENDPOINT)
        .header("x-api-key", &key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let raw = resp.text().await.unwrap_or_default();
        let parsed: Option<ApiErrorEnvelope> = serde_json::from_str(&raw).ok();
        let detail = parsed
            .as_ref()
            .map(|e| format!("{}: {}", e.error.kind, e.error.message))
            .unwrap_or_else(|| raw.clone());
        let msg = match status.as_u16() {
            401 => format!("Invalid Anthropic API key — check Settings → AI ({detail})"),
            429 => format!("Rate limited by Anthropic — try again shortly ({detail})"),
            400 => format!("Anthropic rejected the request: {detail}"),
            _ => format!("Anthropic returned {status}: {detail}"),
        };
        return Err(msg);
    }

    let parsed: RespBody = resp.json().await.map_err(|e| e.to_string())?;

    if let Some(u) = &parsed.usage {
        eprintln!(
            "[reconcile] usage: in={} out={} cache_read={} cache_write={} stop={:?}",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
            parsed.stop_reason,
        );
    }

    let assembled: String = parsed
        .content
        .into_iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text)
        .collect::<Vec<_>>()
        .join("\n\n");

    if assembled.trim().is_empty() {
        return Err(format!(
            "Anthropic returned no text content (stop_reason={:?})",
            parsed.stop_reason
        ));
    }

    // Split off the side-channel observations block (#52). The returned
    // markdown is what the user sees; observations are persisted as
    // `pending` rows the user reviews from the Team detail page.
    let (markdown_body, observations) = strip_observations_block(&assembled);
    if !observations.is_empty() {
        let attendee_ids: std::collections::HashSet<&str> =
            entries.iter().map(|(m, _)| m.id.as_str()).collect();
        let note_id_for_obs = note_path.as_deref();
        match (note_id_for_obs, conn_state.lock()) {
            (Some(nid), Ok(mut c)) => {
                let now_obs_ms = crate::events::current_unix_ms();
                match c.transaction() {
                    Ok(tx) => {
                        let mut inserted = 0usize;
                        for obs in &observations {
                            if !attendee_ids.contains(obs.member_id.as_str()) {
                                continue;
                            }
                            match crate::observations::persist::insert_pending(
                                &tx,
                                &obs.member_id,
                                nid,
                                &obs.body,
                                now_obs_ms,
                            ) {
                                Ok(_) => inserted += 1,
                                Err(e) => eprintln!("[reconcile] insert observation: {e}"),
                            }
                        }
                        if let Err(e) = tx.commit() {
                            eprintln!("[reconcile] commit observations: {e}");
                        } else if inserted > 0 {
                            eprintln!("[reconcile] inserted {inserted} pending observations");
                        }
                    }
                    Err(e) => eprintln!("[reconcile] begin tx for observations: {e}"),
                }
            }
            (None, _) => {
                eprintln!("[reconcile] skipping observations — no note_path available");
            }
            (_, Err(e)) => eprintln!("[reconcile] lock conn for observations: {e}"),
        }
    }

    // Move LLM-emitted `## Action items` from the body into action rows
    // with origin_kind='reconcile' (#144). On any failure, fall through
    // with the original body so the LLM round isn't lost — the existing
    // note-origin parse path will absorb the items on save (pre-#144
    // behaviour). Requires a note_path because origin_note_id is the FK.
    let final_body: String = if let Some(np) = note_path.as_deref() {
        match extract_and_persist_action_items(&conn_state, np, &markdown_body) {
            Ok((stripped, n)) => {
                if n > 0 {
                    eprintln!("[reconcile] persisted {n} reconcile-origin actions");
                }
                stripped
            }
            Err(e) => {
                eprintln!("[reconcile] action persist failed, leaving block in body: {e}");
                markdown_body.clone()
            }
        }
    } else {
        markdown_body.clone()
    };

    // Stamp the transcript so the post-record banner can suppress its
    // Generate-notes CTA next time the note is opened. Failure to write
    // is non-fatal — the user got their reconciled output, the flag is
    // just a UI affordance.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    transcript.reconciled_at = Some(now_ms);
    if let Ok(json) = serde_json::to_vec_pretty(&transcript) {
        if let Err(e) = tokio::fs::write(&transcript_path, json).await {
            eprintln!("[reconcile] could not stamp transcript.json: {e}");
        }
    }

    let _ = app.emit("reconcile-progress", "done");
    Ok(final_body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::{AudioSource, Segment, Transcript};

    fn seg(text: &str, speaker: Option<u32>, source: Option<AudioSource>) -> Segment {
        Segment {
            start_ms: 0,
            end_ms: 1000,
            text: text.into(),
            speaker,
            source,
        }
    }

    fn tx(segments: Vec<Segment>, full_text: &str) -> Transcript {
        Transcript {
            segments,
            full_text: full_text.into(),
            language: "en".into(),
            duration_ms: 0,
            num_speakers: None,
            reconciled_at: None,
            had_errors: false,
        }
    }

    #[test]
    fn format_transcript_renders_source_only_when_no_diarization() {
        let t = tx(
            vec![
                seg("hello there", None, Some(AudioSource::Mic)),
                seg("right back", None, Some(AudioSource::Mic)),
                seg("understood", None, Some(AudioSource::System)),
                seg("got it", None, Some(AudioSource::System)),
            ],
            "ignored — segments take precedence",
        );
        let got = format_transcript(&t);
        assert_eq!(
            got,
            "[mic]: hello there right back\n\n[system]: understood got it"
        );
    }

    #[test]
    fn format_transcript_layers_source_after_speaker() {
        let t = tx(
            vec![
                seg("good morning", Some(1), Some(AudioSource::Mic)),
                seg("how are you", Some(1), Some(AudioSource::Mic)),
                // Same speaker, channel flipped — must start a new paragraph.
                seg("playback running", Some(1), Some(AudioSource::System)),
                // Different speaker, mic again.
                seg("hi", Some(2), Some(AudioSource::Mic)),
            ],
            "",
        );
        let got = format_transcript(&t);
        assert_eq!(
            got,
            "Speaker 1 [mic]: good morning how are you\n\nSpeaker 1 [system]: playback running\n\nSpeaker 2 [mic]: hi"
        );
    }

    #[test]
    fn format_transcript_falls_back_to_full_text_when_neither_is_set() {
        let t = tx(
            vec![
                seg("first part", None, None),
                seg("second part", None, None),
            ],
            "first part second part",
        );
        let got = format_transcript(&t);
        assert_eq!(got, "first part second part");
    }

    fn member(name: &str, role: &str, aliases: &[(&str, &str)], is_self: bool) -> TeamMember {
        TeamMember {
            id: format!("{}-id", name.to_ascii_lowercase()),
            display_name: name.into(),
            role: role.into(),
            aliases: aliases
                .iter()
                .map(|(k, v)| crate::team::TypedAlias {
                    kind: (*k).to_string(),
                    value: (*v).to_string(),
                })
                .collect(),
            is_self,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn excerpt_profile_strips_matching_h1() {
        let body = "# Tom Ruesch\n\nLeads engineering and product.";
        let got = excerpt_profile(body, "Tom Ruesch", 600);
        assert_eq!(got, "Leads engineering and product.");
    }

    #[test]
    fn excerpt_profile_keeps_non_matching_h1() {
        let body = "# Other heading\n\nbody text";
        let got = excerpt_profile(body, "Tom Ruesch", 600);
        assert_eq!(got, "# Other heading\n\nbody text");
    }

    #[test]
    fn excerpt_profile_truncates_at_word_boundary_with_ellipsis() {
        // Build a body well over the cap with simple word boundaries.
        let mut body = String::new();
        for _ in 0..200 {
            body.push_str("alpha beta gamma ");
        }
        let max = 50;
        let got = excerpt_profile(&body, "Anyone", max);
        assert!(got.ends_with('…'), "missing ellipsis: {got:?}");
        // With max=50, the excerpt is ≤ 51 chars (50 + the ellipsis).
        let n = got.chars().count();
        assert!(n <= max + 1, "too long: {n} chars");
        // Last char before the ellipsis is alphabetic — no mid-word cut.
        let last = got
            .chars()
            .rev()
            .nth(1)
            .expect("at least 2 chars in result");
        assert!(last.is_ascii_alphabetic(), "mid-word cut: {got:?}");
    }

    #[test]
    fn excerpt_profile_returns_empty_when_only_h1() {
        let body = "# Tom Ruesch\n";
        let got = excerpt_profile(body, "Tom Ruesch", 600);
        assert_eq!(got, "");
    }

    #[test]
    fn format_attendees_section_marks_self_and_omits_empty_fields() {
        let entries = vec![
            (
                member("Tom Ruesch", "CEO", &[("name", "TJ"), ("name", "Tom")], true),
                "Leads engineering.".to_string(),
            ),
            (member("Sarah Smith", "", &[], false), String::new()),
        ];
        let got = format_attendees_section(&entries).expect("non-empty");
        assert!(got.starts_with("## Attendees\n\n"));
        // The canonical id is now embedded after the display name (#52)
        // so the observations side-channel can reference it.
        assert!(got.contains("**Tom Ruesch** `tom ruesch-id` (You) — CEO"));
        assert!(got.contains("Aliases: TJ, Tom"));
        assert!(got.contains("Background: Leads engineering."));
        assert!(got.contains("**Sarah Smith** `sarah smith-id`"));
        assert!(!got.contains("Aliases: \n"));
        assert!(!got.contains("Background: \n"));
    }

    #[test]
    fn format_attendees_section_returns_none_when_empty() {
        let got = format_attendees_section(&[]);
        assert!(got.is_none());
    }

    // ---------- strip_observations_block (#52) -------------------------

    #[test]
    fn strip_observations_block_returns_raw_when_no_markers() {
        let raw = "# Note\n\n## Summary\n\nA brief meeting.";
        let (body, obs) = strip_observations_block(raw);
        assert_eq!(body, raw);
        assert!(obs.is_empty());
    }

    #[test]
    fn strip_observations_block_extracts_valid_payload() {
        let raw = "# Note\n\n## Summary\n\nWe met.\n\n<!-- MARGIN_OBSERVATIONS_START -->\n[\n  {\"member_id\": \"tm_a\", \"body\": \"Async-first.\"},\n  {\"member_id\": \"tm_b\", \"body\": \"Detail-oriented.\"}\n]\n<!-- MARGIN_OBSERVATIONS_END -->\n";
        let (body, obs) = strip_observations_block(raw);
        assert_eq!(body, "# Note\n\n## Summary\n\nWe met.");
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].member_id, "tm_a");
        assert_eq!(obs[0].body, "Async-first.");
        assert_eq!(obs[1].member_id, "tm_b");
        assert_eq!(obs[1].body, "Detail-oriented.");
    }

    #[test]
    fn strip_observations_block_drops_malformed_json_but_still_strips() {
        let raw = "# Note\n\nBody.\n\n<!-- MARGIN_OBSERVATIONS_START -->\nthis isn't json\n<!-- MARGIN_OBSERVATIONS_END -->";
        let (body, obs) = strip_observations_block(raw);
        // Markers stripped, body preserved.
        assert_eq!(body, "# Note\n\nBody.");
        assert!(obs.is_empty());
    }

    #[test]
    fn strip_observations_block_drops_items_missing_fields() {
        let raw = "# Note\n\nBody.\n\n<!-- MARGIN_OBSERVATIONS_START -->\n[\n  {\"member_id\": \"tm_a\"},\n  {\"body\": \"orphaned\"},\n  {\"member_id\": \"tm_b\", \"body\": \"   \"},\n  {\"member_id\": \"tm_c\", \"body\": \"keep me\"}\n]\n<!-- MARGIN_OBSERVATIONS_END -->";
        let (body, obs) = strip_observations_block(raw);
        assert_eq!(body, "# Note\n\nBody.");
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].member_id, "tm_c");
        assert_eq!(obs[0].body, "keep me");
    }

    #[test]
    fn strip_observations_block_handles_start_without_end_marker() {
        let raw = "# Note\n\nBody.\n\n<!-- MARGIN_OBSERVATIONS_START -->\n[ {\"member_id\":\"tm_a\",\"body\":\"x\"} ]\n";
        let (body, obs) = strip_observations_block(raw);
        // Everything from the start marker is dropped — never leak markers.
        assert_eq!(body, "# Note\n\nBody.");
        assert!(obs.is_empty());
    }

    #[test]
    fn strip_observations_block_trims_trailing_whitespace_before_marker() {
        let raw = "# Note\n\nBody.\n\n\n<!-- MARGIN_OBSERVATIONS_START -->\n[]\n<!-- MARGIN_OBSERVATIONS_END -->\n";
        let (body, obs) = strip_observations_block(raw);
        assert_eq!(body, "# Note\n\nBody.");
        assert!(obs.is_empty());
    }

    // ---------- split_action_items_block (#144) ------------------------

    #[test]
    fn split_action_items_block_extracts_and_strips() {
        let body = "# Title\n\n## Summary\n\nProse.\n\n## Action items\n\n- [ ] do thing 1\n- [x] do thing 2\n- [ ] Heike — Send Q3 budget\n\n## Open questions\n\n- [?] who owns deploy?\n";
        let (stripped, lines) = split_action_items_block(body);
        assert_eq!(lines.len(), 5, "5 raw lines in block (3 actions + 2 blanks): {lines:?}");
        // Stripped body has summary + open questions, no action items heading.
        assert!(!stripped.contains("## Action items"));
        assert!(stripped.contains("## Summary\n\nProse."));
        assert!(stripped.contains("## Open questions\n\n- [?] who owns deploy?"));
        // No triple-newline gaps left behind.
        assert!(!stripped.contains("\n\n\n"));
    }

    #[test]
    fn split_action_items_block_handles_eof_block() {
        let body = "# Title\n\n## Summary\n\nProse.\n\n## Action items\n\n- [ ] last task\n";
        let (stripped, lines) = split_action_items_block(body);
        assert!(lines.iter().any(|l| l.contains("last task")));
        assert!(!stripped.contains("## Action items"));
        assert!(!stripped.contains("last task"));
        assert!(stripped.contains("## Summary\n\nProse."));
    }

    #[test]
    fn split_action_items_block_handles_missing_section() {
        let body = "# Title\n\n## Summary\n\nProse only — no action items heading.\n";
        let (stripped, lines) = split_action_items_block(body);
        assert!(lines.is_empty());
        assert_eq!(stripped, body, "no heading → body unchanged byte-for-byte");
    }

    #[test]
    fn split_action_items_block_drops_non_checkbox_lines_in_block() {
        let body = "# Title\n\n## Action items\n\nstray prose the model shouldn't emit\n- [ ] valid task\nmore prose\n\n## Next\n";
        let (stripped, lines) = split_action_items_block(body);
        // Captured: prose + checkbox + prose + blank (caller filters).
        let parsed: Vec<_> = lines
            .iter()
            .filter_map(|l| crate::notes::parse_action_line(l.trim_start()))
            .collect();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "valid task");
        assert!(!stripped.contains("## Action items"));
        assert!(!stripped.contains("stray prose"));
        assert!(stripped.contains("## Next"));
    }

    #[test]
    fn split_action_items_block_only_strips_first_heading() {
        let body = "# Title\n\n## Action items\n\n- [ ] one\n\n## Other\n\nstuff\n\n## Action items\n\n- [ ] two\n";
        let (stripped, lines) = split_action_items_block(body);
        // First block captured.
        assert!(lines.iter().any(|l| l.contains("one")));
        assert!(!lines.iter().any(|l| l.contains("two")));
        // Second heading and its line survive in the stripped body.
        assert!(stripped.contains("## Action items"));
        assert!(stripped.contains("- [ ] two"));
        assert!(!stripped.contains("- [ ] one"));
    }

    // ---------- extract_and_persist_action_items (#144) ----------------

    fn db_with_note(note_id: &str) -> std::sync::Mutex<rusqlite::Connection> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms, body_size) \
             VALUES (?1, ?1, 'T', 100, 0)",
            rusqlite::params![note_id],
        )
        .unwrap();
        std::sync::Mutex::new(conn)
    }

    fn seed_team_member(
        mutex: &std::sync::Mutex<rusqlite::Connection>,
        id: &str,
        display_name: &str,
        is_self: bool,
    ) {
        let c = mutex.lock().unwrap();
        c.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES (?1, ?2, '', ?3, 100, 100)",
            rusqlite::params![id, display_name, is_self as i64],
        )
        .unwrap();
    }

    #[test]
    fn persist_creates_reconcile_origin_rows_with_correct_shape() {
        let note_id = "/n/note.md";
        let mutex = db_with_note(note_id);
        let body = "# T\n\n## Action items\n\n- [ ] one\n- [x] two\n\n## End\n";

        let (stripped, n) = extract_and_persist_action_items(&mutex, note_id, body).unwrap();
        assert_eq!(n, 2);
        assert!(!stripped.contains("## Action items"));

        let c = mutex.lock().unwrap();
        let (kind, oni, line, done): (String, String, Option<i64>, i64) = c
            .query_row(
                "SELECT origin_kind, origin_note_id, origin_line, done \
                 FROM actions WHERE text = 'one'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(kind, "reconcile");
        assert_eq!(oni, note_id);
        assert!(line.is_none(), "origin_line must be NULL");
        assert_eq!(done, 0);

        let done_two: i64 = c
            .query_row(
                "SELECT done FROM actions WHERE text = 'two'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(done_two, 1);
    }

    #[test]
    fn persist_is_idempotent_on_rerun_preserving_done() {
        let note_id = "/n/note.md";
        let mutex = db_with_note(note_id);
        let body = "## Action items\n\n- [ ] keep me\n";

        let (_, n1) = extract_and_persist_action_items(&mutex, note_id, body).unwrap();
        assert_eq!(n1, 1);

        // User completes the action between runs.
        {
            let c = mutex.lock().unwrap();
            c.execute(
                "UPDATE actions SET done = 1, manual_override = 1 WHERE text = 'keep me'",
                [],
            )
            .unwrap();
        }

        // Re-reconcile emits the same line.
        let (_, n2) = extract_and_persist_action_items(&mutex, note_id, body).unwrap();
        assert_eq!(n2, 1, "second run still attempts the upsert");

        let c = mutex.lock().unwrap();
        let (done, mo, row_count): (i64, i64, i64) = c
            .query_row(
                "SELECT done, manual_override, \
                        (SELECT COUNT(*) FROM actions WHERE text = 'keep me') \
                 FROM actions WHERE text = 'keep me'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(done, 1, "ON CONFLICT DO NOTHING preserves done=1");
        assert_eq!(mo, 1, "manual_override preserved");
        assert_eq!(row_count, 1, "no duplicate row");
    }

    #[test]
    fn persist_resolves_owner_to_assignee() {
        let note_id = "/n/note.md";
        let mutex = db_with_note(note_id);
        seed_team_member(&mutex, "tm_heike", "Heike", false);
        let body = "## Action items\n\n- [ ] Heike — Send Q3 budget\n- [ ] anonymous task\n";

        let (_, _) = extract_and_persist_action_items(&mutex, note_id, body).unwrap();

        let c = mutex.lock().unwrap();
        let assignee: Option<String> = c
            .query_row(
                "SELECT assignee_id FROM actions WHERE text LIKE 'Heike%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(assignee.as_deref(), Some("tm_heike"));

        let anon: Option<String> = c
            .query_row(
                "SELECT assignee_id FROM actions WHERE text = 'anonymous task'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(anon.is_none());
    }

    #[test]
    fn persist_emits_action_created_once_per_new_id() {
        let note_id = "/n/note.md";
        let mutex = db_with_note(note_id);
        let body = "## Action items\n\n- [ ] one\n- [ ] two\n";

        extract_and_persist_action_items(&mutex, note_id, body).unwrap();
        let first_count: i64 = mutex
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_created'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first_count, 2);

        // Rerun with the same body — no new events.
        extract_and_persist_action_items(&mutex, note_id, body).unwrap();
        let second_count: i64 = mutex
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_created'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(second_count, 2, "no new events on idempotent re-run");

        // Add a new item — one new event.
        let body2 = "## Action items\n\n- [ ] one\n- [ ] two\n- [ ] three\n";
        extract_and_persist_action_items(&mutex, note_id, body2).unwrap();
        let third_count: i64 = mutex
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_created'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(third_count, 3);
    }

    #[test]
    fn persist_returns_zero_when_no_block() {
        let note_id = "/n/note.md";
        let mutex = db_with_note(note_id);
        let body = "# T\n\n## Summary\n\nNo action items section here.\n";

        let (stripped, n) = extract_and_persist_action_items(&mutex, note_id, body).unwrap();
        assert_eq!(n, 0);
        assert_eq!(stripped, body);

        let c = mutex.lock().unwrap();
        let rows: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM actions WHERE origin_kind = 'reconcile'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        let events: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_created'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
    }
}
