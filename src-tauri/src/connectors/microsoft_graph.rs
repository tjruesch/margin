//! Microsoft Graph calendar connector (#63).
//!
//! Pulls calendar events from `/me/calendarView` via Microsoft Graph,
//! maps them onto the provider-agnostic `CalendarEvent` shape, and
//! writes through `connectors::calendar::upsert_window`.
//!
//! Window: last 14 days through next 30 days. Polled every 5 minutes
//! by the `SyncRunner`.
//!
//! All times sent and received in UTC via the `Prefer:
//! outlook.timezone="UTC"` header. Saves us from juggling Microsoft's
//! Windows-style timezone names against IANA.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::calendar::{CalendarAttendee, CalendarEvent};
use super::email::{EmailMessage, EmailRecipient};
use super::oauth::with_valid_token;
use super::registry::ConnectorRegistry;
use super::{Connector, ConnectorError, ConnectorRow, SyncCtx, SyncReport};
use crate::team::{self, OwnerResolver, TeamMember};

const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const WINDOW_FORWARD_MS: i64 = 30 * 24 * 3600 * 1000;
const PAGE_SIZE: u32 = 100;

/// Mail ingestion: 200 most recent inbox messages per sync. 4 pages
/// of 50 each — Graph caps `$top` at 1000 but smaller pages mean
/// faster first-byte latency and tolerable memory at peak.
const MAIL_PAGE_SIZE: u32 = 50;
const MAIL_MAX_PAGES: usize = 4;

const KIND: &str = "microsoft_graph";

pub struct MicrosoftGraphConnector {
    id: String,
    kind: String,
    display_name: String,
}

impl MicrosoftGraphConnector {
    pub fn new(row: &ConnectorRow) -> Self {
        Self {
            id: row.id.clone(),
            kind: row.kind.clone(),
            display_name: row.display_name.clone(),
        }
    }

    /// Email portion of the connector_id (`microsoft_graph:<email>`).
    /// Used to flag the corresponding attendee row as `is_self`.
    fn self_email(&self) -> Option<&str> {
        self.id.split_once(':').map(|(_, email)| email)
    }
}

#[async_trait::async_trait]
impl Connector for MicrosoftGraphConnector {
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
        let now = current_unix_ms();
        let window_start = now - WINDOW_BACK_MS;
        let window_end = now + WINDOW_FORWARD_MS;

        // Snapshot team members once for both calendar and mail. Keep
        // the lock window short — release before any network I/O.
        let team = {
            let conn = ctx
                .conn
                .lock()
                .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
            team::list_team_members_raw(&conn).map_err(ConnectorError::Other)?
        };
        let resolver = AttendeeResolver::new(&team);
        let self_email = self.self_email();

        // ---- Calendar half ------------------------------------------------
        let raw_events = with_valid_token(ctx.app, &self.id, &self.kind, |access| async move {
            fetch_calendar_view(&access, window_start, window_end).await
        })
        .await?;

        let events: Vec<CalendarEvent> = raw_events
            .into_iter()
            .map(|raw| map_event(&self.id, raw, &resolver, self_email))
            .collect();

        let calendar_report = {
            let mut conn = ctx
                .conn
                .lock()
                .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
            super::calendar::upsert_window(&mut conn, &self.id, &events, window_start, window_end)
                .map_err(|e| ConnectorError::Other(format!("upsert events: {e}")))?
        };

        // ---- Mail half ----------------------------------------------------
        // Calendar succeeded and rows are committed; the mail half is
        // best-effort for transient errors (network, rate limit). But
        // a `ReauthNeeded` (typically: existing token lacks `Mail.Read`)
        // MUST surface as the connector's overall last_error so the
        // Settings UI prompts a Reconnect — otherwise mail would
        // silently never sync and the user has no signal.
        let mail_report = match self.sync_mail(&ctx, &resolver).await {
            Ok(report) => report,
            Err(ConnectorError::ReauthNeeded(msg)) => {
                eprintln!("[microsoft_graph] mail sync needs reauth: {msg}");
                return Err(ConnectorError::ReauthNeeded(msg));
            }
            Err(e) => {
                eprintln!("[microsoft_graph] mail sync failed (non-fatal): {e}");
                super::email::UpsertReport::default()
            }
        };

        // ---- Teams half (#105) -------------------------------------------
        // Best-effort. Silent skip on ReauthNeeded — that's the
        // expected path for existing users who haven't reconnected
        // since `Chat.Read` was added to the provider scopes. Mail
        // + calendar already succeeded, so the connector overall is
        // healthy; the user will see a Reconnect prompt the next time
        // they look at Settings only if mail/calendar separately
        // started failing, but Teams alone failing is non-fatal.
        let teams_report = match self.sync_teams(&ctx, &resolver).await {
            Ok(report) => report,
            Err(ConnectorError::ReauthNeeded(msg)) => {
                eprintln!(
                    "[microsoft_graph] Teams sync skipped — reauth needed (probably missing Chat.Read scope): {msg}"
                );
                super::teams::UpsertReport::default()
            }
            Err(e) => {
                eprintln!("[microsoft_graph] Teams sync failed (non-fatal): {e}");
                super::teams::UpsertReport::default()
            }
        };

        Ok(SyncReport {
            added: calendar_report.added + mail_report.added + teams_report.added,
            updated: calendar_report.updated + mail_report.updated + teams_report.updated,
            removed: calendar_report.removed,
            skipped: mail_report.skipped + teams_report.skipped,
        })
    }

    /// Lazy body fetch (#69). The trait method dispatches here once
    /// the registry resolves connector_id → this concrete connector
    /// (post-#61 refactor in `commands.rs::get_email_body`).
    async fn fetch_message_body(
        &self,
        app: &tauri::AppHandle,
        external_id: &str,
    ) -> Result<Option<String>, ConnectorError> {
        let id = self.id.clone();
        let kind = self.kind.clone();
        let external = external_id.to_string();
        with_valid_token(app, &id, &kind, |access| async move {
            fetch_message_body(&access, &external).await
        })
        .await
    }
}

