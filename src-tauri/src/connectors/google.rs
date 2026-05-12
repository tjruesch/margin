//! Google connector (#61) — Calendar v3 + Gmail v1 under a single
//! `google` kind. Mirrors the structural shape of `microsoft_graph.rs`
//! so a future provider lands as another file in this directory.
//!
//! Calendar window: last 14 days through next 30 days (matches Microsoft).
//! Mail: 200 most recent inbox messages per sync, headers-only; bodies
//! lazy-fetched via the `Connector::fetch_message_body` trait method
//! when the user opens the email in the UI.
//!
//! Polled every 5 minutes by the `SyncRunner` (same cadence as Microsoft).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Deserialize;

use super::calendar::{CalendarAttendee, CalendarEvent};
use super::email::{EmailMessage, EmailRecipient};
use super::microsoft_graph::AttendeeResolver;
use super::oauth::with_valid_token;
use super::registry::ConnectorRegistry;
use super::{Connector, ConnectorError, ConnectorRow, SyncCtx, SyncReport};
use crate::team;

const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const WINDOW_FORWARD_MS: i64 = 30 * 24 * 3600 * 1000;
const CALENDAR_PAGE_SIZE: u32 = 250;

/// Inbox cap per sync. Mail accumulates across syncs so older messages
/// don't fall off — matches Microsoft's per-sync ceiling.
const MAIL_PAGE_SIZE: u32 = 50;
const MAIL_MAX_PAGES: usize = 4;
/// Concurrent in-flight metadata fetches when expanding the inbox
/// list into per-message metadata. Bounded so we don't open 200
/// sockets to gmail.googleapis.com.
const MAIL_FETCH_CONCURRENCY: usize = 10;

const KIND: &str = "google";

pub struct GoogleConnector {
    id: String,
    kind: String,
    display_name: String,
}

impl GoogleConnector {
    pub fn new(row: &ConnectorRow) -> Self {
        Self {
            id: row.id.clone(),
            kind: row.kind.clone(),
            display_name: row.display_name.clone(),
        }
    }

    /// Email portion of the connector_id (`google:<email>`). Used to
    /// flag the corresponding attendee row as `is_self`.
    fn self_email(&self) -> Option<&str> {
        self.id.split_once(':').map(|(_, email)| email)
    }
}

#[async_trait::async_trait]
impl Connector for GoogleConnector {
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
        let raw_events =
            with_valid_token(ctx.app, &self.id, &self.kind, |access| async move {
                fetch_calendar_events(&access, window_start, window_end).await
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
        // Same partial-success policy as Microsoft: 401/403 propagates so
        // Settings shows Reconnect; transient errors log + skip so the
        // calendar half still wins.
        let mail_report = match self.sync_mail(&ctx, &resolver).await {
            Ok(report) => report,
            Err(ConnectorError::ReauthNeeded(msg)) => {
                eprintln!("[google] mail sync needs reauth: {msg}");
                return Err(ConnectorError::ReauthNeeded(msg));
            }
            Err(e) => {
                eprintln!("[google] mail sync failed (non-fatal): {e}");
                super::email::UpsertReport::default()
            }
        };

        Ok(SyncReport {
            added: calendar_report.added + mail_report.added,
            updated: calendar_report.updated + mail_report.updated,
            removed: calendar_report.removed,
            skipped: mail_report.skipped,
        })
    }

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

impl GoogleConnector {
    async fn sync_mail(
        &self,
        ctx: &SyncCtx<'_>,
        resolver: &AttendeeResolver<'_>,
    ) -> Result<super::email::UpsertReport, ConnectorError> {
        let raw_messages =
            with_valid_token(ctx.app, &self.id, &self.kind, |access| async move {
                fetch_gmail_messages(&access).await
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
}

pub fn register(registry: &ConnectorRegistry) {
    registry.register_kind(
        KIND,
        Arc::new(|row, _app| {
            Ok(Arc::new(GoogleConnector::new(row)) as Arc<dyn Connector>)
        }),
    );
}

// ----- Calendar API client ------------------------------------------------

async fn fetch_calendar_events(
    access_token: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<RawEvent>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;

    let start_iso = ms_to_iso(start_ms);
    let end_iso = ms_to_iso(end_ms);
    let mut all = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events\
             ?timeMin={start_iso}&timeMax={end_iso}\
             &singleEvents=true&orderBy=startTime&maxResults={CALENDAR_PAGE_SIZE}"
        );
        if let Some(token) = page_token.as_ref() {
            url.push_str("&pageToken=");
            url.push_str(token);
        }

        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("calendar GET: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, body));
        }

        let page: CalendarPage = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("calendar parse: {e}")))?;
        all.extend(page.items);
        match page.next_page_token {
            Some(t) if !t.is_empty() => page_token = Some(t),
            _ => break,
        }
    }

    Ok(all)
}

#[derive(Debug, Deserialize)]
struct CalendarPage {
    #[serde(default)]
    items: Vec<RawEvent>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    id: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    start: Option<RawTime>,
    #[serde(default)]
    end: Option<RawTime>,
    #[serde(default)]
    attendees: Vec<RawAttendee>,
    #[serde(default)]
    organizer: Option<RawPerson>,
    #[serde(default)]
    updated: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTime {
    #[serde(rename = "dateTime", default)]
    date_time: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPerson {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAttendee {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "responseStatus", default)]
    response_status: Option<String>,
    #[serde(default)]
    organizer: Option<bool>,
    #[serde(rename = "self", default)]
    is_self: Option<bool>,
}

fn map_event(
    connector_id: &str,
    raw: RawEvent,
    resolver: &AttendeeResolver<'_>,
    self_email: Option<&str>,
) -> CalendarEvent {
    let title = raw.summary.unwrap_or_default();
    let title = if title.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        title
    };

