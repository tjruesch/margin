//! Tauri commands for the embeddings worker (#104).

use tauri::AppHandle;

use super::worker;

/// Force one immediate pass of the embedding worker. Used by Settings
/// to backfill on demand after the user pastes a Voyage API key.
#[tauri::command]
pub async fn force_reindex_embeddings(app: AppHandle) -> Result<(), String> {
    worker::run_once(&app).await
}
