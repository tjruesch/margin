//! Tauri command handlers for the connector module.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::oauth;
use super::providers;
use super::ConnectorInfo;
use super::ConnectorRegistry;

#[tauri::command]
pub fn list_connectors(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<ConnectorInfo>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let mut stmt = c
        .prepare(
            "SELECT c.id, c.kind, c.display_name, c.enabled, \
                    s.last_sync_ms, s.last_success_ms, s.last_error, \
                    COALESCE(s.next_due_ms, 0) AS next_due_ms \
             FROM connectors c \
             LEFT JOIN sync_status s ON s.connector_id = c.id \
             ORDER BY c.kind, c.display_name",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ConnectorInfo {
                id: r.get(0)?,
                kind: r.get(1)?,
                display_name: r.get(2)?,
                enabled: r.get::<_, i64>(3)? != 0,
                last_sync_ms: r.get(4)?,
                last_success_ms: r.get(5)?,
                last_error: r.get(6)?,
                next_due_ms: r.get(7)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Zero `sync_status.next_due_ms` for a connector so the runner picks
/// it up on its next ≤15s tick. Inserts a fresh row if the connector
/// has never synced. Pure DB logic; the command wrapper below adds
/// the state-lock layer.
fn force_next_due_now(conn: &Connection, connector_id: &str) -> rusqlite::Result<()> {
    let n = conn.execute(
        "UPDATE sync_status SET next_due_ms = 0 WHERE connector_id = ?1",
        rusqlite::params![connector_id],
    )?;
    if n == 0 {
        // First-ever sync — no sync_status row yet. Insert one with
        // next_due_ms = 0 so the runner picks it up on the next tick.
        conn.execute(
            "INSERT INTO sync_status(connector_id, last_sync_ms, next_due_ms) \
             VALUES (?1, 0, 0) \
             ON CONFLICT(connector_id) DO UPDATE SET next_due_ms = 0",
            rusqlite::params![connector_id],
        )?;
    }
    Ok(())
}

/// Force the runner to pick this connector up on its next tick (≤15s),
/// regardless of `next_due_ms`. Powers the "Sync now" button after a
/// reauth flow + the impatient-user case. No-op if the connector id
/// doesn't exist; the runner's join against `connectors` will skip it.
#[tauri::command]
pub fn sync_connector_now(
    connector_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    force_next_due_now(&c, &connector_id).map_err(|e| e.to_string())
}

#[derive(Serialize, Clone)]
pub struct OAuthProviderInfo {
    pub kind: String,
    pub display_name: String,
}

/// Returns the OAuth providers whose client_id is set at build time.
/// Drives the "Add connector" picker — providers without a configured
/// client_id are hidden so users don't see a button that can't work.
#[tauri::command]
pub fn list_oauth_providers() -> Vec<OAuthProviderInfo> {
    providers::list_configured()
        .into_iter()
        .map(|p| OAuthProviderInfo {
            kind: p.kind.to_string(),
            display_name: p.display_name.to_string(),
        })
        .collect()
}

/// Run the OAuth flow for `kind`, persist tokens + DB row, and return
/// the resulting `connector_id`.
///
/// Same `connector_id` repeats: the row is updated (tokens rotated)
/// rather than duplicated. This is also the "Reconnect" path — the
/// frontend can call this again with the same kind for an existing
/// account; the user just re-authenticates and the new tokens replace
/// the old ones.
#[tauri::command]
pub async fn start_oauth_connector(
    app: AppHandle,
    kind: String,
    conn: tauri::State<'_, Mutex<Connection>>,
    registry: tauri::State<'_, Arc<ConnectorRegistry>>,
) -> Result<String, String> {
    let provider = providers::lookup(&kind).ok_or_else(|| format!("unknown kind: {kind}"))?;

    let result = oauth::run_authorization_flow(&app, &kind)
        .await
        .map_err(|e| e.to_string())?;

    let connector_id = format!("{}:{}", kind, result.email);
    let display_name = format!("{} ({})", provider.display_name, result.email);
    let now_ms = current_unix_ms();

    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES (?1, ?2, ?3, 1, '{}', ?4, ?4) \
             ON CONFLICT(id) DO UPDATE SET \
                display_name = excluded.display_name, \
                enabled = 1, \
                updated_ms = excluded.updated_ms",
            rusqlite::params![&connector_id, &kind, &display_name, now_ms],
        )
        .map_err(|e| e.to_string())?;

        // Mark the connector due for an immediate sync the next time
        // the runner ticks.
        c.execute(
            "INSERT INTO sync_status(connector_id, next_due_ms) \
             VALUES (?1, 0) \
             ON CONFLICT(connector_id) DO UPDATE SET \
                next_due_ms = 0, \
                last_error = NULL",
            rusqlite::params![&connector_id],
        )
        .map_err(|e| e.to_string())?;
    }

    // Re-instantiate connectors. If a factory for this kind is
    // registered (#61, #63 onward), this picks the new row up; if
    // not, the row is skipped and the runner logs "no factory" until
    // a future build registers one.
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        if let Err(e) = registry.rebuild_instances(&app, &c) {
            eprintln!("[connectors] rebuild after OAuth flow failed: {e}");
        }
    }

    let _ = app.emit(
        "connector-status",
        serde_json::json!({
            "connector_id": connector_id,
            "state": "added",
            "message": null,
        }),
    );

    Ok(connector_id)
}

