use keyring::Entry;

const SERVICE: &str = "com.margin.app";
const ACCOUNT: &str = "anthropic-api-key";

fn entry() -> Result<Entry, String> {
    Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())
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

/// Internal accessor for future summarize_meeting (#10). Never via IPC.
#[allow(dead_code)]
pub fn read_anthropic_api_key() -> Result<String, String> {
    entry()?.get_password().map_err(|e| e.to_string())
}
