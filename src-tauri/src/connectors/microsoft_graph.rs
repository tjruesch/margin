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
use super::oauth::with_valid_token;
use super::registry::ConnectorRegistry;
use super::{Connector, ConnectorError, ConnectorRow, SyncCtx, SyncReport};
use crate::team::{self, OwnerResolver, TeamMember};

const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const WINDOW_BACK_MS: i64 = 14 * 24 * 3600 * 1000;
const WINDOW_FORWARD_MS: i64 = 30 * 24 * 3600 * 1000;
const PAGE_SIZE: u32 = 100;

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

        // Fetch all events via /me/calendarView, paginated.
        let raw_events = with_valid_token(ctx.app, &self.id, &self.kind, |access| async move {
            fetch_calendar_view(&access, window_start, window_end).await
        })
        .await?;

        // Snapshot team members for attendee resolution. Keep the
        // lock window short — release before mapping.
        let team = {
            let conn = ctx
                .conn
                .lock()
                .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
            team::list_team_members_raw(&conn).map_err(ConnectorError::Other)?
        };
        let resolver = AttendeeResolver::new(&team);
        let self_email = self.self_email();

        let events: Vec<CalendarEvent> = raw_events
            .into_iter()
            .map(|raw| map_event(&self.id, raw, &resolver, self_email))
            .collect();

        // Upsert in one transaction.
        let report = {
            let mut conn = ctx
                .conn
                .lock()
                .map_err(|e| ConnectorError::Other(format!("conn lock: {e}")))?;
            super::calendar::upsert_window(&mut conn, &self.id, &events, window_start, window_end)
                .map_err(|e| ConnectorError::Other(format!("upsert events: {e}")))?
        };

        Ok(SyncReport {
            added: report.added,
            updated: report.updated,
            removed: report.removed,
            skipped: 0,
        })
    }
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
                    team_member_id: resolver.resolve(&email_lc, org.name.as_deref()),
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
            team_member_id: resolver.resolve(&email, None),
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
        linked_note_path: None,
        attendees,
    }
}

// ----- Attendee resolver -------------------------------------------------

/// Resolve an attendee's `email` (and optional display name) to a
/// `team_member_id`, if Margin knows the person.
///
/// Strategy:
///   1. Email match: lowercased email vs. team_member's display_name
///      (some teams use email-as-name) AND any of their aliases.
///   2. Name match: display_name through the existing
///      `team::OwnerResolver` (handles diacritic-insensitive
///      first-name matching).
///
/// `OwnerResolver` is in #[allow(dead_code)] until owner resolution
/// expands beyond the action-items pipeline; we hold a reference here.
pub(crate) struct AttendeeResolver<'a> {
    by_name: OwnerResolver,
    by_email: HashMap<String, String>, // email_lc → team_member_id
    _members: &'a [TeamMember],
}

impl<'a> AttendeeResolver<'a> {
    pub(crate) fn new(members: &'a [TeamMember]) -> Self {
        let mut by_email: HashMap<String, String> = HashMap::new();
        for m in members {
            // Some teams put the email itself as display_name; treat
            // that as an email key too.
            if m.display_name.contains('@') {
                by_email.insert(m.display_name.to_lowercase(), m.id.clone());
            }
            for alias in &m.aliases {
                if alias.contains('@') {
                    by_email.insert(alias.to_lowercase(), m.id.clone());
                }
            }
        }
        Self {
            by_name: OwnerResolver::from_members(members),
            by_email,
            _members: members,
        }
    }

    pub(crate) fn resolve(&self, email_lc: &str, display_name: Option<&str>) -> Option<String> {
        if let Some(id) = self.by_email.get(email_lc) {
            return Some(id.clone());
        }
        if let Some(name) = display_name {
            if let Some(id) = self.by_name.resolve(name) {
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

    fn mk_member(id: &str, name: &str, aliases: &[&str]) -> TeamMember {
        TeamMember {
            id: id.to_string(),
            display_name: name.to_string(),
            role: String::new(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            profile_md_path: String::new(),
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
        let members = [mk_member("hk1", "Heike", &["heike@example.com"])];
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
        let members = [mk_member("hk1", "Heike Müller", &["heike@contoso.com"])];
        let r = AttendeeResolver::new(&members);
        assert_eq!(r.resolve("heike@contoso.com", None).as_deref(), Some("hk1"));
        // Case-insensitive
        assert_eq!(r.resolve("Heike@CONTOSO.com".to_lowercase().as_str(), None).as_deref(), Some("hk1"));
    }

    #[test]
    fn attendee_resolver_name_fallback() {
        let members = [mk_member("hk1", "Heike Müller", &[])];
        let r = AttendeeResolver::new(&members);
        // Email not registered as alias.
        assert_eq!(r.resolve("unknown@x.com", Some("Heike Müller")).as_deref(), Some("hk1"));
    }

    #[test]
    fn attendee_resolver_no_match_returns_none() {
        let r = AttendeeResolver::new(&[]);
        assert!(r.resolve("nobody@nope.com", Some("Nobody")).is_none());
    }
}
