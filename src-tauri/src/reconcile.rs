use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{keychain, transcribe::Transcript};

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

---

## Transcript
{full transcript text}
```

The four reconciled sections at the top are *your* synthesis. Everything below \
the first `---` is reference material — preserve it verbatim.

## Reconciliation rules

- **Prioritize the user's notes** when they conflict with the transcript. The \
user's notes are the source of truth on what mattered to them; the transcript \
is broader but noisier.
- **Use the transcript to fill in details** the user didn't capture: names, \
exact decisions, action item owners, deadlines, technical specifics.
- **Preserve the user's notes verbatim** under `## Notes` — do not edit, \
reformat, or 'improve' them. They go in as-is.
- **Preserve the transcript verbatim** under `## Transcript`.
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

---

## Transcript

{transcript verbatim}
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

---

## Transcript

{transcript verbatim}
```

## Final reminder

Produce the entire output: `# {title}` line, then four `##` reconciled \
sections, then `---`, then `## Notes` with the user's notes verbatim (or the \
empty-notes placeholder), then `---`, then `## Transcript` with the transcript \
verbatim. No preamble, no code fences around the whole document, no commentary.";

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
) -> Result<String, String> {
    let key = keychain::read_anthropic_api_key().map_err(|_| {
        "Anthropic API key not configured — open Settings → AI to add one".to_string()
    })?;

    // Load the transcript JSON sidecar produced by transcribe.rs.
    let _ = Path::new(&transcript_path); // path validity check is done by file read
    let bytes = tokio::fs::read(&transcript_path)
        .await
        .map_err(|e| format!("read transcript: {e}"))?;
    let transcript: Transcript =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse transcript: {e}"))?;

    let _ = app.emit("reconcile-progress", "started");

    let resolved_title = if title.trim().is_empty() {
        "Untitled note".to_string()
    } else {
        title.trim().to_string()
    };

    // Hand-notes formatting: pass through as-is. The model is told to keep
    // them verbatim under ## Notes.
    let user_message = format!(
        "Title: {}\n\n## My notes\n\n{}\n\n## Transcript\n\n{}",
        resolved_title,
        hand_notes.trim(),
        transcript.full_text.trim(),
    );

    let model = model.as_deref().unwrap_or(DEFAULT_MODEL);
    let body = ReqBody {
        model,
        max_tokens: MAX_TOKENS,
        thinking: ThinkingConfig { kind: "adaptive" },
        output_config: OutputConfig { effort: EFFORT },
        system: vec![SystemBlock {
            kind: "text",
            text: SYSTEM_PROMPT,
            cache_control: CacheControl { kind: "ephemeral" },
        }],
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

    let _ = app.emit("reconcile-progress", "done");
    Ok(assembled)
}
