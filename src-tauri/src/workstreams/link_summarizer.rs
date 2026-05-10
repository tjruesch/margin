//! AI summary for workstream link URLs.
//!
//! After a link is added via the paste-only composer, this module
//! runs as a fire-and-forget background task: scrape the URL via
//! Firecrawl, ask Claude Haiku for a 2–3 sentence summary, write
//! the result back to the row, emit a `workstream-link-summarized`
//! event so the frontend re-renders without refetching.
//!
//! Failure semantics: any error (no key, network, malformed
//! response, scrape returns thin content) leaves `summary = NULL`.
//! The chip just stays single-line — no retry, no surfaced error.
//!
//! Models: claude-haiku-4-5 (cheap, fast; 2–3 sentences from a
//! single page is well within its strength).
//!
//! Firecrawl: `POST /v1/scrape` with `{url, formats: ["markdown"]}`.
//! We pass `onlyMainContent: true` to drop nav/footer chrome.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::anthropic::{ANTHROPIC_VERSION, ENDPOINT, HAIKU_MODEL};

/// Hard cap on the rendered summary length. Haiku usually stays
/// well under this; we trim defensively so an over-eager model
/// can't bloat the chip.
const SUMMARY_MAX_CHARS: usize = 280;

/// Cap on scraped markdown sent to Haiku. Most pages fit; long
/// articles are truncated and we accept that the summary may miss
/// later sections. Bigger context = bigger spend, so we choose a
/// balance.
const SCRAPED_MARKDOWN_CAP: usize = 12_000;

const FIRECRAWL_ENDPOINT: &str = "https://api.firecrawl.dev/v1/scrape";

const SUMMARIZE_SYSTEM_PROMPT: &str = "Summarize what the linked page is, in 2-3 short sentences.

Rules:
- Lead with what the page IS (a repo, a doc, a blog post, a ticket).
- Then what it's ABOUT — the artifact's purpose, not your own commentary.
- 280 characters maximum, no markdown, no links.
- Skip filler like \"This page is\" or \"This is a\".

If the input looks like a login wall, paywall, error, or 404 page, return only the literal string: NO_SUMMARY";

/// Public entry point. The caller spawns this on a Tokio task right
/// after `add_workstream_link_from_url` inserts the row. Idempotent
/// in the sense that if the link is gone by the time the work
/// completes, the UPDATE no-ops.
pub async fn populate_summary(app: AppHandle, link_id: String, url: String) {
    let firecrawl_key = match crate::keychain::read_firecrawl_api_key() {
        Ok(k) => k,
        Err(_) => {
            eprintln!("[link-summarizer] no Firecrawl key, skipping");
            return;
        }
    };
    let anthropic_key = match crate::keychain::read_anthropic_api_key() {
        Ok(k) => k,
        Err(_) => {
            eprintln!("[link-summarizer] no Anthropic key, skipping");
            return;
        }
    };
    let scraped = match firecrawl_scrape(&firecrawl_key, &url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[link-summarizer] scrape failed for {url}: {e}");
            return;
        }
    };
    if scraped.markdown.trim().len() < 80 {
        eprintln!("[link-summarizer] scraped content too thin for {url}, skipping");
        return;
    }
    let summary = match summarize_with_haiku(&anthropic_key, &scraped).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[link-summarizer] summarize failed for {url}: {e}");
            return;
        }
    };
    if summary.trim().is_empty() || summary.trim() == "NO_SUMMARY" {
        eprintln!("[link-summarizer] model declined to summarize {url}");
        return;
    }
    let summary = truncate_summary(&summary);
    {
        let conn_state = app.state::<std::sync::Mutex<rusqlite::Connection>>();
        let conn = match conn_state.lock() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("[link-summarizer] conn lock failed: {e}");
                return;
            }
        };
        if let Err(e) =
            super::persist::set_workstream_link_summary(&conn, &link_id, Some(&summary))
        {
            eprintln!("[link-summarizer] persist update failed for {link_id}: {e}");
            return;
        }
    }
    let _ = app.emit(
        "workstream-link-summarized",
        SummarizedEvent {
            link_id: link_id.clone(),
            summary: summary.clone(),
        },
    );
}

#[derive(Serialize, Clone)]
struct SummarizedEvent {
    link_id: String,
    summary: String,
}

#[derive(Debug, Clone)]
pub struct Scraped {
    pub title: Option<String>,
    pub markdown: String,
}

#[derive(Serialize)]
struct FirecrawlRequest<'a> {
    url: &'a str,
    formats: &'a [&'a str],
    #[serde(rename = "onlyMainContent")]
    only_main_content: bool,
}