    // start/end can be either dateTime (timed) or date (all-day).
    let (start_ms, all_day_start) = parse_event_time(&raw.start);
    let (end_ms_raw, all_day_end) = parse_event_time(&raw.end);
    let end_ms = if end_ms_raw == 0 { start_ms } else { end_ms_raw };
    let all_day = all_day_start || all_day_end;

    let status = raw
        .status
        .map(|s| if s == "cancelled" { "cancelled".to_string() } else { "confirmed".to_string() })
        .or(Some("confirmed".to_string()));

    let modified_ms = raw
        .updated
        .as_deref()
        .and_then(iso_to_ms)
        .unwrap_or_else(current_unix_ms);

    let mut attendees: Vec<CalendarAttendee> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Organizer first if present and resolves.
    if let Some(org) = raw.organizer.as_ref() {
        if let Some(email) = org.email.as_deref() {
            let email_lc = email.to_lowercase();
            if !email_lc.is_empty() && seen.insert(email_lc.clone()) {
                attendees.push(CalendarAttendee {
                    email: email_lc.clone(),
                    display_name: org.display_name.clone(),
                    response_status: Some("organizer".to_string()),
                    is_self: self_email
                        .map(|e| e.eq_ignore_ascii_case(&email_lc))
                        .unwrap_or(false),
                    is_organizer: true,
                    team_member_id: resolver.resolve_attendee(&email_lc, org.display_name.as_deref()),
                });
            }
        }
    }

    for raw_a in raw.attendees {
        let email = match raw_a.email.as_deref() {
            Some(e) if !e.is_empty() => e.to_lowercase(),
            _ => continue,
        };
        if !seen.insert(email.clone()) {
            continue;
        }
        let is_organizer = raw_a.organizer.unwrap_or(false);
        let is_self = raw_a.is_self.unwrap_or(false)
            || self_email
                .map(|e| e.eq_ignore_ascii_case(&email))
                .unwrap_or(false);
        attendees.push(CalendarAttendee {
            email: email.clone(),
            display_name: raw_a.display_name.clone(),
            response_status: raw_a.response_status,
            is_self,
            is_organizer,
            team_member_id: resolver.resolve_attendee(&email, raw_a.display_name.as_deref()),
        });
    }

    CalendarEvent {
        id: format!("{connector_id}::{}", raw.id),
        connector_id: connector_id.to_string(),
        external_id: raw.id,
        title,
        start_ms,
        end_ms,
        all_day,
        location: raw.location,
        description: raw.description,
        source_calendar: None, // primary calendar only in v1
        status,
        raw_etag: raw.etag,
        modified_ms,
        // User-set on first click of the "Coming up" strip; persisted
        // by `upsert_event`'s ON CONFLICT clause.
        linked_note_id: None,
        attendees,
    }
}

/// Parse a Google Calendar `start` / `end` block.
/// Returns `(unix_ms, was_all_day)`.
fn parse_event_time(t: &Option<RawTime>) -> (i64, bool) {
    let Some(t) = t else { return (0, false); };
    if let Some(dt) = t.date_time.as_deref() {
        return (iso_to_ms(dt).unwrap_or(0), false);
    }
    if let Some(date) = t.date.as_deref() {
        // YYYY-MM-DD interpreted as midnight UTC.
        return (date_to_ms(date).unwrap_or(0), true);
    }
    (0, false)
}

// ----- Gmail API client ---------------------------------------------------

async fn fetch_gmail_messages(access_token: &str) -> Result<Vec<RawMessage>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;

