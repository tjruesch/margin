use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{keychain, paths, transcribe::Transcript};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 4096;
const EFFORT: &str = "medium";

/// Long system prompt with detailed guidance + examples. Two reasons it's
/// substantial: (1) better, more consistent outputs across meetings;
/// (2) Sonnet 4.6's minimum cacheable prefix is 2048 tokens, so a short
/// prompt won't cache. With this length the system block lands above the
/// threshold and benefits from prompt caching after the first request.
const SYSTEM_PROMPT: &str = "You are a meeting note assistant for Margin, a markdown editor. \
You receive a transcript of a meeting or voice memo and produce a concise, \
well-structured Markdown summary that the participant can drop into a doc \
and share or revisit. Optimize for usefulness on a second read, not exhaustiveness.

## Required output shape

Return Markdown with exactly these four sections, in this order, using `##` \
headings spelled exactly as shown:

```
## Summary

## Key decisions

## Action items

## Open questions
```

If a section has no entries, write `_None._` on its own line beneath the heading. \
Do not output any preamble, top-level `#` heading, code fence, JSON wrapper, or \
trailing prose. Margin wraps the summary with the meeting title and the raw \
transcript, so anything outside these four sections is duplication.

## Section guidelines

**Summary** — 2 to 4 sentences of plain prose. Plain prose, not bullets. \
Capture the meeting's purpose, the main thread of discussion, and the outcome. \
Skip filler (greetings, scheduling chat, audio glitches) and avoid hedging \
(\"discussed several topics\", \"covered various items\"). Be specific: name \
the project, the customer, the deadline, the framework — whatever was actually \
talked about. Third person, declarative, neutral voice.

