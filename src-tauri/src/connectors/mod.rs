//! Connector platform foundation (#59).
//!
//! Pluggable architecture for external signal sources. A `Connector`
//! is a self-contained unit that:
//!   - Owns its own auth (OAuth tokens, API keys, etc. — see #60)
//!   - Pulls signals from an external system into local SQLite tables
//!     (calendar events for #61/#63, future: emails, chat messages, ...)
//!   - Reports a polling cadence the `SyncRunner` respects
//!   - Surfaces sync state via the unified `connector-status` Tauri event
//!
//! No real connector implementations live in this PR — those land in
//! #61 (Google Calendar) and #63 (Microsoft Graph). The trait and
//! supporting machinery here are the floor that those PRs (and any
//! future signal source) plug into.
//!
//! `dead_code` is allowed module-wide because the trait + types
//! exposed here are deliberately ahead of their first caller. Removing
//! the allow once #61 lands and exercises this surface in earnest.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;

pub mod calendar;
pub mod commands;
pub mod email;
pub mod google;
pub mod microsoft_graph;
pub mod oauth;
pub mod providers;
pub mod registry;
pub mod runner;

pub use registry::ConnectorRegistry;

/// Implemented by every signal source. Required for `#[dyn Connector]`
/// — the registry holds heterogeneous connectors as `Arc<dyn Connector>`.
///
/// `async_trait` is used here because native async-fn-in-trait
/// (Rust 1.75+) doesn't compose cleanly with `dyn Trait` without extra
/// adapter ceremony. The proc-macro overhead is one allocation per
/// `sync()` call — negligible compared to the network I/O the call
/// will dominate with anyway.
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
    /// Stable id used as the primary key in the `connectors` table and
    /// in all event payloads. Convention: `<kind>:<account>` (e.g.
    /// `google_calendar:tj@example.com`).
    fn id(&self) -> &str;

    /// Factory key that maps to a registered constructor in the
    /// `ConnectorRegistry`. Multiple connectors can share a kind
    /// (e.g. two Google accounts) but each instance has a unique `id`.
    fn kind(&self) -> &str;

    /// Human-readable label for the Settings UI.
    fn display_name(&self) -> &str;

    /// How often `sync` should run when the connector is enabled. The
    /// `SyncRunner` ticks at a finer resolution and respects this as
    /// the per-connector minimum gap between syncs.
    fn poll_interval(&self) -> Duration;

    /// Pull the latest state from the external system into local
    /// storage. Should commit DB writes through `ctx.conn` directly —
    /// callers don't write to the DB on the connector's behalf.
    async fn sync(&self, ctx: SyncCtx<'_>) -> Result<SyncReport, ConnectorError>;

    /// Lazy-fetch the full body of an email message (#69, #61). The
    /// default returns `None` for connectors that don't ingest mail.
    /// Mail-capable connectors (Microsoft Graph, Google Gmail)
    /// override this with a provider-specific GET so the
    /// `get_email_body` Tauri command is fully provider-agnostic at
    /// the command layer.
    async fn fetch_message_body(
        &self,
        _app: &tauri::AppHandle,
        _external_id: &str,
    ) -> Result<Option<String>, ConnectorError> {
        Ok(None)
    }
}

/// Per-sync context passed to `Connector::sync`. Borrows are tied to
/// the runner's iteration so the connector can't squirrel them away.
pub struct SyncCtx<'a> {
    pub app: &'a tauri::AppHandle,
    pub conn: &'a Mutex<Connection>,
    /// Cooperative cancellation flag. Set true when the user disables
    /// or removes the connector mid-sync. Connectors should poll this
    /// at any natural break (between paginated requests, etc.) and
    /// return `ConnectorError::Other("cancelled")` when set. v1
    /// connectors can ignore this — runners give an in-flight sync
    /// time to finish naturally before a subsequent disable takes
    /// effect.
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// Counts the work done in a single sync. Used by the runner to
/// produce the `synced` event payload and (later) to power "X new
/// events today" UI affordances.
#[derive(Debug, Default, Serialize, Clone)]
pub struct SyncReport {
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    pub skipped: u64,
}

/// Failure modes the runner cares about. The variants drive both the
/// stored `sync_status.last_error` text and downstream UI surfacing
/// (e.g. `ReauthNeeded` becomes a "Reconnect" button in Settings once
/// #60's add/remove UI lands).
#[derive(Debug, Clone)]
pub enum ConnectorError {
    Network(String),
    ReauthNeeded(String),
    RateLimited { retry_after_ms: u64 },
    Other(String),
}

impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(m) => write!(f, "network: {m}"),
            Self::ReauthNeeded(m) => write!(f, "reauth_needed: {m}"),
            Self::RateLimited { retry_after_ms } => {
                write!(f, "rate_limited: retry after {retry_after_ms}ms")
            }
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl ConnectorError {
    /// Tag used when persisting to `sync_status.last_error`. Settings
    /// UI matches on this prefix to pick a UX treatment.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Network(_) => "network",
            Self::ReauthNeeded(_) => "reauth_needed",
            Self::RateLimited { .. } => "rate_limited",
            Self::Other(_) => "other",
        }
    }
}

/// Frontend-facing snapshot of one connector + its sync state. Joined
/// from the `connectors` and `sync_status` tables in
/// `commands::list_connectors`.
#[derive(Serialize, Clone)]
pub struct ConnectorInfo {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub enabled: bool,
    pub last_sync_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    pub last_error: Option<String>,
    pub next_due_ms: i64,
}

/// Persisted row in the `connectors` table. Passed to factories via
/// the registry so a connector instance can hydrate from its own
/// per-kind config blob (which the factory parses; the registry treats
/// it as opaque).
#[derive(Debug, Clone)]
pub struct ConnectorRow {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub enabled: bool,
    pub config_json: String,
}

/// Read all connector rows from the DB. Used by both the registry
/// (to instantiate at boot) and the runner (to resolve `id` → `kind`
/// when a previously-known instance has been removed mid-iteration).
pub fn load_connector_rows(conn: &Connection) -> rusqlite::Result<Vec<ConnectorRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, display_name, enabled, config_json FROM connectors \
         ORDER BY kind, display_name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ConnectorRow {
            id: r.get(0)?,
            kind: r.get(1)?,
            display_name: r.get(2)?,
            enabled: r.get::<_, i64>(3)? != 0,
            config_json: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
