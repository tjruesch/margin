//! AI-driven categorization for workstream link URLs.
//!
//! Given a URL the user pasted, we ask Claude Haiku for a short label
//! (~3 words) and a `link_kinds::*` value. The single-shot prompt is
//! deliberately tiny so cost + latency stay under ~$0.001 + ~1s per
//! call.
//!
//! Failure semantics: any error (no API key, network, malformed JSON,
//! invalid kind) falls back to `{label: <hostname>, kind: "other"}` so
//! the user still gets a usable chip. The error is logged but the
//! `add` flow still completes — the user can edit the label / kind
//! later via the inline composer if they reopen it.
//!
//! No DB access. The caller wires this into `add_workstream_link`
//! after the categorization round-trip.
//!
//! Model: claude-haiku-4-5 (see `crate::anthropic::HAIKU_MODEL`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::link_kinds;
use crate::anthropic::{ANTHROPIC_VERSION, ENDPOINT, HAIKU_MODEL};

/// Cap on the AI-returned label. The chip ellipsises on overflow but
/// we trim here too so the DB doesn't carry stray model verbosity.
const LABEL_MAX_CHARS: usize = 40;

const SYSTEM_PROMPT: &str = "You categorize URLs into one of five kinds and pick a short, human \
label for each.

Kinds:
- github: any github.com URL
- linear: linear.app URLs
- notion: notion.so or notion.site URLs
- figma: figma.com URLs
- other: everything else

Label rules:
- 1-4 words, ideally a recognizable name from the URL itself
  (\"Margin repo\", \"Q3 sourcing doc\", \"Bridge wireframes\")
- prefer proper nouns over generic descriptors
- never include the domain or scheme
- max 40 characters

Output strict JSON, no prose, no markdown fences:
{\"label\": \"...\", \"kind\": \"github\"|\"linear\"|\"notion\"|\"figma\"|\"other\"}";

/// Result of a successful categorization. The caller persists this
/// directly via `persist::add_workstream_link`.
#[derive(Debug, Clone)]
pub struct Categorized {
    pub label: String,
    pub kind: String,
}

/// Call Haiku to categorize the URL. On any error, returns the
/// hostname-based fallback. Caller does not need to handle the
/// failure case — the return type is always a usable `Categorized`.
pub async fn categorize_or_fallback(url: &str) -> Categorized {
    let url = url.trim();
    let api_key = match crate::keychain::read_anthropic_api_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[link-categorizer] no API key, using hostname fallback: {e}");
            return fallback(url);
        }
    };
    match call_haiku(&api_key, url).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[link-categorizer] {e}, using hostname fallback for {url}");
            fallback(url)
        }
    }
}

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

async fn call_haiku(api_key: &str, url: &str) -> Result<Categorized, String> {
    let body = ApiRequest {
        model: HAIKU_MODEL,
        max_tokens: 128,
        stream: false,
        system: SYSTEM_PROMPT,
        messages: vec![ApiMessage {
            role: "user",
            content: url,
        }],
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
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
        return Err(format!("anthropic returned {status}: {raw}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("anthropic response parse: {e}"))?;
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
        return Err("empty response text".into());
    }
    parse_categorized(&text)
}

#[derive(Deserialize)]
struct RawCategorized {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// Parse the model's JSON output into a validated `Categorized`.
/// Tolerates optional ```json fences. Rejects empty labels and kinds
/// outside the canonical set; the caller falls back to hostname/other
/// in either case.
pub fn parse_categorized(raw: &str) -> Result<Categorized, String> {
    let stripped = strip_json_fences(raw);
    let parsed: RawCategorized =
        serde_json::from_str(&stripped).map_err(|e| format!("parse: {e}"))?;
    let label = parsed
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing label".to_string())?
        .chars()
        .take(LABEL_MAX_CHARS)
        .collect::<String>();
    let kind = parsed
        .kind
        .as_deref()
        .map(str::trim)
        .map(str::to_lowercase)
        .ok_or_else(|| "missing kind".to_string())?;
    if !is_canonical_kind(&kind) {
        return Err(format!("unknown kind: {kind}"));
    }
    Ok(Categorized { label, kind })
}

fn is_canonical_kind(k: &str) -> bool {
    matches!(
        k,
        link_kinds::GITHUB
            | link_kinds::LINEAR
            | link_kinds::NOTION
            | link_kinds::FIGMA
            | link_kinds::OTHER
    )
}

fn strip_json_fences(s: &str) -> String {
    let trimmed = s.trim();
    let without_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_start();
    without_open
        .strip_suffix("```")
        .unwrap_or(without_open)
        .trim()
        .to_string()
}

/// Last-resort categorization when the API call fails. Uses the URL's
/// hostname (minus a leading "www.") as the label and `other` as the
/// kind. The user can edit either via the inline composer.
pub fn fallback(url: &str) -> Categorized {
    let label = hostname_label(url).unwrap_or_else(|| "Link".to_string());
    Categorized {
        label,
        kind: link_kinds::OTHER.to_string(),
    }
}

fn hostname_label(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next()?.split(':').next()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(host.chars().take(LABEL_MAX_CHARS).collect())
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_categorized_accepts_canonical_response() {
        let raw = r#"{"label": "Margin repo", "kind": "github"}"#;
        let c = parse_categorized(raw).unwrap();
        assert_eq!(c.label, "Margin repo");
        assert_eq!(c.kind, "github");
    }

    #[test]
    fn parse_categorized_strips_json_fences() {
        let raw = "```json\n{\"label\":\"X\",\"kind\":\"linear\"}\n```";
        let c = parse_categorized(raw).unwrap();
        assert_eq!(c.label, "X");
        assert_eq!(c.kind, "linear");
    }

    #[test]
    fn parse_categorized_lowercases_and_validates_kind() {
        let raw = r#"{"label": "Doc", "kind": "NOTION"}"#;
        let c = parse_categorized(raw).unwrap();
        assert_eq!(c.kind, "notion");
    }

    #[test]
    fn parse_categorized_rejects_unknown_kind() {
        let raw = r#"{"label": "Doc", "kind": "slack"}"#;
        assert!(parse_categorized(raw).is_err());
    }

    #[test]
    fn parse_categorized_rejects_empty_label() {
        let raw = r#"{"label": "", "kind": "github"}"#;
        assert!(parse_categorized(raw).is_err());
    }

    #[test]
    fn parse_categorized_truncates_overlong_label() {
        let raw = r#"{"label": "This is a very very very very very long label name", "kind": "other"}"#;
        let c = parse_categorized(raw).unwrap();
        assert_eq!(c.label.chars().count(), LABEL_MAX_CHARS);
    }

    #[test]
    fn parse_categorized_returns_err_on_malformed_json() {
        assert!(parse_categorized("not json").is_err());
    }

    #[test]
    fn fallback_uses_hostname_minus_www() {
        let c = fallback("https://www.linear.app/team/PROJ-123");
        assert_eq!(c.label, "linear.app");
        assert_eq!(c.kind, link_kinds::OTHER);
    }

    #[test]
    fn fallback_handles_url_without_scheme() {
        let c = fallback("github.com/owner/repo");
        assert_eq!(c.label, "github.com");
    }

    #[test]
    fn fallback_strips_port_from_host() {
        let c = fallback("http://localhost:3000/x");
        assert_eq!(c.label, "localhost");
    }

    #[test]
    fn fallback_falls_back_when_url_is_garbage() {
        let c = fallback("");
        assert_eq!(c.label, "Link");
    }
}
