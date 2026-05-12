//! Tauri commands for the workstreams module.

use std::sync::Mutex;

use rusqlite::Connection;
use tauri::AppHandle;

use super::{persist, synthesizer, ClusterReport, Workstream, WorkstreamDetail, WorkstreamLink};

#[tauri::command]
pub async fn synthesize_workstreams(
    app: AppHandle,
    force: bool,
) -> Result<ClusterReport, String> {
    synthesizer::maybe_cluster(&app, force).await
}

#[tauri::command]
pub fn list_workstreams(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<Workstream>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::list_workstreams_active(&c).map_err(|e| e.to_string())
}

/// User-driven workstream creation (#101). Mainly used to spin up
/// umbrella parents that later collect synthesized children, but also
/// works for standalone manual workstreams. Returns the new id on
/// success. Parent-validation rejections come back as a user-facing
/// error string the composer surfaces inline.
#[tauri::command]
pub fn create_workstream(
    title: String,
    summary: Option<String>,
    parent_id: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<String, String> {
    let summary = summary.unwrap_or_default();
    let parent = parent_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::create_workstream(&c, &title, &summary, parent)
        .map_err(|e| e.to_string())?
        .map_err(|reason| reason)
}

#[tauri::command]
pub fn get_workstream_details(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Option<WorkstreamDetail>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::get_workstream_detail(&c, &id).map_err(|e| e.to_string())
}

// set_workstream_action_done / _assignee / delete_workstream_action
// were removed in #111 — the unified `set_action_done` /
// `set_action_assignee` / `delete_action` IPCs in `notes.rs` now
// dispatch on origin_kind and handle both note- and synth-origin
// rows. The DB-only write helpers in `persist` remain as the synth-
// path implementation.

#[tauri::command]
pub fn set_workstream_status(
    id: String,
    status: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    if !matches!(status.as_str(), "active" | "archived" | "snoozed") {
        return Err(format!("invalid status: {status}"));
    }
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_status(&c, &id, &status).map_err(|e| e.to_string())
}

/// Update a workstream's user-authored context (#77). Whitespace-only
/// input is treated as a clear (persists `NULL`) so the prompt-omission
/// logic downstream can `filter(|s| !s.is_empty())` cleanly.
#[tauri::command]
pub fn set_workstream_user_notes(
    id: String,
    notes: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let trimmed = notes.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_user_notes(&c, &id, trimmed).map_err(|e| e.to_string())
}

/// List archived workstreams for the Workstreams view's collapsed
/// "Archived (N)" accordion (#78). Most recently archived first.
#[tauri::command]
pub fn list_archived_workstreams(
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<Workstream>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::list_workstreams_archived(&c).map_err(|e| e.to_string())
}

/// Clear the `reopened_at_ms` marker on a workstream (#78). Called by
/// the detail view's unmount cleanup once the user has visited a
/// reopened workstream — the "Reopened" badge stops showing on
/// subsequent list renders.
#[tauri::command]
pub fn mark_workstream_seen(
    id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::mark_seen(&c, &id).map_err(|e| e.to_string())
}

/// Set or clear a workstream's owner (#81). Pass `None` to unassign.
/// User-only authority — synthesizer never sets this.
#[tauri::command]
pub fn set_workstream_owner(
    id: String,
    owner_member_id: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_owner(&c, &id, owner_member_id.as_deref()).map_err(|e| e.to_string())
}

/// Set or clear a workstream's parent (#89). Pass `null` to make it a
/// top-level standalone. The 2-level hierarchy is enforced server-side
/// — invalid edges (self-parent, would-be-grandparent, current
/// workstream already has children, parent doesn't exist) come back as
/// a user-facing error string the UI surfaces verbatim.
#[tauri::command]
pub fn set_workstream_parent(
    id: String,
    parent_id: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::set_workstream_parent(&c, &id, parent_id.as_deref())
        .map_err(|e| e.to_string())?
        .map_err(|reason| reason)
}

// ----- User-curated links (#88) ------------------------------------------

#[tauri::command]
pub fn list_workstream_links(
    workstream_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<Vec<WorkstreamLink>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::list_workstream_links(&c, &workstream_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_workstream_link(
    workstream_id: String,
    label: String,
    url: String,
    kind: Option<String>,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<WorkstreamLink, String> {
    let now_ms = chrono::Local::now().timestamp_millis();
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::add_workstream_link(&c, &workstream_id, &label, &url, kind.as_deref(), now_ms)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_workstream_link(
    link_id: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    persist::remove_workstream_link(&c, &link_id)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Paste-only link entry: the user supplies just a URL; Haiku
/// categorizes it into a `(label, kind)` tuple, and we persist via the
/// same `add_workstream_link` path. Categorization failures fall back
/// to `{label: <hostname>, kind: "other"}` so the user still gets a
/// usable chip — they can rename it later via the inline composer.
///
/// After the row lands, a fire-and-forget background task fetches the
/// page via Firecrawl and asks Haiku for a 2–3 sentence summary; the
/// result lands on the row and a `workstream-link-summarized` event
/// fires so the frontend re-renders without a refetch.
#[tauri::command]
pub async fn add_workstream_link_from_url(
    app: tauri::AppHandle,
    workstream_id: String,
    url: String,
    conn: tauri::State<'_, Mutex<Connection>>,
) -> Result<WorkstreamLink, String> {
    let trimmed = url.trim().to_string();
    if trimmed.is_empty() {
        return Err("URL is required".into());
    }
    // Run the AI categorization OUTSIDE the connection lock — the
    // network round-trip can take ~1s and we don't want to block
    // other commands behind it.
    let categorized = super::link_categorizer::categorize_or_fallback(&trimmed).await;
    let now_ms = chrono::Local::now().timestamp_millis();
    let link = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        persist::add_workstream_link(
            &c,
            &workstream_id,
            &categorized.label,
            &trimmed,
            Some(&categorized.kind),
            now_ms,
        )
        .map_err(|e| e.to_string())?
    };
    // Fire-and-forget summarization. Skips silently if either key is
    // missing or the scrape returns thin content; emits the
    // `workstream-link-summarized` event on success.
    let app_clone = app.clone();
    let link_id = link.id.clone();
    let url_clone = trimmed.clone();
    tauri::async_runtime::spawn(async move {
        super::link_summarizer::populate_summary(app_clone, link_id, url_clone).await;
    });
    Ok(link)
}