impl MicrosoftGraphConnector {
    async fn sync_mail(
        &self,
        ctx: &SyncCtx<'_>,
        resolver: &AttendeeResolver<'_>,
    ) -> Result<super::email::UpsertReport, ConnectorError> {
        let raw_messages =
            with_valid_token(ctx.app, &self.id, &self.kind, |access| async move {
                fetch_inbox_messages(&access).await
            })
            .await?;

        let messages: Vec<EmailMessage> = raw_messages
            .into_iter()
            .filter_map(|raw| map_message(&self.id, raw, resolver))
            .collect();

        let report = {
            let mut conn = ctx
                .conn
                .lock()
                .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
            super::email::upsert_messages(&mut conn, &self.id, &messages)
                .map_err(|e| ConnectorError::Other(format!("upsert messages: {e}")))?
        };
        Ok(report)
    }

    /// Sync Teams 1:1 + group + meeting chat messages (#105). Two-step
    /// fetch: list the user's chats, then per chat pull recent messages.
    /// Channel chats are excluded. Returns aggregate upsert counts.
    async fn sync_teams(
        &self,
        ctx: &SyncCtx<'_>,
        resolver: &AttendeeResolver<'_>,
    ) -> Result<super::teams::UpsertReport, ConnectorError> {
        let chats = with_valid_token(ctx.app, &self.id, &self.kind, |access| async move {
            fetch_teams_chats(&access).await
        })
        .await?;

        let self_email = self.self_email().map(|s| s.to_string());
        let now = current_unix_ms();
        let cutoff_ms = now - TEAMS_LOOKBACK_MS;
        let mut total = super::teams::UpsertReport::default();

        for chat in chats {
            // Skip channels — v1 scope.
            if chat
                .chat_type
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("channel"))
                .unwrap_or(false)
            {
                continue;
            }
            let chat_id = chat.id.clone();
            let chat_kind = chat.chat_type.clone().unwrap_or_else(|| "group".to_string());
            let chat_topic = chat.topic.clone();

            // Persist chat membership for this chat.
            let members: Vec<super::teams::TeamsChatMember> = chat
                .members
                .iter()
                .map(|m| map_chat_member(&chat_id, m, resolver, self_email.as_deref()))
                .collect();
            {
                let mut conn = ctx
                    .conn
                    .lock()
                    .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
                super::teams::upsert_chat_members(&mut conn, &chat_id, &members)
                    .map_err(|e| ConnectorError::Other(format!("upsert chat members: {e}")))?;
            }

            // Fetch + map messages.
            let raw_messages =
                match with_valid_token(ctx.app, &self.id, &self.kind, |access| {
                    let chat_id = chat_id.clone();
                    async move { fetch_teams_chat_messages(&access, &chat_id).await }
                })
                .await
                {
                    Ok(v) => v,
                    Err(ConnectorError::Network(e)) => {
                        eprintln!("[microsoft_graph] teams messages for {chat_id} failed: {e}");
                        continue;
                    }
                    Err(e) => return Err(e),
                };

            let messages: Vec<super::teams::TeamsMessage> = raw_messages
                .into_iter()
                .filter_map(|raw| {
                    if raw.message_type.as_deref() != Some("message") {
                        return None; // skip system events
                    }
                    let sent_at_ms = parse_iso8601_ms(raw.created_date_time.as_deref()?)?;
                    if sent_at_ms < cutoff_ms {
                        return None;
                    }
                    Some(map_teams_message(
                        &self.id,
                        &chat_id,
                        &chat_kind,
                        chat_topic.as_deref(),
                        sent_at_ms,
                        raw,
                        &members,
                    ))
                })
                .collect();

            if messages.is_empty() {
                continue;
            }

            let report = {
                let mut conn = ctx
                    .conn
                    .lock()
                    .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
                super::teams::upsert_messages(&mut conn, &messages)
                    .map_err(|e| ConnectorError::Other(format!("upsert teams messages: {e}")))?
            };
            total.added += report.added;
            total.updated += report.updated;
            total.skipped += report.skipped;
        }
        Ok(total)
    }
}

/// Teams: only sync messages from the last 90 days. Older history is
/// excluded for both cost (Graph paging) and embedding-quota reasons.
const TEAMS_LOOKBACK_MS: i64 = 90 * 24 * 3600 * 1000;
const TEAMS_CHATS_PAGE_SIZE: u32 = 50;
const TEAMS_MSGS_PAGE_SIZE: u32 = 50;
const TEAMS_MAX_PAGES: usize = 4;

/// Lazy-fetch a single message body via Graph. Public so the
/// `get_email_body` Tauri command can call it.
pub async fn fetch_message_body(
    access_token: &str,
    external_id: &str,
) -> Result<Option<String>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;
    // URL-encode the external id (Graph IDs contain `=`/`/`/`+`).
    let encoded = percent_encode_path(external_id);
    let url = format!(
        "https://graph.microsoft.com/v1.0/me/messages/{encoded}?$select=body"
    );
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| ConnectorError::Network(format!("graph GET body: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(map_status(status, retry_after, body));
    }
    let parsed: RawMessageBody = resp
        .json()
        .await
        .map_err(|e| ConnectorError::Other(format!("graph body parse: {e}")))?;
    Ok(parsed.body.and_then(|b| b.content))
}

/// Plug the factory into the registry. Called once at app boot from
/// `lib.rs`. Future PR (#61, Google) calls a similar `register()` from
/// its own module.
pub fn register(registry: &ConnectorRegistry) {
    registry.register_kind(
        KIND,
        Arc::new(|row, _app| {
            Ok(Arc::new(MicrosoftGraphConnector::new(row)) as Arc<dyn Connector>)
        }),
    );
}