/// Remove a connector's DB row + keychain tokens. Idempotent — calling
/// twice is safe.
#[tauri::command]
pub async fn delete_connector(
    app: AppHandle,
    connector_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
    registry: tauri::State<'_, Arc<ConnectorRegistry>>,
) -> Result<(), String> {
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        // CASCADE on sync_status takes care of the status row.
        c.execute(
            "DELETE FROM connectors WHERE id = ?1",
            rusqlite::params![&connector_id],
        )
        .map_err(|e| e.to_string())?;
    }

    if let Err(e) = oauth::forget_tokens(&connector_id) {
        // Log but don't fail — the DB row is gone, which is the
        // source of truth. Orphan keychain entries are inert.
        eprintln!("[connectors] forget_tokens failed for {connector_id}: {e}");
    }

    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        if let Err(e) = registry.rebuild_instances(&app, &c) {
            eprintln!("[connectors] rebuild after delete failed: {e}");
        }
    }

    let _ = app.emit(
        "connector-status",
        serde_json::json!({
            "connector_id": connector_id,
            "state": "removed",
            "message": null,
        }),
    );

    Ok(())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- Calendar query commands (#63) -------------------------------------

#[tauri::command]
pub fn list_calendar_events(
    start_ms: i64,
    end_ms: i64,
    connector_id: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<super::calendar::CalendarEvent>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    super::calendar::list_events_in_range(&c, start_ms, end_ms, connector_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_event_details(
    event_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<super::calendar::CalendarEvent>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    super::calendar::get_event_details(&c, &event_id).map_err(|e| e.to_string())
}

/// Click handler for the "Coming up" strip (#62, #112). Returns the
/// `note_id` of the note belonging to this calendar event:
///   - If the event already has a `linked_note_id` and the row still
///     exists in `notes`, return that id.
///   - Otherwise, create a fresh `notes` row with a starter body
///     (calendar metadata at the top of body_md), persist meeting
///     attendees, link the event row, and return the new id.
#[tauri::command]
pub fn open_or_create_event_note(
    event_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<String, String> {
    let event = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        super::calendar::get_event_details(&c, &event_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("event {event_id} not found"))?
    };

    // Reuse the linked note row if it still exists.
    if let Some(linked_id) = &event.linked_note_id {
        let c = conn.lock().map_err(|e| e.to_string())?;
        let exists: bool = c
            .query_row(
                "SELECT 1 FROM notes WHERE id = ?1",
                rusqlite::params![linked_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_some();
        if exists {
            return Ok(linked_id.clone());
        }
    }

    // Create a fresh note row. The body carries a frontmatter-style
    // metadata block + title heading so AI ask can join back via
    // `calendar_event_id` in future prompts (#64). After #112 body_md
    // is opaque markdown text to the runtime; the YAML at the top
    // survives as content.
    let new_id = uuid::Uuid::new_v4().to_string();
    let body = format_event_note_body(&event);
    let title = event.title.clone();
    let now = current_unix_ms();

    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                           preview, body_size, created_ms) \
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?4)",
        rusqlite::params![
            new_id,
            title,
            body,
            now,
            crate::notes::extract_preview(&body),
            body.len() as i64,
        ],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO notes_fts(note_id, title, body) VALUES (?1, ?2, ?3)",
        rusqlite::params![new_id, title, body],
    )
    .map_err(|e| e.to_string())?;

    let member_ids: Vec<String> = event
        .attendees
        .iter()
        .filter_map(|a| a.team_member_id.clone())
        .collect();
    if !member_ids.is_empty() {
        let mut stmt = tx
            .prepare(
                "INSERT INTO meeting_attendees(note_id, member_id) VALUES (?1, ?2) \
                 ON CONFLICT(note_id, member_id) DO NOTHING",
            )
            .map_err(|e| e.to_string())?;
        for member_id in &member_ids {
            stmt.execute(rusqlite::params![new_id, member_id])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    super::calendar::set_linked_note_id(&mut c, &event.id, &new_id)
        .map_err(|e| e.to_string())?;

    Ok(new_id)
}

fn format_event_note_body(event: &super::calendar::CalendarEvent) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("calendar_event_id: {}\n", yaml_escape(&event.id)));
    s.push_str(&format!("meeting_start_ms: {}\n", event.start_ms));
    s.push_str(&format!("meeting_end_ms: {}\n", event.end_ms));
    if let Some(loc) = &event.location {
        s.push_str(&format!("location: {}\n", yaml_escape(loc)));
    }
    s.push_str("---\n\n");
    s.push_str(&format!("# {}\n\n", event.title));
    s
}

// ----- Email query commands (#69) ----------------------------------------

#[tauri::command]
pub fn list_email_messages(
    thread_id: Option<String>,
    sent_from_ms: Option<i64>,
    sent_to_ms: Option<i64>,
    connector_id: Option<String>,
    limit: Option<u32>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<super::email::EmailMessage>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    if let Some(tid) = thread_id {
        return super::email::list_messages_by_thread(&c, &tid).map_err(|e| e.to_string());
    }
    let from = sent_from_ms.unwrap_or(0);
    let to = sent_to_ms.unwrap_or_else(|| current_unix_ms() + 24 * 3600 * 1000);
    let lim = limit.unwrap_or(100) as usize;
    super::email::list_messages_in_range(&c, from, to, connector_id.as_deref(), lim)
        .map_err(|e| e.to_string())
}

/// Lazy-fetch a message body. On first call we hand off to the
/// connector's `fetch_message_body` trait method (which knows how to
/// talk to its own provider — Graph for Microsoft, Gmail for Google).
/// We then persist the result and return. Subsequent calls return the
/// cached value.
///
/// Pre-#61 this function had a hardcoded fallback to `microsoft_graph`
/// when parsing connector_id failed; now dispatch is by registered
/// connector instance, so any provider that overrides
/// `fetch_message_body` works automatically.
#[tauri::command]
pub async fn get_email_body(
    app: AppHandle,
    message_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
    registry: tauri::State<'_, Arc<ConnectorRegistry>>,
) -> Result<Option<String>, String> {
    // Fast path: already cached.
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        match super::email::get_message_body_html(&c, &message_id) {
            Ok(Some(body)) => return Ok(Some(body)),
            Ok(None) => {} // exists but body not yet fetched
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
    }

    // Look up origin (connector_id, external_id).
    let origin = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        super::email::get_message_origin(&c, &message_id).map_err(|e| e.to_string())?
    };
    let (connector_id, external_id) = match origin {
        Some(t) => t,
        None => return Ok(None),
    };

    // Dispatch through the connector trait. Calendar-only connectors
    // get the default Ok(None) — UI then renders the empty state.
    let connector = registry
        .get(&connector_id)
        .ok_or_else(|| format!("connector {connector_id} not registered"))?;
    let body = connector
        .fetch_message_body(&app, &external_id)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(ref html) = body {
        let c = conn.lock().map_err(|e| e.to_string())?;
        if let Err(e) = super::email::set_message_body_html(&c, &message_id, html) {
            eprintln!("[connectors] persist email body failed for {message_id}: {e}");
        }
    }
    Ok(body)
}

// ----- GitHub connector (#165) -------------------------------------------

/// Far-future expiry for the stored PAT. GitHub personal access tokens
/// don't carry an expiry in the token response (classic tokens may never
/// expire; fine-grained ones expire server-side and simply start
/// returning 401, which surfaces as `reauth_needed`). We park the
/// keychain `expires_at_ms` in the year ~3000 so no refresh is attempted.
const GITHUB_TOKEN_EXPIRY_MS: i64 = 32_503_680_000_000;

/// Connect a GitHub account from a pasted personal access token. Unlike
/// `start_oauth_connector` there's no browser flow: we validate the
/// token via `GET /user`, store it in the keychain, write the connector
/// row, and mark it due so the runner backfills the last 30 days on its
/// next tick. Re-running with a token for the same account rotates it.
#[tauri::command]
pub async fn connect_github(
    app: AppHandle,
    token: String,
    conn: tauri::State<'_, Mutex<Connection>>,
    registry: tauri::State<'_, Arc<ConnectorRegistry>>,
) -> Result<String, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("Paste a GitHub personal access token first.".to_string());
    }

    let identity = super::github::validate_token(&token)
        .await
        .map_err(|e| match e {
            super::ConnectorError::ReauthNeeded(_) => {
                "That token didn't work. Check it has `repo` (or read) scope and hasn't expired."
                    .to_string()
            }
            other => other.to_string(),
        })?;

    let connector_id = format!("github:{}", identity.login);
    let display_name = match identity.name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => format!("GitHub ({} · {n})", identity.login),
        _ => format!("GitHub ({})", identity.login),
    };

    // Persist the PAT in the keychain — same slot the OAuth connectors
    // use, so `delete_connector` cleans it up for free.
    let tokens = crate::keychain::ConnectorTokens {
        access_token: token,
        refresh_token: None,
        expires_at_ms: GITHUB_TOKEN_EXPIRY_MS,
        scope: String::new(),
    };
    crate::keychain::write_connector_tokens(&connector_id, &tokens).map_err(|e| e.to_string())?;

    let now_ms = current_unix_ms();
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES (?1, 'github', ?2, 1, '{}', ?3, ?3) \
             ON CONFLICT(id) DO UPDATE SET \
                display_name = excluded.display_name, \
                enabled = 1, \
                updated_ms = excluded.updated_ms",
            rusqlite::params![&connector_id, &display_name, now_ms],
        )
        .map_err(|e| e.to_string())?;
        c.execute(
            "INSERT INTO sync_status(connector_id, next_due_ms) \
             VALUES (?1, 0) \
             ON CONFLICT(connector_id) DO UPDATE SET next_due_ms = 0, last_error = NULL",
            rusqlite::params![&connector_id],
        )
        .map_err(|e| e.to_string())?;
    }

    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        if let Err(e) = registry.rebuild_instances(&app, &c) {
            eprintln!("[connectors] rebuild after connect_github failed: {e}");
        }
    }

    let _ = app.emit(
        "connector-status",
        serde_json::json!({
            "connector_id": connector_id,
            "state": "added",
            "message": null,
        }),
    );

    Ok(connector_id)
}

