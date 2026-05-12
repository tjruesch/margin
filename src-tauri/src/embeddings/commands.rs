//! Tauri commands for the embeddings worker (#104).

use tauri::AppHandle;

use super::worker;

/// Force one immediate pass of the embedding worker. Used by Settings
/// to backfill on demand after the user pastes a Voyage API key or
/// adds a payment method (clears any active rate-limit backoff).
#[tauri::command]
pub async fn force_reindex_embeddings(app: AppHandle) -> Result<(), String> {
    worker::clear_backoff();
    worker::run_once(&app).await
}
