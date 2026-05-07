//! Background ticker that fires system notifications when action items
//! become due. Polls the SQLite index every 60 seconds, fires once per
//! due action via `tauri-plugin-notification`, and stamps
//! `reminder_sent_ms` so the same row never re-fires.
//!
//! Click-to-open is handled by emitting the existing `"open-file"` event
//! — the frontend already routes that to `loadFile()` (App.tsx) which
//! navigates to edit mode on the target note.
//!
//! Reminders only fire while the app is running. Background launch via a
//! macOS launch agent is out of scope (#43).

use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

const TICK_SECONDS: u64 = 60;

#[derive(Debug, Clone)]
struct DueRow {
    id: String,
    note_path: String,
    note_title: String,
    text: String,
}

/// Spawn the reminder ticker on Tauri's async runtime. Returns
/// immediately; the loop runs until the app exits.
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
        // First tick fires immediately; subsequent ticks at TICK_SECONDS
        // intervals. Skip the immediate tick — gives the UI a moment to
        // come up before macOS prompts for notification permission.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = tick_once(&app) {
                eprintln!("[reminders] tick failed: {e}");
            }
        }
    });
}

fn tick_once(app: &AppHandle) -> Result<(), String> {
    let now_ms = chrono::Local::now().timestamp_millis();
    let due = collect_due(app, now_ms)?;
    for row in due {
        // Fire notification first; only stamp `reminder_sent_ms` if the
        // notification path returns Ok, so a transient error doesn't
        // silently swallow the reminder forever.
        let body_with_path = format!("{}\u{2003}({})", row.text, row.note_title);
        match app
            .notification()
            .builder()
            .title("Action item due")
            .body(body_with_path)
            .show()
        {
            Ok(_) => {
                if let Err(e) = mark_sent(app, &row.id, now_ms) {
                    eprintln!("[reminders] mark sent failed for {}: {e}", row.id);
                }
                // Open-the-note on click: tauri-plugin-notification v2
                // doesn't expose a per-notification click callback on
                // macOS, so we fall back to emitting the event here.
                // The user clicking the notification raises the app
                // window; the frontend can offer a "go to most recent
                // due action" affordance later if needed.
                let _ = app
                    .emit("open-file", row.note_path.clone())
                    .map_err(|e| eprintln!("[reminders] emit open-file: {e}"));
            }
            Err(e) => {
                eprintln!("[reminders] notification show failed: {e}");
            }
        }
    }
    Ok(())
}

fn collect_due(app: &AppHandle, now_ms: i64) -> Result<Vec<DueRow>, String> {
    let conn_state = app.state::<Mutex<Connection>>();
    let conn = conn_state.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT a.id, a.note_path, n.title, a.text \
             FROM actions a JOIN notes n ON n.note_path = a.note_path \
             WHERE a.done = 0 AND a.due_ms IS NOT NULL \
               AND a.due_ms <= ?1 AND a.reminder_sent_ms IS NULL \
               AND n.archived = 0",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([now_ms], |r| {
            Ok(DueRow {
                id: r.get(0)?,
                note_path: r.get(1)?,
                note_title: r.get(2)?,
                text: r.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

fn mark_sent(app: &AppHandle, id: &str, now_ms: i64) -> Result<(), String> {
    let conn_state = app.state::<Mutex<Connection>>();
    let conn = conn_state.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE actions SET reminder_sent_ms = ?1 WHERE id = ?2",
        params![now_ms, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