    // Phase 1: list message IDs from the inbox.
    let mut ids: Vec<String> = Vec::new();
    let mut page_token: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let mut url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages\
             ?labelIds=INBOX&maxResults={MAIL_PAGE_SIZE}"
        );
        if let Some(t) = page_token.as_ref() {
            url.push_str("&pageToken=");
            url.push_str(t);
        }
        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| ConnectorError::Network(format!("gmail list: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, retry_after, body));
        }
        let page: MessageIdPage = resp
            .json()
            .await
            .map_err(|e| ConnectorError::Other(format!("gmail list parse: {e}")))?;
        ids.extend(page.messages.into_iter().map(|m| m.id));
        pages += 1;
        if pages >= MAIL_MAX_PAGES {
            break;
        }
        match page.next_page_token {
            Some(t) if !t.is_empty() => page_token = Some(t),
            _ => break,
        }
    }

    // Phase 2: per-id metadata fetch with bounded concurrency.
    let client_arc = Arc::new(client);
    let token_arc: Arc<String> = Arc::new(access_token.to_string());
    let mut in_flight = FuturesUnordered::new();
    let mut iter = ids.into_iter();
    for _ in 0..MAIL_FETCH_CONCURRENCY {
        if let Some(id) = iter.next() {
            in_flight.push(fetch_message_metadata(client_arc.clone(), token_arc.clone(), id));
        }
    }
    let mut out: Vec<RawMessage> = Vec::new();
    while let Some(result) = in_flight.next().await {
        match result {
            Ok(msg) => out.push(msg),
            Err(ConnectorError::ReauthNeeded(_)) => {
                // Propagate immediately — no point continuing 199 more
                // requests that will all 401 too.
                return Err(result.unwrap_err());
            }
            Err(e) => {
                eprintln!("[google] gmail metadata fetch failed: {e}");
                // Skip this message; don't fail the whole sync.
            }
        }
        if let Some(id) = iter.next() {
            in_flight.push(fetch_message_metadata(client_arc.clone(), token_arc.clone(), id));
        }
    }
    Ok(out)
}

async fn fetch_message_metadata(
    client: Arc<reqwest::Client>,
    token: Arc<String>,
    id: String,
) -> Result<RawMessage, ConnectorError> {
    let url = format!(
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}\
         ?format=metadata\
         &metadataHeaders=From\
         &metadataHeaders=To\
         &metadataHeaders=Cc\
         &metadataHeaders=Bcc\
         &metadataHeaders=Subject\
         &metadataHeaders=Date"
    );
    let resp = client
        .get(&url)
        .bearer_auth(token.as_str())
        .send()
        .await
        .map_err(|e| ConnectorError::Network(format!("gmail metadata: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(map_status(status, retry_after, body));
    }
    let parsed: RawMessage = resp
        .json()
        .await
        .map_err(|e| ConnectorError::Other(format!("gmail metadata parse: {e}")))?;
    Ok(parsed)
}

/// Lazy body fetch via `messages.get?format=full`. Walks the MIME tree
/// and returns the first `text/html` part (falling back to `text/plain`).
pub async fn fetch_message_body(
    access_token: &str,
    external_id: &str,
) -> Result<Option<String>, ConnectorError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ConnectorError::Network(format!("client init: {e}")))?;
    let url = format!(
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/{external_id}?format=full"
    );
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| ConnectorError::Network(format!("gmail body: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(map_status(status, retry_after, body));
    }
    let parsed: RawMessageFull = resp
        .json()
        .await
        .map_err(|e| ConnectorError::Other(format!("gmail body parse: {e}")))?;
    Ok(parsed.payload.and_then(|p| extract_body_from_payload(&p)))
}

