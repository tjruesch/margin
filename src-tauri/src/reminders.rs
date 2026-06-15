//! Background reminder ticker for due todos (#166).
//!
//! Re-introduces the reminders ticker that was removed with the
//! note-derived actions feature (#162) — now driven by the standalone
//! `todos` table. Mirrors `connectors/runner.rs`: one
//! `tauri::async_runtime::spawn` task ticking on a fixed interval,
//! errors logged but never fatal.
//!
//! Each tick finds incomplete, dated todos that are due and haven't
//! been notified, fires a native OS notification, and stamps
//! `notified_ms` so the same todo never re-fires (rescheduling the due
//! date re-arms it — see `todos::update`). A `todos-changed` event is
//! emitted after a batch so the UI refreshes.

use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// How often to check for due todos. Fine enough that a reminder fires
/// within ~30s of its due time without busy-polling.
const TICK_SECS: u64 = 30;

/// Spawn the reminder ticker. Call once at app boot (in `lib.rs::setup`).
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_SECS));
        // Skip the immediate tick so boot settles before any prompt.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = tick_once(&app) {
                eprintln!("[reminders] tick failed: {e}");
            }
        }
    });
}

fn tick_once(app: &AppHandle) -> Result<(), String> {
    let now = current_unix_ms();
    let conn_state = app.state::<Mutex<Connection>>();

    let due = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        crate::todos::due_unnotified(&c, now).map_err(|e| e.to_string())?
    };
    if due.is_empty() {
        return Ok(());
    }

    let mut any = false;
    for todo in &due {
        // `.show()` is synchronous; no lock is held across it.
        let result = app
            .notification()
            .builder()
            .title("Todo due")
            .body(&todo.text)
            .show();
        if let Err(e) = result {
            // Leave notified_ms unset so the next tick retries.
            eprintln!("[reminders] notification failed: {e}");
            continue;
        }
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        if let Err(e) = crate::todos::mark_notified(&c, &todo.id, now) {
            eprintln!("[reminders] mark_notified failed: {e}");
        }
        any = true;
    }

    if any {
        // Nudge the UI so overdue badges / the page refresh.
        let _ = app.emit("todos-changed", ());
    }
    Ok(())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