// ----- Teams Graph layer (#105) ------------------------------------------

#[derive(Debug, Deserialize)]
struct RawTeamsChat {
    id: String,
    #[serde(rename = "chatType", default)]
    chat_type: Option<String>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    members: Vec<RawTeamsChatMember>,
}

#[derive(Debug, Deserialize)]
struct RawTeamsChatMember {
    #[serde(rename = "userId", default)]
    user_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphChatsPage {
    value: Vec<RawTeamsChat>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTeamsMessage {
    id: String,
    #[serde(rename = "messageType", default)]
    message_type: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(default)]
    from: Option<RawTeamsFromWrapper>,
    #[serde(default)]
    body: Option<RawTeamsBody>,
    #[serde(rename = "replyToId", default)]
    reply_to_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTeamsFromWrapper {
    #[serde(default)]
    user: Option<RawTeamsUser>,
}

#[derive(Debug, Deserialize)]
struct RawTeamsUser {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTeamsBody {
    #[serde(default)]
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphChatMessagesPage {
    value: Vec<RawTeamsMessage>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
}

async fn fetch_teams_chats(
    access_token: &str,
) -> Result<Vec<RawTeamsChat>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;

    let mut url = format!(
        "https://graph.microsoft.com/v1.0/me/chats?$top={TEAMS_CHATS_PAGE_SIZE}&$expand=members"
    );
    let mut all: Vec<RawTeamsChat> = Vec::new();
    let mut pages = 0usize;

    loop {
        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("graph GET chats: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, body));
        }
        let page: GraphChatsPage = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("graph chats parse: {e}")))?;
        all.extend(page.value);
        pages += 1;
        if pages >= TEAMS_MAX_PAGES {
            break;
        }
        match page.next_link {
            Some(next) if !next.is_empty() => url = next,
            _ => break,
        }
    }
    Ok(all)
}

async fn fetch_teams_chat_messages(
    access_token: &str,
    chat_id: &str,
) -> Result<Vec<RawTeamsMessage>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;

    let encoded = percent_encode_path(chat_id);
    let mut url = format!(
        "https://graph.microsoft.com/v1.0/me/chats/{encoded}/messages?$top={TEAMS_MSGS_PAGE_SIZE}"
    );
    let mut all: Vec<RawTeamsMessage> = Vec::new();
    let mut pages = 0usize;

    loop {
        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("graph GET chat messages: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, body));
        }
        let page: GraphChatMessagesPage = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("graph chat msgs parse: {e}")))?;
        all.extend(page.value);
        pages += 1;
        if pages >= TEAMS_MAX_PAGES {
            break;
        }
        match page.next_link {
            Some(next) if !next.is_empty() => url = next,
            _ => break,
        }
    }
    Ok(all)
}

fn map_chat_member(
    chat_id: &str,
    raw: &RawTeamsChatMember,
    resolver: &AttendeeResolver<'_>,
    self_email: Option<&str>,
) -> super::teams::TeamsChatMember {
    let email = raw.email.as_deref().map(str::to_lowercase);
    let team_member_id = email.as_deref().and_then(|e| {
        resolver
            .resolve_attendee(e, raw.display_name.as_deref())
            .map(|s| s.to_string())
    });
    let is_self = match (email.as_deref(), self_email) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    super::teams::TeamsChatMember {
        chat_id: chat_id.to_string(),
        aad_id: raw.user_id.clone().unwrap_or_default(),
        email,
        display_name: raw.display_name.clone(),
        team_member_id,
        is_self,
    }
}

fn map_teams_message(
    connector_id: &str,
    chat_id: &str,
    chat_kind: &str,
    chat_topic: Option<&str>,
    sent_at_ms: i64,
    raw: RawTeamsMessage,
    members: &[super::teams::TeamsChatMember],
) -> super::teams::TeamsMessage {
    let modified_ms = raw
        .last_modified_date_time
        .as_deref()
        .and_then(parse_iso8601_ms)
        .unwrap_or(sent_at_ms);
    let from_user = raw.from.as_ref().and_then(|f| f.user.as_ref());
    let from_aad_id = from_user.and_then(|u| u.id.clone());
    let from_name = from_user.and_then(|u| u.display_name.clone());
    // Resolve from_email via the chat-members snapshot we already have.
    let from_email = from_aad_id
        .as_deref()
        .and_then(|aad| members.iter().find(|m| m.aad_id == aad))
        .and_then(|m| m.email.clone());
    let body_html = raw
        .body
        .as_ref()
        .filter(|b| b.content_type.as_deref().unwrap_or("html") == "html")
        .and_then(|b| b.content.clone());
    let body_preview = raw
        .body
        .as_ref()
        .and_then(|b| b.content.as_deref())
        .map(|s| strip_html_for_preview(s))
        .filter(|s| !s.is_empty());

    let id = format!("{}::teams::{}", connector_id, raw.id);
    super::teams::TeamsMessage {
        id,
        connector_id: connector_id.to_string(),
        external_id: raw.id,
        chat_id: chat_id.to_string(),
        chat_kind: chat_kind.to_string(),
        chat_topic: chat_topic.map(str::to_string),
        sent_at_ms,
        from_aad_id,
        from_email,
        from_name,
        body_html,
        body_preview,
        reply_to_id: raw.reply_to_id,
        modified_ms,
        raw_etag: None,
    }
}

/// Quick-and-dirty HTML strip for body_preview generation when Graph
/// returns HTML content. Not a full parser — collapses tags + extra
/// whitespace, truncates at 240 chars (matches Graph's typical preview).
fn strip_html_for_preview(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut prev_space = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            _ if !in_tag => {
                if c.is_whitespace() {
                    if !prev_space {
                        out.push(' ');
                        prev_space = true;
                    }
                } else {
                    out.push(c);
                    prev_space = false;
                }
            }
            _ => {}
        }
    }
    let trimmed = out.trim();
    if trimmed.chars().count() <= 240 {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(237).collect();
        format!("{cut}…")
    }
}