/// True when a GitHub connector is configured. Drives the Settings card
/// (connect form vs connected state) and the changelog empty state.
#[tauri::command]
pub fn has_github_connector(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<bool, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM connectors WHERE kind = 'github'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// Changelog feed query — pull requests only, newest-first.
#[tauri::command]
pub fn list_github_contributions(
    limit: Option<u32>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<super::github_contributions::Contribution>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let lim = limit.unwrap_or(300) as usize;
    super::github_contributions::list_contributions(&c, lim).map_err(|e| e.to_string())
}

/// AI changelog insight for one PR. Cached after first generation;
/// `regenerate: true` forces a fresh pass. Generation needs the
/// Anthropic key (keychain) and calls out to GitHub + Claude, so it's
/// `async` and lazy (only when the user opens the detail view).
#[derive(Serialize)]
pub struct ContributionInsight {
    pub summary: String,
    pub highlight: Option<super::github::InsightHighlight>,
    pub generated_ms: i64,
    pub cached: bool,
}

#[tauri::command]
pub async fn get_contribution_insight(
    id: String,
    regenerate: Option<bool>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<ContributionInsight, String> {
    let contribution = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        super::github_contributions::get_contribution(&c, &id).map_err(|e| e.to_string())?
    }
    .ok_or_else(|| "contribution not found".to_string())?;

    // Cached path — return the stored insight unless a regen is forced.
    if !regenerate.unwrap_or(false) {
        if let Some(gen_ms) = contribution.ai_generated_ms {
            let highlight = contribution
                .ai_highlight
                .as_deref()
                .and_then(|j| serde_json::from_str::<super::github::InsightHighlight>(j).ok());
            return Ok(ContributionInsight {
                summary: contribution.ai_summary.clone().unwrap_or_default(),
                highlight,
                generated_ms: gen_ms,
                cached: true,
            });
        }
    }

    let number = contribution
        .external_id
        .rsplit('#')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| format!("can't parse PR number from {}", contribution.external_id))?;

    let generated = super::github::generate_pr_insight(
        &contribution.connector_id,
        &contribution.repo,
        number,
        &contribution.title,
        contribution.body.as_deref(),
    )
    .await?;

    let now_ms = current_unix_ms();
    let highlight_json = generated
        .highlight
        .as_ref()
        .and_then(|h| serde_json::to_string(h).ok());
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        super::github_contributions::set_ai_insight(
            &c,
            &id,
            &generated.summary,
            highlight_json.as_deref(),
            now_ms,
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(ContributionInsight {
        summary: generated.summary,
        highlight: generated.highlight,
        generated_ms: now_ms,
        cached: false,
    })
}

/// Minimal YAML string escape — sufficient for IDs / locations the
/// connector hands us. If the value contains any special chars
/// (colon, quotes, newline, leading/trailing whitespace) we wrap it
/// in double quotes and backslash-escape internal quotes/backslashes.
fn yaml_escape(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains(':')
        || s.contains('\n')
        || s.contains('"')
        || s.contains('\'')
        || s.contains('#');
    if !needs_quoting {
        return s.to_string();
    }
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); \
             INSERT INTO meta(key, value) VALUES ('schema_version', '0');",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/008_connectors.sql"))
            .unwrap();
        // Seed one connector so FK on sync_status.connector_id can hold.
        conn.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES ('c1', 'microsoft_graph', 'Test', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn sync_now_resets_next_due_for_existing_status() {
        let conn = open_test_db();
        // Seed sync_status with a far-future next_due_ms.
        conn.execute(
            "INSERT INTO sync_status(connector_id, last_sync_ms, last_success_ms, next_due_ms) \
             VALUES ('c1', 1_000, 1_000, 9_999_999_999_999)",
            [],
        )
        .unwrap();
        force_next_due_now(&conn, "c1").unwrap();
        let next: i64 = conn
            .query_row(
                "SELECT next_due_ms FROM sync_status WHERE connector_id = 'c1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, 0);
    }

    #[test]
    fn sync_now_creates_status_when_missing() {
        let conn = open_test_db();
        // No sync_status row yet for c1 — verify pre-state.
        let pre: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_status WHERE connector_id = 'c1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pre, 0);
        force_next_due_now(&conn, "c1").unwrap();
        let (n, due): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(next_due_ms) FROM sync_status WHERE connector_id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(due, 0);
    }

    #[test]
    fn sync_now_preserves_last_error_so_dot_stays_until_runner_succeeds() {
        // Edge case: user clicks "Sync now" while the connector is in
        // reauth_needed state. We must NOT clear last_error here —
        // only the runner's success path clears it (write_sync_status_ok).
        // Otherwise the dot would disappear before the sync actually
        // succeeded, misleading the user.
        let conn = open_test_db();
        conn.execute(
            "INSERT INTO sync_status(connector_id, last_sync_ms, last_error, next_due_ms) \
             VALUES ('c1', 1_000, 'reauth_needed: revoked', 9_999_999_999_999)",
            [],
        )
        .unwrap();
        force_next_due_now(&conn, "c1").unwrap();
        let (next, err): (i64, Option<String>) = conn
            .query_row(
                "SELECT next_due_ms, last_error FROM sync_status WHERE connector_id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(next, 0);
        assert_eq!(err.as_deref(), Some("reauth_needed: revoked"));
    }
}
