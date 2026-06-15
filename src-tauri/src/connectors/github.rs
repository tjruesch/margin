//! GitHub connector (#165) — polls the authenticated user's
//! contributions and persists them as a changelog.
//!
//! Auth is a Personal Access Token (classic or fine-grained), pasted
//! once in Settings and stored in the keychain as the connector's
//! `access_token` (no refresh token, far-future expiry). Unlike the
//! OAuth providers there's no browser flow — the `connect_github`
//! command validates the token via `GET /user` and writes the row.
//!
//! Each sync re-scans a rolling 30-day window via the Search API:
//!   - merged & open PRs authored by the user (`type:pr`) — merged PRs
//!     are "delivered features" in the changelog
//!   - commits authored by the user (`/search/commits`) — work in
//!     progress
//! The window doubles as the 30-day backfill: the very first sync after
//! connecting covers the last 30 days. Storage is accumulate-only
//! (see `github_contributions`), so the changelog grows past the window.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::anthropic::{ANTHROPIC_VERSION, DEFAULT_MODEL, ENDPOINT};

use super::github_contributions::{self, Contribution};
use super::registry::ConnectorRegistry;
use super::{Connector, ConnectorError, ConnectorRow, SyncCtx, SyncReport};

const KIND: &str = "github";
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Rolling lookback window. Also the backfill depth on first sync.
const WINDOW_DAYS: i64 = 30;
const SEARCH_PER_PAGE: u32 = 100;
/// Page cap per query. A personal account rarely exceeds 100
/// contributions of one type in 30 days; 3 pages (300) is generous
/// headroom without risking an unbounded crawl.
const MAX_PAGES: u32 = 3;

const USER_AGENT: &str = "Margin-Connector";
const API_VERSION: &str = "2022-11-28";

pub struct GitHubConnector {
    id: String,
    kind: String,
    display_name: String,
}

impl GitHubConnector {
    pub fn new(row: &ConnectorRow) -> Self {
        Self {
            id: row.id.clone(),
            kind: row.kind.clone(),
            display_name: row.display_name.clone(),
        }
    }

    /// Login portion of the connector_id (`github:<login>`). The Search
    /// API queries are scoped to this user.
    fn login(&self) -> &str {
        self.id.split_once(':').map(|(_, l)| l).unwrap_or(&self.id)
    }

    fn read_token(&self) -> Result<String, ConnectorError> {
        match crate::keychain::read_connector_tokens(&self.id) {
            Ok(Some(t)) => Ok(t.access_token),
            Ok(None) => Err(ConnectorError::ReauthNeeded(
                "no GitHub token stored — reconnect in Settings".into(),
            )),
            Err(e) => Err(ConnectorError::Other(format!("keychain read: {e}"))),
        }
    }
}

#[async_trait::async_trait]
impl Connector for GitHubConnector {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &str {
        &self.kind
    }
    fn display_name(&self) -> &str {
        &self.display_name
    }
    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    async fn sync(&self, ctx: SyncCtx<'_>) -> Result<SyncReport, ConnectorError> {
        let token = self.read_token()?;
        let login = self.login().to_string();
        let since = ms_to_date(current_unix_ms() - WINDOW_DAYS * 24 * 3600 * 1000);

        let client = build_client()?;

        // Pull requests only — merged PRs are delivered features, open /
        // closed PRs are work in progress. Commits proved too noisy
        // (squash-merge commits duplicate their PR; release tags add
        // chaff), so the changelog is PR-only.
        let prs = fetch_pull_requests(&client, &token, &login, &since).await?;
        let contributions: Vec<Contribution> =
            prs.into_iter().filter_map(|p| map_pr(&self.id, p)).collect();

        let report = {
            let mut conn = ctx
                .conn
                .lock()
                .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
            let r = github_contributions::upsert_contributions(&mut conn, &self.id, &contributions)
                .map_err(|e| ConnectorError::Other(format!("upsert contributions: {e}")))?;
            // Drain any commit rows left by the pre-PR-only build.
            let pruned = github_contributions::prune_non_prs(&conn, &self.id)
                .map_err(|e| ConnectorError::Other(format!("prune commits: {e}")))?;
            (r, pruned)
        };

        Ok(SyncReport {
            added: report.0.added,
            updated: report.0.updated,
            removed: report.1,
            skipped: 0,
        })
    }
}

