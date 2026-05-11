//! Tauri commands for the edge synthesizer (#103).

use tauri::AppHandle;

use super::synthesizer::{self, EdgeSynthReport};

/// User-driven trigger for the deterministic edge synthesizer.
/// `force=true` bypasses the TTL gate. Mirrors `synthesize_workstreams`.
#[tauri::command]
pub async fn synthesize_edges(
    app: AppHandle,
    force: bool,
) -> Result<EdgeSynthReport, String> {
    synthesizer::maybe_run(&app, force).await
}