#[derive(Debug, Deserialize)]
struct MessageIdPage {
    #[serde(default)]
    messages: Vec<MessageIdRef>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageIdRef {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    id: String,
    #[serde(rename = "threadId", default)]
    thread_id: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(rename = "internalDate", default)]
    internal_date: Option<String>,
    #[serde(rename = "labelIds", default)]
    label_ids: Vec<String>,
    #[serde(default)]
    payload: Option<MessagePayloadHeaders>,
}

#[derive(Debug, Deserialize)]
struct MessagePayloadHeaders {
    #[serde(default)]
    headers: Vec<RawHeader>,
}

#[derive(Debug, Deserialize)]
struct RawHeader {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct RawMessageFull {
    #[serde(default)]
    payload: Option<MessagePayloadFull>,
}

#[derive(Debug, Deserialize)]
struct MessagePayloadFull {
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(default)]
    body: Option<MessageBody>,
    #[serde(default)]
    parts: Vec<MessagePayloadFull>,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    #[serde(default)]
    data: Option<String>, // base64url-encoded
}

fn map_message(
    connector_id: &str,
    raw: RawMessage,
    resolver: &AttendeeResolver<'_>,
) -> Option<EmailMessage> {
    let headers: HashMap<String, String> = raw
        .payload
        .as_ref()
        .map(|p| {
            p.headers
                .iter()
                .map(|h| (h.name.to_lowercase(), h.value.clone()))
                .collect()
        })
        .unwrap_or_default();

    // From — required to map at all.
    let from_raw = headers.get("from")?;
    let (from_email, from_name) = parse_address(from_raw)?;

    let subject = headers
        .get("subject")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no subject)".to_string());

    let sent_at_ms = raw
        .internal_date
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let thread_id = raw.thread_id.unwrap_or_else(|| raw.id.clone());

    let mut recipients: Vec<EmailRecipient> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for (header_key, kind) in [("to", "to"), ("cc", "cc"), ("bcc", "bcc")] {
        if let Some(raw_value) = headers.get(header_key) {
            for one in split_address_list(raw_value) {
                let Some((email, name)) = parse_address(&one) else { continue };
                if !seen.insert((email.clone(), kind.to_string())) {
                    continue;
                }
                recipients.push(EmailRecipient {
                    team_member_id: resolver.resolve_attendee(&email, name.as_deref()),
                    email,
                    display_name: name,
                    recipient_type: kind.to_string(),
                });
            }
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
        body_preview: raw.snippet.filter(|s| !s.is_empty()),
        body_html: None,
        has_attachments: false, // Gmail's metadata format doesn't surface this; lazy body fetch can update.
        is_read: !raw.label_ids.iter().any(|l| l == "UNREAD"),
        raw_etag: None,
        modified_ms: sent_at_ms,
        recipients,
    })
}

/// Parse a single RFC 5322 address: `"Name" <email>`, `Name <email>`,
/// or just `email`. Returns `(email_lc, optional_name)`.
fn parse_address(raw: &str) -> Option<(String, Option<String>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Look for "<email>" anywhere in the string.
    if let (Some(lt), Some(gt)) = (trimmed.find('<'), trimmed.rfind('>')) {
        if lt < gt {
            let email = trimmed[lt + 1..gt].trim().to_lowercase();
            if email.is_empty() {
                return None;
            }
            // Name is whatever's before the `<`, minus optional surrounding quotes.
            let name_raw = trimmed[..lt].trim();
            let name_clean = name_raw
                .trim_matches(|c| c == '"' || c == '\'')
                .trim()
                .to_string();
            let name = if name_clean.is_empty() {
                None
            } else {
                Some(name_clean)
            };
            return Some((email, name));
        }
    }

    // Bare email.
    let email = trimmed.to_lowercase();
    if !email.contains('@') {
        return None;
    }
    Some((email, None))
}

/// Split a header value (To, Cc, Bcc) on commas while respecting
/// quoted-string commas. Conservative — not a full RFC 5322 parser
/// but handles the common case where display names contain commas
/// (e.g. `"Doe, John" <jd@e.com>`).
fn split_address_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut depth_brackets = 0i32;
    for ch in raw.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '<' if !in_quotes => {
                depth_brackets += 1;
                current.push(ch);
            }
            '>' if !in_quotes => {
                depth_brackets -= 1;
                current.push(ch);
            }
            ',' if !in_quotes && depth_brackets <= 0 => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