pub fn register(registry: &ConnectorRegistry) {
    registry.register_kind(
        KIND,
        Arc::new(|row, _app| Ok(Arc::new(GitHubConnector::new(row)) as Arc<dyn Connector>)),
    );
}

// ----- Token validation (for the connect_github command) -----------------

#[derive(Debug, Clone)]
pub struct GitHubIdentity {
    pub login: String,
    pub name: Option<String>,
}

/// Validate a PAT by calling `GET /user`. Returns the account's login
/// (used to build `github:<login>`) and display name. Surfaces a
/// `ReauthNeeded` on 401 so the UI can say "that token didn't work".
pub async fn validate_token(token: &str) -> Result<GitHubIdentity, ConnectorError> {
    let client = build_client()?;
    let resp = client
        .get("https://api.github.com/user")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| ConnectorError::Network(format!("GET /user: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(resp.headers());
        let rl_reset = parse_rate_limit_reset(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(map_status(status, retry_after, rl_reset, body));
    }
    let user: RawUser = resp
        .json()
        .await
        .map_err(|e| ConnectorError::Other(format!("/user parse: {e}")))?;
    Ok(GitHubIdentity {
        login: user.login,
        name: user.name,
    })
}

#[derive(Debug, Deserialize)]
struct RawUser {
    login: String,
    #[serde(default)]
    name: Option<String>,
}

// ----- Search API clients -------------------------------------------------

fn build_client() -> Result<reqwest::Client, ConnectorError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))
}

async fn fetch_pull_requests(
    client: &reqwest::Client,
    token: &str,
    login: &str,
    since: &str,
) -> Result<Vec<RawIssue>, ConnectorError> {
    let q = format!("author:{login} type:pr updated:>={since}");
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let resp = client
            .get("https://api.github.com/search/issues")
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .query(&[
                ("q", q.as_str()),
                ("sort", "updated"),
                ("order", "desc"),
                ("per_page", &SEARCH_PER_PAGE.to_string()),
                ("page", &page.to_string()),
            ])
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("search/issues: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let rl_reset = parse_rate_limit_reset(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, rl_reset, body));
        }
        let page_data: IssueSearch = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("search/issues parse: {e}")))?;
        let n = page_data.items.len();
        all.extend(page_data.items);
        if n < SEARCH_PER_PAGE as usize {
            break;
        }
    }
    Ok(all)
}

// ----- Raw response shapes ------------------------------------------------

#[derive(Debug, Deserialize)]
struct IssueSearch {
    #[serde(default)]
    items: Vec<RawIssue>,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    number: i64,
    title: String,
    html_url: String,
    state: String,
    #[serde(default)]
    body: Option<String>,
    created_at: String,
    updated_at: String,
    repository_url: String,
    #[serde(default)]
    user: Option<RawAuthor>,
    #[serde(default)]
    pull_request: Option<RawPrInfo>,
}

