use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{
    keychain,
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
- Bullet list. _None._ if nothing.

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
Phrased as questions.
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

- Does auto-creating a sample project violate the empty-canvas philosophy \
enough to outweigh the activation lift?
- Do we have the data to prove the empty state is the cause of the 40% drop?

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

    // Hand-notes formatting: pass through as-is. The model is told to keep
    // them verbatim under ## Notes.
    let transcript_body = format_transcript(&transcript);
    let user_message = format!(
        "Title: {}\n\n## My notes\n\n{}\n\n## Transcript\n\n{}",
        resolved_title,
        hand_notes.trim(),
        transcript_body.trim(),
    );

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
}
