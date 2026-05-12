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

#[tauri::command]
pub fn set_anthropic_api_key(key: String) -> Result<(), String> {
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
}

#[tauri::command]
pub fn delete_anthropic_api_key() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => {
            eprintln!("[keychain] delete ERR: {e:?}");
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub fn has_anthropic_api_key() -> bool {
    let e = match entry() {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[keychain] entry() ERR: {err}");
            return false;
        }
    };
    match e.get_password() {
        Ok(_) => {
            eprintln!("[keychain] get_password OK");
            true
        }
        Err(keyring::Error::NoEntry) => {
            eprintln!("[keychain] get_password NoEntry");
            false
        }
        Err(err) => {
            eprintln!("[keychain] get_password ERR: {err:?}");
            false
        }
    }
}

/// Internal accessor for reconcile_notes. Never via IPC.
#[allow(dead_code)]
pub fn read_anthropic_api_key() -> Result<String, String> {
    entry()?.get_password().map_err(|e| e.to_string())
}

// ----- Firecrawl API key ------------------------------------------------

#[tauri::command]
pub fn set_firecrawl_api_key(key: String) -> Result<(), String> {
    firecrawl_entry()?.set_password(&key).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_firecrawl_api_key() -> Result<(), String> {
    match firecrawl_entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn has_firecrawl_api_key() -> bool {
    let e = match firecrawl_entry() {
        Ok(e) => e,
        Err(_) => return false,
    };
    matches!(e.get_password(), Ok(_))
}

/// Internal accessor for the link summarizer. Never via IPC.
#[allow(dead_code)]
pub fn read_firecrawl_api_key() -> Result<String, String> {
    firecrawl_entry()?.get_password().map_err(|e| e.to_string())
}

// ----- Voyage AI API key (#104) ----------------------------------------

fn voyage_entry() -> Result<Entry, String> {
    Entry::new(SERVICE, VOYAGE_ACCOUNT).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_voyage_api_key(key: String) -> Result<(), String> {
    voyage_entry()?.set_password(&key).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_voyage_api_key() -> Result<(), String> {
    match voyage_entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn has_voyage_api_key() -> bool {
    let e = match voyage_entry() {
        Ok(e) => e,
        Err(_) => return false,
    };
    matches!(e.get_password(), Ok(_))
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
