//! Keychain access for IPC + internal callers.
//!
//! ## Why the IPC commands are `async fn`
//!
//! Sync `#[tauri::command] pub fn …` handlers are dispatched inline
//! from the WKWebView URL-scheme handler — which runs on the AppKit
//! main thread. If macOS decides to show a SecurityAgent permission
//! dialog (every code-identity change triggers one), the dialog
//! needs the main thread's run loop to render, but the main thread
//! is blocked inside `SecKeychainFindGenericPassword`. Permanent
//! deadlock; the window never appears.
//!
//! Making each IPC handler `async fn` moves dispatch onto Tauri's
//! tokio runtime. We then `spawn_blocking` the actual keyring call
//! so we don't starve the runtime workers — `keyring` is a sync API
//! that can block for unbounded time waiting on Keychain prompts.
//! The main thread stays free to render the prompt, the user clicks
//! Allow, the future resolves, the WebView gets its answer.
//!
//! Internal `read_*` accessors stay sync. Every caller is already
//! inside an `async` context where briefly blocking a tokio worker
//! is acceptable (the main thread is free; the prompt renders; the
//! worker unblocks when the user responds).
//!
//! See #155 for the full root-cause writeup.

use keyring::Entry;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "com.margin.app";
const ACCOUNT: &str = "anthropic-api-key";
const FIRECRAWL_ACCOUNT: &str = "firecrawl-api-key";
const VOYAGE_ACCOUNT: &str = "voyage-api-key";

fn entry() -> Result<Entry, String> {
    Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())
}

fn firecrawl_entry() -> Result<Entry, String> {
    Entry::new(SERVICE, FIRECRAWL_ACCOUNT).map_err(|e| e.to_string())
}

fn connector_entry(connector_id: &str) -> Result<Entry, String> {
    let account = format!("connector::{connector_id}");
    Entry::new(SERVICE, &account).map_err(|e| e.to_string())
}

fn voyage_entry() -> Result<Entry, String> {
    Entry::new(SERVICE, VOYAGE_ACCOUNT).map_err(|e| e.to_string())
}

/// Wrap a sync keychain closure for use from an IPC command. Joins
/// the blocking task and collapses the join error into a String so
/// callers see one consistent `Result<T, String>` shape.
async fn blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("keychain task join error: {e}"))?
}

// ----- Anthropic API key ------------------------------------------------

#[tauri::command]
pub async fn set_anthropic_api_key(key: String) -> Result<(), String> {
    blocking(move || {
        match entry()?.set_password(&key) {
            Ok(()) => {
                eprintln!("[keychain] set_password OK ({} chars)", key.len());
                Ok(())
            }
            Err(err) => {
                eprintln!("[keychain] set_password ERR: {err:?}");
                Err(err.to_string())
            }
        }
    })
    .await
}

#[tauri::command]
pub async fn delete_anthropic_api_key() -> Result<(), String> {
    blocking(|| match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => {
            eprintln!("[keychain] delete ERR: {e:?}");
            Err(e.to_string())
        }
    })
    .await
}

#[tauri::command]
pub async fn has_anthropic_api_key() -> bool {
    // `bool` return type means we can't surface a join error, so
    // fall through to `false` on any failure (same as the prior
    // sync impl, which also returned false on every error path).
    blocking(|| {
        let e = entry().map_err(|err| {
            eprintln!("[keychain] entry() ERR: {err}");
            err
        })?;
        match e.get_password() {
            Ok(_) => {
                eprintln!("[keychain] get_password OK");
                Ok(true)
            }
            Err(keyring::Error::NoEntry) => {
                eprintln!("[keychain] get_password NoEntry");
                Ok(false)
            }
            Err(err) => {
                eprintln!("[keychain] get_password ERR: {err:?}");
                Ok(false)
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Internal accessor for reconcile_notes. Never via IPC.
#[allow(dead_code)]
pub fn read_anthropic_api_key() -> Result<String, String> {
    entry()?.get_password().map_err(|e| e.to_string())
}

// ----- Firecrawl API key ------------------------------------------------

#[tauri::command]
pub async fn set_firecrawl_api_key(key: String) -> Result<(), String> {
    blocking(move || {
        firecrawl_entry()?
            .set_password(&key)
            .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
pub async fn delete_firecrawl_api_key() -> Result<(), String> {
    blocking(|| match firecrawl_entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    })
    .await
}

#[tauri::command]
pub async fn has_firecrawl_api_key() -> bool {
    blocking(|| {
        let e = firecrawl_entry()?;
        Ok(matches!(e.get_password(), Ok(_)))
    })
    .await
    .unwrap_or(false)
}

/// Internal accessor for the link summarizer. Never via IPC.
#[allow(dead_code)]
pub fn read_firecrawl_api_key() -> Result<String, String> {
    firecrawl_entry()?.get_password().map_err(|e| e.to_string())
}

// ----- Voyage AI API key (#104) ----------------------------------------

#[tauri::command]
pub async fn set_voyage_api_key(key: String) -> Result<(), String> {
    blocking(move || voyage_entry()?.set_password(&key).map_err(|e| e.to_string()))
        .await
}

#[tauri::command]
pub async fn delete_voyage_api_key() -> Result<(), String> {
    blocking(|| match voyage_entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    })
    .await
}

#[tauri::command]
pub async fn has_voyage_api_key() -> bool {
    blocking(|| {
        let e = voyage_entry()?;
        Ok(matches!(e.get_password(), Ok(_)))
    })
    .await
    .unwrap_or(false)
}

/// Internal accessor for the embeddings worker. Never via IPC.
#[allow(dead_code)]
pub fn read_voyage_api_key() -> Result<String, String> {
    voyage_entry()?.get_password().map_err(|e| e.to_string())
}

// ----- Per-connector OAuth tokens (#60) ---------------------------------

/// One connector's OAuth state, stored as a single JSON-serialized
/// keychain entry under `service=com.margin.app, account=connector::<id>`.
/// Single entry per connector keeps the keychain clean compared to
/// three separate entries (access / refresh / expiry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorTokens {
    pub access_token: String,
    /// Optional because some flows (notably Microsoft public-client
    /// without `offline_access` scope) don't issue a refresh token.
    /// We request `offline_access` for both providers, so this should
    /// always be `Some` in practice.
    pub refresh_token: Option<String>,
    /// Unix-ms timestamp at which `access_token` expires. The
    /// `with_valid_token` helper refreshes when `now + 60s >= this`.
    pub expires_at_ms: i64,
    /// Space-separated scopes the provider actually granted. May
    /// differ from the request — Google/MS sometimes downgrade or
    /// substitute scopes.
    pub scope: String,
}

#[allow(dead_code)] // first caller lands with #60's oauth.rs
pub fn write_connector_tokens(
    connector_id: &str,
    tokens: &ConnectorTokens,
) -> Result<(), String> {
    let json = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
    connector_entry(connector_id)?
        .set_password(&json)
        .map_err(|e| e.to_string())
}

#[allow(dead_code)]
pub fn read_connector_tokens(
    connector_id: &str,
) -> Result<Option<ConnectorTokens>, String> {
    let entry = connector_entry(connector_id)?;
    match entry.get_password() {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| format!("connector tokens parse: {e}")),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[allow(dead_code)]
pub fn delete_connector_tokens(connector_id: &str) -> Result<(), String> {
    match connector_entry(connector_id)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
