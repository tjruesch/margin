//! Background sync orchestration. One `tauri::async_runtime::spawn`
//! task started at app boot, ticking every `RUNNER_TICK_SECS` seconds,
//! checking each registered connector against `sync_status.next_due_ms`
//! and invoking `sync()` for the ones that are due.
//!
//! Mirrors the existing reminders ticker in `src-tauri/src/reminders.rs`
//! — same `tokio::time::interval` pattern, same `app.state::<...>()`
//! discipline, errors logged but not fatal.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::{Connector, ConnectorError, ConnectorRegistry, SyncCtx, SyncReport};

/// Minimum gap between runner iterations. The runner checks each live
/// connector's `next_due_ms` on every tick — connectors with longer
/// `poll_interval`s simply skip more ticks. 15s is fine-grained
/// enough that a 60s `poll_interval` actually fires within ~75s of
/// the last sync rather than rounded to a minute boundary.
const RUNNER_TICK_SECS: u64 = 15;

/// Spawn the runner task. Call once at app boot (in `lib.rs::setup`).
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(RUNNER_TICK_SECS));
        // Skip the immediate tick — gives the rest of setup a chance
        // to settle before the first sync hits the DB.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_due(&app).await {
                eprintln!("[connectors] runner error: {e}");
            }
        }
    });
}

/// Iterate live connectors, sync the ones whose `next_due_ms <= now`,
/// emit status events, and persist updated sync state.
async fn run_due(app: &AppHandle) -> Result<(), String> {
    let registry = app.state::<Arc<ConnectorRegistry>>();
    let conn_state = app.state::<Mutex<Connection>>();

    let now_ms = current_unix_ms();
    let due_ids = collect_due_ids(&conn_state, now_ms)?;
    if due_ids.is_empty() {
        return Ok(());
    }

    // Resolve ids → live instances. A connector that was removed
    // mid-iteration just disappears from the registry; we skip it.
    let due: Vec<Arc<dyn Connector>> = due_ids
        .iter()
        .filter_map(|id| registry.get(id))
        .collect();

    for connector in due {
        emit_status(app, connector.id(), StreamState::Syncing, None);
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = SyncCtx {
            app,
            conn: &conn_state,
            cancel: cancel.clone(),
        };

        let result = connector.sync(ctx).await;

        let interval = connector.poll_interval();
        let next_due = now_ms + interval.as_millis() as i64;
        match &result {
            Ok(report) => {
                if let Err(e) =
                    write_sync_status_ok(&conn_state, connector.id(), now_ms, next_due)
                {
                    eprintln!("[connectors] write sync_status (ok) failed: {e}");
                }
                emit_status(
                    app,
                    connector.id(),
                    StreamState::Synced,
                    Some(format_report(report)),
                );
            }
            Err(err) => {
                let tag = err.tag();
                let msg = err.to_string();
                // Backoff on rate-limit; otherwise schedule next attempt
                // at the connector's regular interval.
                let retry_ms = match err {
                    ConnectorError::RateLimited { retry_after_ms } => {
                        now_ms + (*retry_after_ms as i64)
                    }
                    _ => next_due,
                };
                if let Err(e) = write_sync_status_err(
                    &conn_state,
                    connector.id(),
                    now_ms,
                    retry_ms,
                    &msg,
                ) {
                    eprintln!("[connectors] write sync_status (err) failed: {e}");
                }
                emit_status(
                    app,
                    connector.id(),
                    StreamState::Errored,
                    Some(format!("{tag}: {msg}")),
                );
            }
        }
    }

    Ok(())
}

fn collect_due_ids(
    conn_state: &Mutex<Connection>,
    now_ms: i64,
) -> Result<Vec<String>, String> {
    let conn = conn_state.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT c.id FROM connectors c \
             LEFT JOIN sync_status s ON s.connector_id = c.id \
             WHERE c.enabled = 1 AND COALESCE(s.next_due_ms, 0) <= ?1 \
             ORDER BY c.id",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([now_ms], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

fn write_sync_status_ok(
    conn_state: &Mutex<Connection>,
    connector_id: &str,
    now_ms: i64,
    next_due_ms: i64,
) -> Result<(), String> {
    let conn = conn_state.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO sync_status(connector_id, last_sync_ms, last_success_ms, last_error, cursor, next_due_ms) \
         VALUES (?1, ?2, ?2, NULL, NULL, ?3) \
         ON CONFLICT(connector_id) DO UPDATE SET \
            last_sync_ms = excluded.last_sync_ms, \
            last_success_ms = excluded.last_success_ms, \
            last_error = NULL, \
            next_due_ms = excluded.next_due_ms",
        rusqlite::params![connector_id, now_ms, next_due_ms],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn write_sync_status_err(
    conn_state: &Mutex<Connection>,
    connector_id: &str,
    now_ms: i64,
    next_due_ms: i64,
    error: &str,
) -> Result<(), String> {
    let conn = conn_state.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO sync_status(connector_id, last_sync_ms, last_success_ms, last_error, cursor, next_due_ms) \
         VALUES (?1, ?2, NULL, ?3, NULL, ?4) \
         ON CONFLICT(connector_id) DO UPDATE SET \
            last_sync_ms = excluded.last_sync_ms, \
            last_error = excluded.last_error, \
            next_due_ms = excluded.next_due_ms",
        rusqlite::params![connector_id, now_ms, error, next_due_ms],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum StreamState {
    Syncing,
    Synced,
    Errored,
    #[allow(dead_code)] // future: surfaced when a sync is skipped due to backoff
    Skipped,
}

#[derive(Serialize, Clone)]
struct StatusEvent<'a> {
    connector_id: &'a str,
    state: StreamState,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn emit_status(
    app: &AppHandle,
    connector_id: &str,
    state: StreamState,
    message: Option<String>,
) {
    let _ = app.emit(
        "connector-status",
        StatusEvent {
            connector_id,
            state,
            message,
        },
    );
}

fn format_report(r: &SyncReport) -> String {
    format!(
        "+{}/~{}/-{} (skipped {})",
        r.added, r.updated, r.removed, r.skipped
    )
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