fn parse_iso8601_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

// ----- Graph API client --------------------------------------------------

async fn fetch_calendar_view(
    access_token: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<RawEvent>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;

    let start_iso = ms_to_iso(start_ms);
    let end_iso = ms_to_iso(end_ms);
    let mut url = format!(
        "https://graph.microsoft.com/v1.0/me/calendarView?startDateTime={start_iso}&endDateTime={end_iso}&$top={PAGE_SIZE}&$orderby=start/dateTime"
    );

    let mut all_events: Vec<RawEvent> = Vec::new();

    loop {
        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .header("Prefer", "outlook.timezone=\"UTC\"")
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("graph GET: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, body));
        }

        let page: GraphPage = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("graph response parse: {e}")))?;
        all_events.extend(page.value);

        match page.next_link {
            Some(next) if !next.is_empty() => url = next,
            _ => break,
        }
    }

    Ok(all_events)
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| secs * 1000)
}

fn map_status(
    status: reqwest::StatusCode,
    retry_after_ms: Option<u64>,
    body: String,
) -> ConnectorError {
    match status.as_u16() {
        401 => ConnectorError::ReauthNeeded(format!("graph 401: {body}")),
        // 403 typically means the token's grant doesn't cover the
        // requested resource (e.g. existing token without Mail.Read
        // calling /me/messages). Reconnecting re-prompts consent for
        // the new scope set, which is the right user action.
        403 => ConnectorError::ReauthNeeded(format!("graph 403: {body}")),
        429 => ConnectorError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(60_000),
        },
        503 => ConnectorError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(30_000),
        },
        _ => ConnectorError::Other(format!("graph {status}: {body}")),
    }
}