/// Walk a Gmail message payload, returning the first text/html body
/// we find, falling back to text/plain. Bodies are base64-url
/// encoded; we decode + UTF-8 stringify.
fn extract_body_from_payload(payload: &MessagePayloadFull) -> Option<String> {
    fn decode(body: &MessageBody) -> Option<String> {
        let data = body.data.as_deref()?;
        // Gmail uses URL-safe base64 without padding.
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(data)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
            .ok()?;
        String::from_utf8(bytes).ok()
    }

    fn walk<'a>(
        node: &'a MessagePayloadFull,
        target_mime: &str,
    ) -> Option<&'a MessagePayloadFull> {
        if node
            .mime_type
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case(target_mime))
            .unwrap_or(false)
            && node
                .body
                .as_ref()
                .and_then(|b| b.data.as_ref())
                .is_some()
        {
            return Some(node);
        }
        for child in &node.parts {
            if let Some(found) = walk(child, target_mime) {
                return Some(found);
            }
        }
        None
    }

    if let Some(html_node) = walk(payload, "text/html") {
        if let Some(html) = html_node.body.as_ref().and_then(decode) {
            return Some(html);
        }
    }
    if let Some(text_node) = walk(payload, "text/plain") {
        if let Some(text) = text_node.body.as_ref().and_then(decode) {
            return Some(text);
        }
    }
    // Some single-part messages put the body directly on the payload
    // without `parts`, with the mime on the payload itself. Try that.
    if let Some(body) = payload.body.as_ref() {
        return decode(body);
    }
    None
}

// ----- HTTP error mapping -------------------------------------------------

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
        401 => ConnectorError::ReauthNeeded(format!("google 401: {body}")),
        // Google returns 403 for insufficient_scope / userinfo missing.
        // Treating it as a reconnect prompt mirrors Microsoft Graph.
        403 => ConnectorError::ReauthNeeded(format!("google 403: {body}")),
        429 => ConnectorError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(60_000),
        },
        503 => ConnectorError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(30_000),
        },
        _ => ConnectorError::Other(format!("google {status}: {body}")),
    }
}

// ----- Time helpers --------------------------------------------------------

fn iso_to_ms(s: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).timestamp_millis());
    }
    None
}