#[derive(Deserialize)]
struct FirecrawlResponse {
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    data: Option<FirecrawlData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct FirecrawlData {
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    metadata: Option<FirecrawlMetadata>,
}

#[derive(Deserialize)]
struct FirecrawlMetadata {
    #[serde(default)]
    title: Option<String>,
}

pub async fn firecrawl_scrape(api_key: &str, url: &str) -> Result<Scraped, String> {
    let body = FirecrawlRequest {
        url,
        formats: &["markdown"],
        only_main_content: true,
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("client init: {e}"))?;
    let resp = client
        .post(FIRECRAWL_ENDPOINT)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        return Err(format!("firecrawl returned {status}: {text}"));
    }
    parse_firecrawl_response(&text)
}

/// Parse Firecrawl's `/v1/scrape` JSON envelope. Public for tests;
/// the caller never has to construct one of these by hand.
pub fn parse_firecrawl_response(raw: &str) -> Result<Scraped, String> {
    let parsed: FirecrawlResponse =
        serde_json::from_str(raw).map_err(|e| format!("parse: {e}"))?;
    if parsed.success == Some(false) {
        let reason = parsed.error.unwrap_or_else(|| "unknown error".to_string());
        return Err(format!("firecrawl reported failure: {reason}"));
    }
    let data = parsed
        .data
        .ok_or_else(|| "firecrawl response missing data".to_string())?;
    let markdown = data
        .markdown
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "firecrawl returned empty markdown".to_string())?;
    let title = data.metadata.and_then(|m| m.title);
    let markdown = if markdown.chars().count() > SCRAPED_MARKDOWN_CAP {
        markdown
            .chars()
            .take(SCRAPED_MARKDOWN_CAP)
            .collect::<String>()
            + "…"
    } else {
        markdown
    };
    Ok(Scraped { title, markdown })
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

async fn summarize_with_haiku(api_key: &str, scraped: &Scraped) -> Result<String, String> {
    let user_message = match &scraped.title {
        Some(t) if !t.trim().is_empty() => {
            format!("Title: {t}\n\n{}", scraped.markdown)
        }
        _ => scraped.markdown.clone(),
    };
    let body = ApiRequest {
        model: HAIKU_MODEL,
        max_tokens: 200,
        stream: false,
        system: SUMMARIZE_SYSTEM_PROMPT,
        messages: vec![ApiMessage {
            role: "user",
            content: &user_message,
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
        .map_err(|e| format!("anthropic parse: {e}"))?;
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
    Ok(text.trim().to_string())
}

/// Char-aware truncation with an ellipsis suffix.
pub fn truncate_summary(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= SUMMARY_MAX_CHARS {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(SUMMARY_MAX_CHARS).collect();
    format!("{truncated}…")
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_firecrawl_response_extracts_markdown_and_title() {
        let raw = r#"{
            "success": true,
            "data": {
                "markdown": "Hello world\n\nThis is the page body.",
                "metadata": {"title": "Example Domain"}
            }
        }"#;
        let s = parse_firecrawl_response(raw).unwrap();
        assert_eq!(s.title.as_deref(), Some("Example Domain"));
        assert!(s.markdown.starts_with("Hello world"));
    }

    #[test]
    fn parse_firecrawl_response_rejects_failed_envelope() {
        let raw = r#"{"success": false, "error": "scrape blocked"}"#;
        let err = parse_firecrawl_response(raw).unwrap_err();
        assert!(err.contains("scrape blocked"));
    }

    #[test]
    fn parse_firecrawl_response_rejects_empty_markdown() {
        let raw = r#"{"success": true, "data": {"markdown": "  "}}"#;
        let err = parse_firecrawl_response(raw).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn parse_firecrawl_response_handles_missing_data() {
        let raw = r#"{"success": true}"#;
        let err = parse_firecrawl_response(raw).unwrap_err();
        assert!(err.contains("missing data"));
    }

    #[test]
    fn parse_firecrawl_response_truncates_long_markdown() {
        let big = "a".repeat(20_000);
        let raw = format!(r#"{{"success": true, "data": {{"markdown": "{big}"}}}}"#);
        let s = parse_firecrawl_response(&raw).unwrap();
        // Cap + the appended ellipsis.
        assert_eq!(s.markdown.chars().count(), SCRAPED_MARKDOWN_CAP + 1);
        assert!(s.markdown.ends_with('…'));
    }

    #[test]
    fn truncate_summary_passes_short_strings_through() {
        assert_eq!(truncate_summary("Short summary."), "Short summary.");
    }

    #[test]
    fn truncate_summary_appends_ellipsis_on_overflow() {
        let s = "x".repeat(SUMMARY_MAX_CHARS + 50);
        let out = truncate_summary(&s);
        assert_eq!(out.chars().count(), SUMMARY_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_summary_trims_whitespace_first() {
        assert_eq!(truncate_summary("   hi   "), "hi");
    }
}