// ----- JSON shape --------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GraphPage {
    value: Vec<RawEvent>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawEvent {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(rename = "start")]
    pub(crate) start: Option<RawDateTime>,
    #[serde(rename = "end")]
    pub(crate) end: Option<RawDateTime>,
    #[serde(rename = "isAllDay", default)]
    pub(crate) is_all_day: bool,
    #[serde(rename = "isCancelled", default)]
    pub(crate) is_cancelled: bool,
    #[serde(default)]
    pub(crate) location: Option<RawLocation>,
    #[serde(rename = "bodyPreview", default)]
    pub(crate) body_preview: Option<String>,
    #[serde(default)]
    pub(crate) attendees: Vec<RawAttendee>,
    #[serde(default)]
    pub(crate) organizer: Option<RawOrganizer>,
    #[serde(rename = "@odata.etag", default)]
    pub(crate) etag: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    pub(crate) last_modified: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawDateTime {
    #[serde(rename = "dateTime")]
    pub(crate) date_time: String,
    #[serde(rename = "timeZone", default)]
    pub(crate) time_zone: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawLocation {
    #[serde(rename = "displayName", default)]
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAttendee {
    #[serde(rename = "emailAddress")]
    pub(crate) email_address: Option<RawEmailAddress>,
    #[serde(default)]
    pub(crate) status: Option<RawAttendeeStatus>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawEmailAddress {
    #[serde(default)]
    pub(crate) address: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAttendeeStatus {
    #[serde(default)]
    pub(crate) response: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawOrganizer {
    #[serde(rename = "emailAddress")]
    pub(crate) email_address: Option<RawEmailAddress>,
}

// ----- Mapping -----------------------------------------------------------

pub(crate) fn map_event(
    connector_id: &str,
    raw: RawEvent,
    resolver: &AttendeeResolver<'_>,
    self_email: Option<&str>,
) -> CalendarEvent {
    let title = raw.subject.unwrap_or_default();
    let title = if title.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        title
    };

    let start_ms = raw
        .start
        .as_ref()
        .and_then(|d| iso_to_ms(&d.date_time))
        .unwrap_or(0);
    let end_ms = raw
        .end
        .as_ref()
        .and_then(|d| iso_to_ms(&d.date_time))
        .unwrap_or(start_ms);

    let location = raw.location.and_then(|l| l.display_name);
    let description = raw.body_preview;
    let status = Some(if raw.is_cancelled { "cancelled" } else { "confirmed" }.to_string());
    let modified_ms = raw
        .last_modified
        .as_deref()
        .and_then(iso_to_ms)
        .unwrap_or_else(current_unix_ms);

    // Attendees: organizer first, then other attendees deduped by email.
    let mut attendees: Vec<CalendarAttendee> = Vec::new();
    let mut seen_emails: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Some(org) = raw.organizer.as_ref().and_then(|o| o.email_address.as_ref()) {
        if let Some(email) = org.address.as_deref() {
            let email_lc = email.to_lowercase();
            if !email_lc.is_empty() && seen_emails.insert(email_lc.clone()) {
                attendees.push(CalendarAttendee {
                    email: email_lc.clone(),
                    display_name: org.name.clone(),
                    response_status: Some("organizer".to_string()),
                    is_self: self_email.map(|e| e.eq_ignore_ascii_case(&email_lc)).unwrap_or(false),
                    is_organizer: true,
                    team_member_id: resolver.resolve_attendee(&email_lc, org.name.as_deref()),
                });
            }
        }
    }

    for raw_a in raw.attendees {
        let email_addr = match raw_a.email_address {
            Some(e) => e,
            None => continue,
        };
        let email = match email_addr.address {
            Some(e) if !e.is_empty() => e.to_lowercase(),
            _ => continue,
        };
        if !seen_emails.insert(email.clone()) {
            // Already added (organizer dup, or duplicate attendee row).
            continue;
        }
        attendees.push(CalendarAttendee {
            email: email.clone(),
            display_name: email_addr.name,
            response_status: raw_a.status.and_then(|s| s.response),
            is_self: self_email.map(|e| e.eq_ignore_ascii_case(&email)).unwrap_or(false),
            is_organizer: false,
            team_member_id: resolver.resolve_attendee(&email, None),
        });
    }

    CalendarEvent {
        id: format!("{connector_id}::{}", raw.id),
        connector_id: connector_id.to_string(),
        external_id: raw.id,
        title,
        start_ms,
        end_ms,
        all_day: raw.is_all_day,
        location,
        description,
        source_calendar: None, // primary calendar only in v1
        status,
        raw_etag: raw.etag,
        modified_ms,
        // Connector never sets the link — only the user does, via the
        // "Coming up" strip click handler. `upsert_event` preserves
        // any existing value across re-syncs.
        linked_note_id: None,
        attendees,
    }
}

// ----- Mail: API client + JSON shape + mapping ----------------------------

async fn fetch_inbox_messages(access_token: &str) -> Result<Vec<RawMessage>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;

    let mut url = format!(
        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages\
         ?$top={MAIL_PAGE_SIZE}\
         &$orderby=sentDateTime%20desc\
         &$select=id,conversationId,subject,from,toRecipients,ccRecipients,bccRecipients,bodyPreview,sentDateTime,hasAttachments,isRead,lastModifiedDateTime"
    );

    let mut all: Vec<RawMessage> = Vec::new();
    let mut pages = 0usize;

    loop {
        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("graph GET inbox: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, body));
        }

        let page: GraphMessagePage = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("graph mail parse: {e}")))?;
        all.extend(page.value);

        pages += 1;
        if pages >= MAIL_MAX_PAGES {
            break;
        }
        match page.next_link {
            Some(next) if !next.is_empty() => url = next,
            _ => break,
        }
    }

    Ok(all)
}

#[derive(Debug, Deserialize)]
struct GraphMessagePage {
    value: Vec<RawMessage>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMessage {
    pub(crate) id: String,
    #[serde(rename = "conversationId", default)]
    pub(crate) conversation_id: Option<String>,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(default)]
    pub(crate) from: Option<RawRecipientWrapper>,
    #[serde(rename = "toRecipients", default)]
    pub(crate) to_recipients: Vec<RawRecipientWrapper>,
    #[serde(rename = "ccRecipients", default)]
    pub(crate) cc_recipients: Vec<RawRecipientWrapper>,
    #[serde(rename = "bccRecipients", default)]
    pub(crate) bcc_recipients: Vec<RawRecipientWrapper>,
    #[serde(rename = "bodyPreview", default)]
    pub(crate) body_preview: Option<String>,
    #[serde(rename = "sentDateTime", default)]
    pub(crate) sent_date_time: Option<String>,
    #[serde(rename = "hasAttachments", default)]
    pub(crate) has_attachments: bool,
    #[serde(rename = "isRead", default)]
    pub(crate) is_read: bool,
    #[serde(rename = "lastModifiedDateTime", default)]
    pub(crate) last_modified: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawRecipientWrapper {
    #[serde(rename = "emailAddress", default)]
    pub(crate) email_address: Option<RawEmailAddress>,
}

#[derive(Debug, Deserialize)]
struct RawMessageBody {
    body: Option<RawBody>,
}

#[derive(Debug, Deserialize)]
struct RawBody {
    content: Option<String>,
}

/// Map a Graph message into the provider-agnostic `EmailMessage`.
/// Returns `None` for messages we can't represent (no `from`, no
/// sentDateTime — system-generated noise we'd otherwise persist with
/// blank fields and a 1970 timestamp).
pub(crate) fn map_message(
    connector_id: &str,
    raw: RawMessage,
    resolver: &AttendeeResolver<'_>,
) -> Option<EmailMessage> {
    let from_addr = raw
        .from
        .as_ref()
        .and_then(|w| w.email_address.as_ref())
        .and_then(|a| a.address.as_deref())
        .filter(|s| !s.is_empty())?;
    let from_email = from_addr.to_lowercase();
    let from_name = raw
        .from
        .as_ref()
        .and_then(|w| w.email_address.as_ref())
        .and_then(|a| a.name.clone());

    let subject = match raw.subject {
        Some(s) if !s.trim().is_empty() => s,
        _ => "(no subject)".to_string(),
    };

    let sent_at_ms = raw
        .sent_date_time
        .as_deref()
        .and_then(iso_to_ms)
        .unwrap_or(0);
    let modified_ms = raw
        .last_modified
        .as_deref()
        .and_then(iso_to_ms)
        .unwrap_or(sent_at_ms);

    let thread_id = raw.conversation_id.unwrap_or_else(|| raw.id.clone());

    let mut recipients: Vec<EmailRecipient> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for (kind, list) in [
        ("to", raw.to_recipients),
        ("cc", raw.cc_recipients),
        ("bcc", raw.bcc_recipients),
    ] {
        for r in list {
            let addr = match r.email_address {
                Some(a) => a,
                None => continue,
            };
            let email = match addr.address {
                Some(e) if !e.is_empty() => e.to_lowercase(),
                _ => continue,
            };
            if !seen.insert((email.clone(), kind.to_string())) {
                continue;
            }
            recipients.push(EmailRecipient {
                team_member_id: resolver.resolve_attendee(&email, addr.name.as_deref()),
                email,
                display_name: addr.name,
                recipient_type: kind.to_string(),
            });
        }
    }

    Some(EmailMessage {
        id: format!("{connector_id}::{}", raw.id),
        connector_id: connector_id.to_string(),
        external_id: raw.id,
        thread_id,
        subject,
        from_email,
        from_name,
        sent_at_ms,
        body_preview: raw.body_preview.filter(|s| !s.is_empty()),
        body_html: None,
        has_attachments: raw.has_attachments,
        is_read: raw.is_read,
        raw_etag: None,
        modified_ms,
        recipients,
    })
}

/// Percent-encode characters in a Microsoft Graph message ID for use
/// as a URL path segment. Graph IDs use URL-safe Base64-ish strings
/// that may include `=`, `/`, and `+`, none of which are valid
/// unescaped in a path segment.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

// ----- Attendee resolver -------------------------------------------------

/// Resolve a person identifier (email, GitHub login, Slack id, …) to a
/// `team_member_id`, if Margin knows the person. Each alias kind has
/// its own lookup map built from the typed `team_member_aliases` rows
/// (#87). Connectors call the per-kind methods directly; the email +
/// calendar pipelines use the convenience `resolve_attendee` shim that
/// preserves the existing email-then-name fallback.
pub(crate) struct AttendeeResolver<'a> {
    by_email: HashMap<String, String>,        // email_lc → team_member_id
    by_github_login: HashMap<String, String>, // login_lc → team_member_id
    by_slack_id: HashMap<String, String>,     // slack_id → team_member_id (case-preserved)
    by_name: OwnerResolver,
    _members: &'a [TeamMember],
}

impl<'a> AttendeeResolver<'a> {
    pub(crate) fn new(members: &'a [TeamMember]) -> Self {
        let mut by_email: HashMap<String, String> = HashMap::new();
        let mut by_github_login: HashMap<String, String> = HashMap::new();
        let mut by_slack_id: HashMap<String, String> = HashMap::new();
        for m in members {
            // Some teams put the email itself as display_name; treat
            // that as an email key too — preserves prior behavior.
            if m.display_name.contains('@') {
                by_email.insert(m.display_name.to_lowercase(), m.id.clone());
            }
            for alias in &m.aliases {
                match alias.kind.as_str() {
                    team::kinds::EMAIL => {
                        by_email.insert(alias.value.to_lowercase(), m.id.clone());
                    }
                    team::kinds::GITHUB_LOGIN => {
                        by_github_login.insert(alias.value.to_lowercase(), m.id.clone());
                    }
                    team::kinds::SLACK_ID => {
                        // Slack ids are case-significant ("U0ABCDE")
                        // but practically uppercase; index as-is.
                        by_slack_id.insert(alias.value.clone(), m.id.clone());
                    }
                    // `name` aliases live in `OwnerResolver`; other
                    // unknown kinds are dropped here and re-added when
                    // a future resolver method is registered.
                    _ => {}
                }
            }
        }
        Self {
            by_email,
            by_github_login,
            by_slack_id,
            by_name: OwnerResolver::from_members(members),
            _members: members,
        }
    }

    pub(crate) fn resolve_by_email(&self, email_lc: &str) -> Option<String> {
        self.by_email.get(email_lc).cloned()
    }

    pub(crate) fn resolve_by_name(&self, name: &str) -> Option<String> {
        self.by_name.resolve(name)
    }

    #[allow(dead_code)]
    pub(crate) fn resolve_by_github_login(&self, login: &str) -> Option<String> {
        self.by_github_login.get(&login.to_lowercase()).cloned()
    }

    #[allow(dead_code)]
    pub(crate) fn resolve_by_slack_id(&self, slack_id: &str) -> Option<String> {
        self.by_slack_id.get(slack_id).cloned()
    }

    /// Combined helper preserving the existing email-then-name dispatch
    /// used by every email/calendar connector. New connectors that
    /// don't carry both kinds should call the per-kind methods directly.
    pub(crate) fn resolve_attendee(
        &self,
        email_lc: &str,
        display_name: Option<&str>,
    ) -> Option<String> {
        if let Some(id) = self.resolve_by_email(email_lc) {
            return Some(id);
        }
        if let Some(name) = display_name {
            if let Some(id) = self.resolve_by_name(name) {
                return Some(id);
            }
        }
        None
    }
}

// ----- Time helpers ------------------------------------------------------

fn iso_to_ms(s: &str) -> Option<i64> {
    // Microsoft sends `dateTime` like "2026-05-12T09:30:00.0000000"
    // (no timezone suffix when Prefer outlook.timezone is UTC) OR
    // RFC 3339 like "2026-05-09T12:34:56Z" for lastModifiedDateTime.
    // Try both: first as RFC 3339 (with Z), fall back to assuming UTC.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).timestamp_millis());
    }
    // No timezone suffix: parse as naive then attach UTC.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(Utc.from_utc_datetime(&naive).timestamp_millis());
    }
    None
}