#[derive(Debug, Deserialize)]
struct RawPrInfo {
    #[serde(default)]
    merged_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAuthor {
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

// ----- Mapping ------------------------------------------------------------

fn map_pr(connector_id: &str, raw: RawIssue) -> Option<Contribution> {
    let repo = repo_from_url(&raw.repository_url);
    let merged_at_ms = raw
        .pull_request
        .as_ref()
        .and_then(|p| p.merged_at.as_deref())
        .and_then(iso_to_ms);
    let state = if merged_at_ms.is_some() {
        "merged".to_string()
    } else {
        // search/issues `state` is open|closed for the issue side of a PR.
        raw.state.clone()
    };
    let external_id = format!("pr:{repo}#{}", raw.number);
    let created_at_ms = iso_to_ms(&raw.created_at).unwrap_or_else(current_unix_ms);
    let modified_ms = iso_to_ms(&raw.updated_at).unwrap_or(created_at_ms);
    let (author_login, author_avatar_url) = match raw.user {
        Some(u) => (u.login.unwrap_or_default(), u.avatar_url),
        None => (String::new(), None),
    };
    Some(Contribution {
        id: format!("{connector_id}::{external_id}"),
        connector_id: connector_id.to_string(),
        external_id,
        kind: "pr".to_string(),
        state,
        title: first_line(&raw.title),
        body: raw.body.filter(|b| !b.trim().is_empty()),
        repo,
        url: raw.html_url,
        author_login,
        author_avatar_url,
        created_at_ms,
        merged_at_ms,
        modified_ms,
        ai_summary: None,
        ai_highlight: None,
        ai_generated_ms: None,
    })
}

/// `https://api.github.com/repos/owner/name` → `owner/name`.
fn repo_from_url(url: &str) -> String {
    url.split("/repos/")
        .nth(1)
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// First non-empty line, trimmed. Commit messages are subject + body;
/// the changelog title is the subject only.
fn first_line(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let t = line.trim();
    if t.is_empty() {
        "(no title)".to_string()
    } else {
        t.to_string()
    }
}

// ----- HTTP error mapping -------------------------------------------------

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| secs * 1000)
}

/// Epoch-seconds value of `X-RateLimit-Reset`, when present.
fn parse_rate_limit_reset(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
}

fn rate_limit_exhausted(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn map_status(
    status: reqwest::StatusCode,
    retry_after_ms: Option<u64>,
    rate_limit_reset_secs: Option<i64>,
    body: String,
) -> ConnectorError {
    match status.as_u16() {
        401 => ConnectorError::ReauthNeeded(format!("github 401 (bad token): {body}")),
        403 | 429 => {
            // 403 covers both primary rate limits (reset header) and
            // secondary limits (Retry-After). 429 is the newer secondary
            // signal. Compute a wait from whichever the response carries.
            if let Some(ms) = retry_after_ms {
                ConnectorError::RateLimited { retry_after_ms: ms }
            } else if let Some(reset) = rate_limit_reset_secs {
                let wait = (reset * 1000 - current_unix_ms()).max(1_000) as u64;
                ConnectorError::RateLimited { retry_after_ms: wait }
            } else if status.as_u16() == 403 {
                // 403 without rate-limit headers → token lacks a scope.
                ConnectorError::ReauthNeeded(format!("github 403 (forbidden): {body}"))
            } else {
                ConnectorError::RateLimited { retry_after_ms: 60_000 }
            }
        }
        _ => ConnectorError::Other(format!("github {status}: {body}")),
    }
}

// ----- Changelog insight (#165 follow-up) ---------------------------------

/// A blog/LinkedIn-worthy angle on a PR. `None` at the call site means
/// nothing cleared the bar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightHighlight {
    pub angle: String,
    pub content: String,
}

/// What the model returns for one PR: a plain summary plus an optional
/// high-bar highlight.
#[derive(Debug, Clone, Serialize)]
pub struct GeneratedInsight {
    pub summary: String,
    pub highlight: Option<InsightHighlight>,
}

const INSIGHT_SYSTEM_PROMPT: &str = "You write a personal changelog entry for the author of a merged GitHub pull request.

You are given the PR title, description, and changed-file list. Produce STRICT JSON, no prose, no markdown fences:
{\"summary\": \"...\", \"highlight\": null | {\"angle\": \"...\", \"content\": \"...\"}}

summary: 1-3 sentences, plain language, describing what was implemented or changed and its architectural or user-facing effect. Past tense, factual, no hype or marketing adjectives. Don't just restate the title.

highlight: an OPTIONAL angle for a short blog or LinkedIn post. Apply a HIGH bar. Return null unless the PR contains a genuinely interesting technical detail, a non-obvious design decision or tradeoff, a debugging war story, a clever solution, or a transferable learning other engineers would find worth reading. Routine work — version bumps, dependency updates, copy/UI tweaks, straightforward CRUD, small refactors, config or test-only changes — must return null. Do NOT invent or embellish; use only the provided material. When in doubt, return null.
  angle: a crisp hook/headline for the post (max ~12 words).
  content: 2-4 sentences on the insight and why it's worth sharing.";

const INSIGHT_BODY_CAP: usize = 6000;
const INSIGHT_MAX_FILES: usize = 60;

/// Generate (and return) an AI insight for one merged/open PR. Fetches
/// the PR's changed-file list for extra context (best-effort), then asks
/// Claude for a summary + optional high-bar highlight. Errors propagate
/// as strings for the command layer to surface.
pub async fn generate_pr_insight(
    connector_id: &str,
    repo: &str,
    number: u64,
    title: &str,
    body: Option<&str>,
) -> Result<GeneratedInsight, String> {
    let api_key = crate::keychain::read_anthropic_api_key()
        .map_err(|e| format!("Anthropic API key not configured: {e}"))?;

    // Best-effort changed-file context — a missing/forbidden token just
    // means a body-only prompt, still useful.
    let files = match crate::keychain::read_connector_tokens(connector_id) {
        Ok(Some(t)) => {
            let client = build_client().map_err(|e| e.to_string())?;
            fetch_pr_files(&client, &t.access_token, repo, number)
                .await
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let user_prompt = build_insight_prompt(repo, title, body, &files);
    call_anthropic_insight(&api_key, &user_prompt).await
}

#[derive(Debug, Deserialize)]
struct RawPrFile {
    filename: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    additions: i64,
    #[serde(default)]
    deletions: i64,
}

async fn fetch_pr_files(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    number: u64,
) -> Result<Vec<RawPrFile>, ConnectorError> {
    let url = format!("https://api.github.com/repos/{repo}/pulls/{number}/files");
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .query(&[("per_page", "100")])
        .send()
        .await
        .map_err(|e| ConnectorError::Network(format!("pulls/files: {e}")))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    resp.json::<Vec<RawPrFile>>()
        .await
        .map_err(|e| ConnectorError::Other(format!("pulls/files parse: {e}")))
}

fn build_insight_prompt(
    repo: &str,
    title: &str,
    body: Option<&str>,
    files: &[RawPrFile],
) -> String {
    let mut s = String::new();
    s.push_str(&format!("Repository: {repo}\nPull request: {title}\n\n"));
    if let Some(b) = body {
        let b = b.trim();
        if !b.is_empty() {
            let capped: String = b.chars().take(INSIGHT_BODY_CAP).collect();
            s.push_str("Description:\n");
            s.push_str(&capped);
            s.push_str("\n\n");
        }
    }
    if !files.is_empty() {
        s.push_str(&format!("Changed files ({}):\n", files.len()));
        for f in files.iter().take(INSIGHT_MAX_FILES) {
            s.push_str(&format!(
                "- {} ({}, +{}/-{})\n",
                f.filename,
                f.status.as_deref().unwrap_or("modified"),
                f.additions,
                f.deletions
            ));
        }
    }
    s
}

#[derive(Serialize)]
struct InsightApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    system: &'a str,
    messages: Vec<InsightApiMessage<'a>>,
}

#[derive(Serialize)]
struct InsightApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

async fn call_anthropic_insight(
    api_key: &str,
    user_prompt: &str,
) -> Result<GeneratedInsight, String> {
    let body = InsightApiRequest {
        model: DEFAULT_MODEL,
        max_tokens: 700,
        stream: false,
        system: INSIGHT_SYSTEM_PROMPT,
        messages: vec![InsightApiMessage {
            role: "user",
            content: user_prompt,
        }],
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
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
    parse_insight(&text)
}

#[derive(Deserialize)]
struct RawInsight {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    highlight: Option<RawHighlight>,
}

#[derive(Deserialize)]
struct RawHighlight {
    #[serde(default)]
    angle: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// Parse the model's JSON, tolerating optional ```json fences. A
/// highlight missing either field collapses to None (below bar).
fn parse_insight(raw: &str) -> Result<GeneratedInsight, String> {
    let stripped = strip_json_fences(raw);
    let parsed: RawInsight =
        serde_json::from_str(&stripped).map_err(|e| format!("insight parse: {e}"))?;
    let summary = parsed
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing summary".to_string())?
        .to_string();
    let highlight = parsed.highlight.and_then(|h| {
        let angle = h.angle.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
        let content = h.content.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
        Some(InsightHighlight {
            angle: angle.to_string(),
            content: content.to_string(),
        })
    });
    Ok(GeneratedInsight { summary, highlight })
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

// ----- Time helpers -------------------------------------------------------

fn iso_to_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
}

fn ms_to_date(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp(ms / 1000, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_url_parses_owner_name() {
        assert_eq!(
            repo_from_url("https://api.github.com/repos/octocat/hello-world"),
            "octocat/hello-world"
        );
        assert_eq!(repo_from_url("garbage"), "unknown");
    }

    #[test]
    fn first_line_takes_subject_only() {
        assert_eq!(first_line("Fix the bug\n\nLong body here"), "Fix the bug");
        assert_eq!(first_line("   \nSecond line"), "Second line");
        assert_eq!(first_line(""), "(no title)");
    }

    #[test]
    fn map_pr_marks_merged() {
        let raw = RawIssue {
            number: 42,
            title: "Add feature".into(),
            html_url: "https://github.com/o/r/pull/42".into(),
            state: "closed".into(),
            body: Some("desc".into()),
            created_at: "2026-06-01T10:00:00Z".into(),
            updated_at: "2026-06-02T10:00:00Z".into(),
            repository_url: "https://api.github.com/repos/o/r".into(),
            user: Some(RawAuthor {
                login: Some("octocat".into()),
                avatar_url: Some("https://avatars/x.png".into()),
            }),
            pull_request: Some(RawPrInfo {
                merged_at: Some("2026-06-02T09:00:00Z".into()),
            }),
        };
        let c = map_pr("github:octocat", raw).unwrap();
        assert_eq!(c.kind, "pr");
        assert_eq!(c.state, "merged");
        assert_eq!(c.external_id, "pr:o/r#42");
        assert_eq!(c.repo, "o/r");
        assert!(c.merged_at_ms.is_some());
        assert_eq!(c.author_login, "octocat");
    }

    #[test]
    fn map_pr_open_keeps_state() {
        let raw = RawIssue {
            number: 7,
            title: "WIP".into(),
            html_url: "https://github.com/o/r/pull/7".into(),
            state: "open".into(),
            body: None,
            created_at: "2026-06-01T10:00:00Z".into(),
            updated_at: "2026-06-01T10:00:00Z".into(),
            repository_url: "https://api.github.com/repos/o/r".into(),
            user: None,
            pull_request: Some(RawPrInfo { merged_at: None }),
        };
        let c = map_pr("github:octocat", raw).unwrap();
        assert_eq!(c.state, "open");
        assert!(c.merged_at_ms.is_none());
    }

    #[test]
    fn parse_insight_with_highlight() {
        let raw = r#"{"summary":"Did X.","highlight":{"angle":"Y","content":"Z because reasons."}}"#;
        let g = parse_insight(raw).unwrap();
        assert_eq!(g.summary, "Did X.");
        let h = g.highlight.unwrap();
        assert_eq!(h.angle, "Y");
    }

    #[test]
    fn parse_insight_null_highlight_below_bar() {
        let raw = "```json\n{\"summary\":\"Bumped a dep.\",\"highlight\":null}\n```";
        let g = parse_insight(raw).unwrap();
        assert_eq!(g.summary, "Bumped a dep.");
        assert!(g.highlight.is_none());
    }

    #[test]
    fn parse_insight_partial_highlight_collapses_to_none() {
        let raw = r#"{"summary":"S","highlight":{"angle":"only angle"}}"#;
        let g = parse_insight(raw).unwrap();
        assert!(g.highlight.is_none());
    }

    #[test]
    fn parse_insight_requires_summary() {
        assert!(parse_insight(r#"{"highlight":null}"#).is_err());
    }

    #[test]
    fn ms_to_date_formats_day() {
        // 2026-06-14 ~ 1781740800 s. Just assert shape.
        let d = ms_to_date(1_781_740_800_000);
        assert_eq!(d.len(), 10);
        assert_eq!(&d[4..5], "-");
    }
}