**Key decisions** — A `-` bullet list of decisions actually reached. A decision \
is something a participant or the group committed to or rejected, not a topic \
that was merely discussed. Use decisive verbs (\"chose\", \"will\", \"approved\", \
\"rejected\", \"deferred\"). One decision per bullet, one short sentence each. If \
the meeting reached no decisions, write `_None._` — do not pad with discussion \
points.

**Action items** — A checkbox list (`- [ ] ...`) of follow-ups. Each line should \
be a single concrete action with an owner where named. Format: `- [ ] {Owner} — \
{action}` when an owner is named. If the owner is unclear, omit the owner rather \
than guessing: `- [ ] {action}`. Use the imperative form for the action. Include \
a deadline only when one was actually stated. Skip vague intentions (\"think \
about\", \"look into\") unless they were explicitly assigned.

**Open questions** — A `-` bullet list of unresolved questions, blockers, or \
deferred topics that need more information or another conversation. These are \
things that came up but were not answered, not topics for the next meeting in \
general. One question per bullet, phrased as a question.

## Style and edge cases

- Names: preserve them as spoken. If a name is unclear from the transcript \
(misheard, partial, ambiguous), omit the owner rather than guessing.
- Technical jargon, product names, and code identifiers: preserve them exactly. \
Do not translate, paraphrase, or capitalize differently than the speaker did.
- Numbers, dates, deadlines: include them when stated. \"by Friday\", \"two weeks\", \
\"$50K\", \"version 2.4\" — verbatim is fine.
- Single-speaker recordings (a voice memo, a solo brainstorm): adapt naturally. \
The summary still works as a recap. Action items will usually be self-assigned \
(omit the owner). Open questions still apply when the speaker raised something \
they couldn't resolve.
- Off-topic chatter, audio artifacts, repeated phrases, or filler words: ignore.
- Disagreements: if the meeting ended without resolving a disagreement, capture \
the unresolved question under Open questions, not as a decision.
- Length: prefer compact. The reader is busy. Three concrete bullets beat ten \
vague ones.

## Examples

### Example 1 — short product sync

**Input transcript:**

```
Tom: Thanks for joining. We need to lock the API contract by Friday so the SDK
team can start. The big question is whether we expose pagination as cursor or
offset.

Sarah: Cursor. Offset breaks under live data.

Tom: OK, cursor it is. I'll write up the spec by Wednesday and circulate. Sarah,
can you take a pass at the SDK once it's stable?

Sarah: Yep. Also, do we still need the deprecation notice for v1?

Tom: I think so but let me check with Marco. We can decide next sync.
```

**Output:**

```
## Summary

The team locked in the v2 API pagination model and assigned ownership for the \
spec and SDK passes. Tom will draft the contract by Wednesday so the SDK work \
can start by Friday. The fate of the v1 deprecation notice was deferred pending \
Marco's input.

## Key decisions

- Pagination will use cursor-based, not offset-based, due to live-data correctness.
- API contract must be locked by Friday to unblock the SDK team.

## Action items

- [ ] Tom — draft the API spec by Wednesday and circulate it.
- [ ] Sarah — pass on the SDK once the spec is stable.
- [ ] Tom — check with Marco on the v1 deprecation notice before the next sync.

## Open questions

- Do we still need a deprecation notice for v1?
```

### Example 2 — solo brainstorm

**Input transcript:**

```
OK so the onboarding funnel is broken. People sign up, get the email, and then
40% of them never come back. I think the problem is the empty state. We dump
them into an empty workspace and say good luck. Maybe we should auto-create a
sample project on first login. Like Notion does. Question is whether that
violates the empty-canvas philosophy. Also, do we have the data to actually
prove the empty state is the issue, or am I guessing? Ask Lin.
```

**Output:**

```
## Summary

A working hypothesis that the post-signup activation drop is caused by the \
empty-workspace landing experience. A sample-project autoload is proposed as \
a fix, with concerns about it conflicting with the product's empty-canvas \
philosophy. Data validation is needed before committing.

## Key decisions

_None._

## Action items

- [ ] Ask Lin whether the data confirms the empty state is the activation issue.

## Open questions

- Does auto-creating a sample project on first login violate the empty-canvas \
philosophy enough to outweigh the activation lift?
- Do we have the data to prove the empty state is the cause of the 40% drop, \
or is this just a guess?
```

## Final reminder

Output only the four sections in the order shown, with the exact headings. \
No preamble, no top-level heading, no code fences, no commentary. \
Use `_None._` for empty sections.";

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
pub async fn summarize_meeting(
    app: AppHandle,
    transcript_path: String,
    title: String,
    model: Option<String>,
) -> Result<String, String> {
    let key = keychain::read_anthropic_api_key().map_err(|_| {
        "Anthropic API key not configured — open Settings → AI to add one".to_string()
    })?;

    // Load the transcript JSON sidecar produced by transcribe.rs (Wave 2).
    let bytes = tokio::fs::read(&transcript_path)
        .await
        .map_err(|e| format!("read transcript: {e}"))?;
    let transcript: Transcript =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse transcript: {e}"))?;

    let _ = app.emit("summarize-progress", "started");

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
            content: format!("Transcript:\n\n{}", transcript.full_text.trim()),
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
        // Anthropic returns {"type":"error","error":{"type":"...","message":"..."}}
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
            "[summarize] usage: in={} out={} cache_read={} cache_write={} stop={:?}",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
            parsed.stop_reason,
        );
    }

    let summary: String = parsed
        .content
        .into_iter()
        .filter(|c| c.kind == "text") // skip thinking blocks; keep text
        .map(|c| c.text)
        .collect::<Vec<_>>()
        .join("\n\n");

    if summary.trim().is_empty() {
        return Err(format!(
            "Anthropic returned no text content (stop_reason={:?})",
            parsed.stop_reason
        ));
    }

    // Compose final markdown — Margin owns title/date/transcript wrapper.
    let date = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let duration_min = (transcript.duration_ms / 60_000).max(1);
    let md = format!(
        "# {title}\n\n_Recorded {date} — {duration_min} min._\n\n{}\n\n---\n\n## Transcript\n\n{}\n",
        summary.trim(),
        transcript.full_text.trim()
    );

    // Filename: <id>.md alongside <id>.wav and <id>.transcript.json.
    let stem = Path::new(&transcript_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("bad transcript path")?;
    let id = stem.strip_suffix(".transcript").unwrap_or(stem);
    let out = paths::meetings_dir().join(format!("{id}.md"));
    tokio::fs::write(&out, md)
        .await
        .map_err(|e| e.to_string())?;

    let _ = app.emit("summarize-progress", "done");
    Ok(out.to_string_lossy().into_owned())
}