fn ms_to_iso(ms: i64) -> String {
    let secs = ms / 1000;
    let nsec = ((ms % 1000) * 1_000_000) as u32;
    DateTime::<Utc>::from_timestamp(secs, nsec)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

use chrono::TimeZone as _;

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_member(id: &str, name: &str, aliases: &[(&str, &str)]) -> TeamMember {
        TeamMember {
            id: id.to_string(),
            display_name: name.to_string(),
            role: String::new(),
            aliases: aliases
                .iter()
                .map(|(k, v)| team::TypedAlias {
                    kind: (*k).to_string(),
                    value: (*v).to_string(),
                })
                .collect(),
            is_self: false,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn iso_to_ms_handles_rfc3339_and_utc_naive() {
        let ms_z = iso_to_ms("2026-05-12T09:30:00Z").unwrap();
        let ms_naive = iso_to_ms("2026-05-12T09:30:00.0000000").unwrap();
        assert_eq!(ms_z, ms_naive);
    }

    #[test]
    fn map_event_basic() {
        let raw = RawEvent {
            id: "AAMkA1".to_string(),
            subject: Some("Standup".to_string()),
            start: Some(RawDateTime {
                date_time: "2026-05-12T09:30:00.0000000".to_string(),
                time_zone: Some("UTC".to_string()),
            }),
            end: Some(RawDateTime {
                date_time: "2026-05-12T10:00:00.0000000".to_string(),
                time_zone: Some("UTC".to_string()),
            }),
            is_all_day: false,
            is_cancelled: false,
            location: Some(RawLocation {
                display_name: Some("Zoom".to_string()),
            }),
            body_preview: Some("Quarterly metrics".to_string()),
            attendees: vec![RawAttendee {
                email_address: Some(RawEmailAddress {
                    address: Some("Heike@Example.com".to_string()),
                    name: Some("Heike Müller".to_string()),
                }),
                status: Some(RawAttendeeStatus {
                    response: Some("accepted".to_string()),
                }),
            }],
            organizer: Some(RawOrganizer {
                email_address: Some(RawEmailAddress {
                    address: Some("tj@example.com".to_string()),
                    name: Some("Tom".to_string()),
                }),
            }),
            etag: Some("W/\"abc\"".to_string()),
            last_modified: Some("2026-05-09T12:34:56Z".to_string()),
        };
        let members = [mk_member("hk1", "Heike", &[("email", "heike@example.com")])];
        let resolver = AttendeeResolver::new(&members);
        let ev = map_event("microsoft_graph:tj@example.com", raw, &resolver, Some("tj@example.com"));

        assert_eq!(ev.title, "Standup");
        assert_eq!(ev.location.as_deref(), Some("Zoom"));
        assert_eq!(ev.status.as_deref(), Some("confirmed"));
        assert_eq!(ev.attendees.len(), 2);

        // Organizer first, lowercased email, marked is_self.
        let org = &ev.attendees[0];
        assert_eq!(org.email, "tj@example.com");
        assert!(org.is_organizer);
        assert!(org.is_self);

        // Heike was resolved via her email alias.
        let heike = &ev.attendees[1];
        assert_eq!(heike.email, "heike@example.com");
        assert_eq!(heike.team_member_id.as_deref(), Some("hk1"));
        assert!(!heike.is_organizer);
    }

    #[test]
    fn map_event_treats_cancelled() {
        let mut raw = RawEvent {
            id: "x".into(),
            subject: Some("Was a meeting".into()),
            start: Some(RawDateTime {
                date_time: "2026-05-12T09:00:00".into(),
                time_zone: None,
            }),
            end: Some(RawDateTime {
                date_time: "2026-05-12T10:00:00".into(),
                time_zone: None,
            }),
            is_all_day: false,
            is_cancelled: true,
            location: None,
            body_preview: None,
            attendees: vec![],
            organizer: None,
            etag: None,
            last_modified: None,
        };
        raw.is_cancelled = true;
        let resolver = AttendeeResolver::new(&[]);
        let ev = map_event("mg:tj@e.com", raw, &resolver, None);
        assert_eq!(ev.status.as_deref(), Some("cancelled"));
    }

    #[test]
    fn map_event_default_subject_when_missing() {
        let raw = RawEvent {
            id: "y".into(),
            subject: None,
            start: Some(RawDateTime {
                date_time: "2026-05-12T09:00:00".into(),
                time_zone: None,
            }),
            end: Some(RawDateTime {
                date_time: "2026-05-12T10:00:00".into(),
                time_zone: None,
            }),
            is_all_day: false,
            is_cancelled: false,
            location: None,
            body_preview: None,
            attendees: vec![],
            organizer: None,
            etag: None,
            last_modified: None,
        };
        let resolver = AttendeeResolver::new(&[]);
        let ev = map_event("mg:tj@e.com", raw, &resolver, None);
        assert_eq!(ev.title, "(no subject)");
    }

    #[test]
    fn attendee_resolver_email_match() {
        let members = [mk_member("hk1", "Heike Müller", &[("email", "heike@contoso.com")])];
        let r = AttendeeResolver::new(&members);
        assert_eq!(r.resolve_attendee("heike@contoso.com", None).as_deref(), Some("hk1"));
        // Case-insensitive
        assert_eq!(r.resolve_attendee("Heike@CONTOSO.com".to_lowercase().as_str(), None).as_deref(), Some("hk1"));
    }

    #[test]
    fn attendee_resolver_name_fallback() {
        let members = [mk_member("hk1", "Heike Müller", &[])];
        let r = AttendeeResolver::new(&members);
        // Email not registered as alias.
        assert_eq!(r.resolve_attendee("unknown@x.com", Some("Heike Müller")).as_deref(), Some("hk1"));
    }

    #[test]
    fn attendee_resolver_no_match_returns_none() {
        let r = AttendeeResolver::new(&[]);
        assert!(r.resolve_attendee("nobody@nope.com", Some("Nobody")).is_none());
    }

    #[test]
    fn attendee_resolver_resolves_by_github_login() {
        let members = [mk_member(
            "hk1",
            "Heike Müller",
            &[("github_login", "heike-mueller")],
        )];
        let r = AttendeeResolver::new(&members);
        assert_eq!(r.resolve_by_github_login("heike-mueller").as_deref(), Some("hk1"));
        // Case-insensitive
        assert_eq!(r.resolve_by_github_login("Heike-Mueller").as_deref(), Some("hk1"));
        // Email map untouched by a github_login alias.
        assert!(r.resolve_by_email("heike-mueller").is_none());
    }

    #[test]
    fn attendee_resolver_resolves_by_slack_id() {
        let members = [mk_member("hk1", "Heike Müller", &[("slack_id", "U0ABCDE12")])];
        let r = AttendeeResolver::new(&members);
        assert_eq!(r.resolve_by_slack_id("U0ABCDE12").as_deref(), Some("hk1"));
        // Slack ids are case-significant; lowercase shouldn't match.
        assert!(r.resolve_by_slack_id("u0abcde12").is_none());
    }

    #[test]
    fn attendee_resolver_kinds_are_isolated() {
        // A `name` alias whose value matches an email local part should
        // not leak into the email map (#87).
        let members = [mk_member("hk1", "Heike Müller", &[("name", "heike")])];
        let r = AttendeeResolver::new(&members);
        assert!(r.resolve_by_email("heike").is_none(), "no email alias was registered");
        assert_eq!(r.resolve_by_name("heike").as_deref(), Some("hk1"));
    }

    fn mk_recip(email: &str, name: Option<&str>) -> RawRecipientWrapper {
        RawRecipientWrapper {
            email_address: Some(RawEmailAddress {
                address: Some(email.to_string()),
                name: name.map(|s| s.to_string()),
            }),
        }
    }

    #[test]
    fn map_message_basic() {
        let raw = RawMessage {
            id: "MSG1".into(),
            conversation_id: Some("THREAD1".into()),
            subject: Some("Roadmap review".into()),
            from: Some(mk_recip("alice@Example.com", Some("Alice"))),
            to_recipients: vec![mk_recip("tj@example.com", Some("TJ"))],
            cc_recipients: vec![mk_recip("bob@example.com", Some("Bob"))],
            bcc_recipients: vec![],
            body_preview: Some("Let's align on…".into()),
            sent_date_time: Some("2026-05-09T12:34:56Z".into()),
            has_attachments: true,
            is_read: false,
            last_modified: Some("2026-05-09T12:35:00Z".into()),
        };
        let resolver = AttendeeResolver::new(&[]);
        let m = map_message("microsoft_graph:tj@example.com", raw, &resolver).unwrap();

        assert_eq!(m.id, "microsoft_graph:tj@example.com::MSG1");
        assert_eq!(m.thread_id, "THREAD1");
        assert_eq!(m.subject, "Roadmap review");
        assert_eq!(m.from_email, "alice@example.com");
        assert_eq!(m.from_name.as_deref(), Some("Alice"));
        assert!(m.has_attachments);
        assert!(!m.is_read);
        assert_eq!(m.recipients.len(), 2);
        assert_eq!(m.recipients[0].email, "tj@example.com");
        assert_eq!(m.recipients[0].recipient_type, "to");
        assert_eq!(m.recipients[1].email, "bob@example.com");
        assert_eq!(m.recipients[1].recipient_type, "cc");
    }

    #[test]
    fn map_message_no_recipients() {
        let raw = RawMessage {
            id: "MSG2".into(),
            conversation_id: Some("THREAD2".into()),
            subject: Some("Release notes".into()),
            from: Some(mk_recip("notifications@github.com", None)),
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            body_preview: None,
            sent_date_time: Some("2026-05-09T12:00:00Z".into()),
            has_attachments: false,
            is_read: true,
            last_modified: None,
        };
        let resolver = AttendeeResolver::new(&[]);
        let m = map_message("mg:tj", raw, &resolver).unwrap();
        assert_eq!(m.recipients.len(), 0);
        assert!(m.is_read);
    }

    #[test]
    fn map_message_default_subject_when_missing() {
        let raw = RawMessage {
            id: "MSG3".into(),
            conversation_id: None,
            subject: Some("   ".into()),
            from: Some(mk_recip("a@b.com", None)),
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            body_preview: None,
            sent_date_time: None,
            has_attachments: false,
            is_read: false,
            last_modified: None,
        };
        let resolver = AttendeeResolver::new(&[]);
        let m = map_message("mg:x", raw, &resolver).unwrap();
        assert_eq!(m.subject, "(no subject)");
        // Falls back to message id when conversation_id missing.
        assert_eq!(m.thread_id, "MSG3");
    }

    #[test]
    fn map_message_returns_none_when_from_missing() {
        let raw = RawMessage {
            id: "MSG4".into(),
            conversation_id: None,
            subject: Some("???".into()),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            body_preview: None,
            sent_date_time: None,
            has_attachments: false,
            is_read: false,
            last_modified: None,
        };
        let resolver = AttendeeResolver::new(&[]);
        assert!(map_message("mg:x", raw, &resolver).is_none());
    }

    #[test]
    fn map_message_resolves_recipients_via_team() {
        let members = [mk_member("hk1", "Heike Müller", &[("email", "heike@contoso.com")])];
        let resolver = AttendeeResolver::new(&members);
        let raw = RawMessage {
            id: "MSG5".into(),
            conversation_id: Some("THREAD5".into()),
            subject: Some("Hi".into()),
            from: Some(mk_recip("alice@e.com", None)),
            to_recipients: vec![mk_recip("Heike@CONTOSO.com", Some("Heike"))],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            body_preview: None,
            sent_date_time: Some("2026-05-09T12:00:00Z".into()),
            has_attachments: false,
            is_read: false,
            last_modified: None,
        };
        let m = map_message("mg:tj", raw, &resolver).unwrap();
        assert_eq!(m.recipients.len(), 1);
        assert_eq!(m.recipients[0].email, "heike@contoso.com");
        assert_eq!(m.recipients[0].team_member_id.as_deref(), Some("hk1"));
    }

    #[test]
    fn percent_encode_path_handles_graph_id_chars() {
        let encoded = percent_encode_path("AAMkA+/=foo");
        assert_eq!(encoded, "AAMkA%2B%2F%3Dfoo");
    }

    #[test]
    fn map_status_treats_403_as_reauth_needed() {
        let err = map_status(
            reqwest::StatusCode::FORBIDDEN,
            None,
            "ErrorAccessDenied".into(),
        );
        match err {
            ConnectorError::ReauthNeeded(msg) => {
                assert!(msg.contains("403"));
                assert!(msg.contains("ErrorAccessDenied"));
            }
            other => panic!("expected ReauthNeeded, got {other:?}"),
        }
    }
}
