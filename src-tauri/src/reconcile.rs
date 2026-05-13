use std::path::Path;

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

/// Cap on profile-body chars copied into the `## Attendees` section per
/// attendee. ~600 chars × ~5 attendees ≈ 3K chars per reconcile — small
/// next to the transcript and well within budget.
const PROFILE_EXCERPT_CHARS: usize = 600;

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
    s.push_str("**");
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

    // Read each attendee's profile.md (best-effort) and build truncated
    // excerpts. A read error degrades to an empty excerpt for that member —
    // the rest of the prompt is unaffected.
    let mut entries: Vec<(TeamMember, String)> = Vec::with_capacity(attendees.len());
    for m in attendees {
        let body = match tokio::fs::read_to_string(&m.profile_md_path).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[reconcile] read profile {} failed: {e}",
                    m.profile_md_path
                );
                String::new()
            }
        };
        let excerpt = excerpt_profile(&body, &m.display_name, PROFILE_EXCERPT_CHARS);
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
    Ok(assembled)
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
            profile_md_path: format!("/tmp/{}.md", name),
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
        assert!(got.contains("**Tom Ruesch** (You) — CEO"));
        assert!(got.contains("Aliases: TJ, Tom"));
        assert!(got.contains("Background: Leads engineering."));
        // Sarah has no role, no aliases, no profile — single-line entry.
        assert!(got.contains("**Sarah Smith**"));
        assert!(!got.contains("Sarah Smith** —"));
        assert!(!got.contains("Aliases: \n"));
        assert!(!got.contains("Background: \n"));
    }

    #[test]
    fn format_attendees_section_returns_none_when_empty() {
        let got = format_attendees_section(&[]);
        assert!(got.is_none());
    }
}