fn date_to_ms(s: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let naive = date.and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&naive).timestamp_millis())
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

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::TeamMember;

    fn mk_member(id: &str, name: &str, aliases: &[(&str, &str)]) -> TeamMember {
        TeamMember {
            id: id.to_string(),
            display_name: name.to_string(),
            role: String::new(),
            aliases: aliases
                .iter()
                .map(|(k, v)| crate::team::TypedAlias {
                    kind: (*k).to_string(),
                    value: (*v).to_string(),
                })
                .collect(),
            profile_md_path: String::new(),
            is_self: false,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    fn empty_resolver() -> AttendeeResolver<'static> {
        // Zero-length slice with 'static lifetime.
        AttendeeResolver::new(&[])
    }

    #[test]
    fn parse_address_with_quoted_name() {
        let (email, name) = parse_address("\"Heike Müller\" <heike@example.com>").unwrap();
        assert_eq!(email, "heike@example.com");
        assert_eq!(name.as_deref(), Some("Heike Müller"));
    }

    #[test]
    fn parse_address_with_unquoted_name() {
        let (email, name) = parse_address("Heike Müller <heike@example.com>").unwrap();
        assert_eq!(email, "heike@example.com");
        assert_eq!(name.as_deref(), Some("Heike Müller"));
    }

    #[test]
    fn parse_address_bare_email() {
        let (email, name) = parse_address("heike@example.com").unwrap();
        assert_eq!(email, "heike@example.com");
        assert_eq!(name, None);
    }

    #[test]
    fn parse_address_lowercases_email() {
        let (email, _) = parse_address("Heike@EXAMPLE.com").unwrap();
        assert_eq!(email, "heike@example.com");
    }

    #[test]
    fn parse_address_returns_none_for_empty() {
        assert!(parse_address("   ").is_none());
        assert!(parse_address("not-an-email").is_none());
    }

    #[test]
    fn split_address_list_handles_quoted_commas() {
        let parts = split_address_list("\"Doe, John\" <jd@e.com>, alice@e.com");
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("Doe, John"));
        assert_eq!(parts[1], "alice@e.com");
    }

    #[test]
    fn split_address_list_handles_angle_bracket_commas() {
        let parts =
            split_address_list("Bob <bob@e.com>, Sue <sue@e.com>, dan@e.com");
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn split_address_list_drops_empty_segments() {
        let parts = split_address_list("a@e.com, , b@e.com,");
        assert_eq!(parts, vec!["a@e.com", "b@e.com"]);
    }

    #[test]
    fn map_event_basic() {
        let raw = RawEvent {
            id: "evt1".into(),
            etag: Some("\"abc\"".into()),
            summary: Some("Standup".into()),
            description: Some("Quarterly metrics".into()),
            location: Some("Zoom".into()),
            status: Some("confirmed".into()),
            start: Some(RawTime {
                date_time: Some("2026-05-12T09:30:00Z".into()),
                date: None,
            }),
            end: Some(RawTime {
                date_time: Some("2026-05-12T10:00:00Z".into()),
                date: None,
            }),
            attendees: vec![RawAttendee {
                email: Some("Heike@Example.com".into()),
                display_name: Some("Heike Müller".into()),
                response_status: Some("accepted".into()),
                organizer: Some(false),
                is_self: Some(false),
            }],
            organizer: Some(RawPerson {
                email: Some("tj@example.com".into()),
                display_name: Some("Tom".into()),
            }),
            updated: Some("2026-05-09T12:00:00Z".into()),
        };
        let members = [mk_member("hk1", "Heike", &[("email", "heike@example.com")])];
        let resolver = AttendeeResolver::new(&members);
        let ev = map_event("google:tj@example.com", raw, &resolver, Some("tj@example.com"));

        assert_eq!(ev.title, "Standup");
        assert_eq!(ev.location.as_deref(), Some("Zoom"));
        assert_eq!(ev.status.as_deref(), Some("confirmed"));
        assert!(!ev.all_day);
        assert_eq!(ev.attendees.len(), 2);

        // Organizer (TJ) first.
        assert!(ev.attendees[0].is_organizer);
        assert!(ev.attendees[0].is_self);
        assert_eq!(ev.attendees[0].email, "tj@example.com");

        // Heike resolved via alias.
        let heike = &ev.attendees[1];
        assert_eq!(heike.email, "heike@example.com");
        assert_eq!(heike.team_member_id.as_deref(), Some("hk1"));
        assert_eq!(heike.response_status.as_deref(), Some("accepted"));
    }

    #[test]
    fn map_event_all_day_uses_date_field() {
        let raw = RawEvent {
            id: "evt2".into(),
            etag: None,
            summary: Some("Holiday".into()),
            description: None,
            location: None,
            status: Some("confirmed".into()),
            start: Some(RawTime {
                date_time: None,
                date: Some("2026-05-12".into()),
            }),
            end: Some(RawTime {
                date_time: None,
                date: Some("2026-05-13".into()),
            }),
            attendees: vec![],
            organizer: None,
            updated: None,
        };
        let resolver = empty_resolver();
        let ev = map_event("google:tj", raw, &resolver, None);
        assert!(ev.all_day);
        assert!(ev.start_ms > 0);
        assert!(ev.end_ms > ev.start_ms);
    }

    #[test]
    fn map_event_treats_cancelled() {
        let mut raw = RawEvent {
            id: "evt3".into(),
            etag: None,
            summary: Some("Was a meeting".into()),
            description: None,
            location: None,
            status: Some("cancelled".into()),
            start: None,
            end: None,
            attendees: vec![],
            organizer: None,
            updated: None,
        };
        raw.status = Some("cancelled".into());
        let resolver = empty_resolver();
        let ev = map_event("google:x", raw, &resolver, None);
        assert_eq!(ev.status.as_deref(), Some("cancelled"));
    }

    #[test]
    fn map_event_default_subject_when_missing() {
        let raw = RawEvent {
            id: "evt4".into(),
            etag: None,
            summary: None,
            description: None,
            location: None,
            status: None,
            start: None,
            end: None,
            attendees: vec![],
            organizer: None,
            updated: None,
        };
        let resolver = empty_resolver();
        let ev = map_event("google:x", raw, &resolver, None);
        assert_eq!(ev.title, "(no subject)");
    }

    fn header(name: &str, value: &str) -> RawHeader {
        RawHeader {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn map_message_basic() {
        let raw = RawMessage {
            id: "msg1".into(),
            thread_id: Some("thread1".into()),
            snippet: Some("Quick check on…".into()),
            internal_date: Some("1715258400000".into()),
            label_ids: vec!["INBOX".into(), "UNREAD".into()],
            payload: Some(MessagePayloadHeaders {
                headers: vec![
                    header("From", "Heike Müller <heike@example.com>"),
                    header("To", "tj@example.com"),
                    header("Cc", "Bob <bob@example.com>, Sue <sue@example.com>"),
                    header("Subject", "Roadmap review"),
                ],
            }),
        };
        let resolver = empty_resolver();
        let m = map_message("google:tj", raw, &resolver).unwrap();
        assert_eq!(m.subject, "Roadmap review");
        assert_eq!(m.from_email, "heike@example.com");
        assert_eq!(m.from_name.as_deref(), Some("Heike Müller"));
        assert_eq!(m.thread_id, "thread1");
        assert!(!m.is_read); // UNREAD label present
        assert_eq!(m.recipients.len(), 3);
        // To
        assert!(m.recipients.iter().any(|r| r.email == "tj@example.com" && r.recipient_type == "to"));
        // Cc both
        assert!(m.recipients.iter().any(|r| r.email == "bob@example.com" && r.recipient_type == "cc"));
        assert!(m.recipients.iter().any(|r| r.email == "sue@example.com" && r.recipient_type == "cc"));
    }

    #[test]
    fn map_message_default_subject_when_missing() {
        let raw = RawMessage {
            id: "msg2".into(),
            thread_id: None,
            snippet: None,
            internal_date: None,
            label_ids: vec![],
            payload: Some(MessagePayloadHeaders {
                headers: vec![header("From", "alice@example.com")],
            }),
        };
        let resolver = empty_resolver();
        let m = map_message("google:tj", raw, &resolver).unwrap();
        assert_eq!(m.subject, "(no subject)");
        assert_eq!(m.thread_id, "msg2"); // falls back to id
    }

    #[test]
    fn map_message_returns_none_when_from_missing() {
        let raw = RawMessage {
            id: "msg3".into(),
            thread_id: None,
            snippet: None,
            internal_date: None,
            label_ids: vec![],
            payload: Some(MessagePayloadHeaders {
                headers: vec![header("Subject", "lonely")],
            }),
        };
        let resolver = empty_resolver();
        assert!(map_message("google:x", raw, &resolver).is_none());
    }

    fn body_from(data: &str) -> MessageBody {
        MessageBody {
            data: Some(
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(data.as_bytes()),
            ),
        }
    }

    #[test]
    fn extract_body_text_html_preferred_over_plain() {
        let payload = MessagePayloadFull {
            mime_type: Some("multipart/alternative".into()),
            body: None,
            parts: vec![
                MessagePayloadFull {
                    mime_type: Some("text/plain".into()),
                    body: Some(body_from("plain content")),
                    parts: vec![],
                },
                MessagePayloadFull {
                    mime_type: Some("text/html".into()),
                    body: Some(body_from("<p>html content</p>")),
                    parts: vec![],
                },
            ],
        };
        assert_eq!(
            extract_body_from_payload(&payload).as_deref(),
            Some("<p>html content</p>")
        );
    }

    #[test]
    fn extract_body_falls_back_to_text_plain() {
        let payload = MessagePayloadFull {
            mime_type: Some("multipart/mixed".into()),
            body: None,
            parts: vec![MessagePayloadFull {
                mime_type: Some("text/plain".into()),
                body: Some(body_from("just text")),
                parts: vec![],
            }],
        };
        assert_eq!(
            extract_body_from_payload(&payload).as_deref(),
            Some("just text")
        );
    }

    #[test]
    fn extract_body_handles_single_part_payload() {
        // Some messages put the body directly on the top-level payload
        // without nested parts.
        let payload = MessagePayloadFull {
            mime_type: Some("text/html".into()),
            body: Some(body_from("<p>direct</p>")),
            parts: vec![],
        };
        assert_eq!(
            extract_body_from_payload(&payload).as_deref(),
            Some("<p>direct</p>")
        );
    }

    #[test]
    fn extract_body_returns_none_when_no_displayable_part() {
        let payload = MessagePayloadFull {
            mime_type: Some("multipart/mixed".into()),
            body: None,
            parts: vec![MessagePayloadFull {
                mime_type: Some("application/pdf".into()),
                body: Some(body_from("PDFDATA")),
                parts: vec![],
            }],
        };
        assert!(extract_body_from_payload(&payload).is_none());
    }
}
