use std::fs;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::paths;

#[derive(Serialize)]
pub struct MeetingItem {
    pub path: String,
    pub title: String,
    pub modified_ms: i64,
    pub duration_ms: Option<u64>,
}

/// List meeting `.md` files in `~/.margin/meetings/`. Newest first by mtime.
/// For each file: pulls the title from the first `# ` heading (falls back to
/// the filename stem) and the duration from the matching
/// `<id>.transcript.json` sidecar if present.
#[tauri::command]
pub fn list_meetings() -> Result<Vec<MeetingItem>, String> {
    let dir = paths::meetings_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Title from first `# ` line.
        let body = fs::read_to_string(&path).unwrap_or_default();
        let title = body
            .lines()
            .find_map(|l| {
                let trimmed = l.trim_start();
                trimmed
                    .strip_prefix("# ")
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
            })
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        // Duration from sidecar JSON.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let sidecar = dir.join(format!("{stem}.transcript.json"));
        let duration_ms = if sidecar.exists() {
            fs::read_to_string(&sidecar)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| {
                    v.get("duration_ms")
                        .and_then(|d| d.as_u64())
                })
        } else {
            None
        };

        out.push(MeetingItem {
            path: path.to_string_lossy().into_owned(),
            title,
            modified_ms,
            duration_ms,
        });
    }

    out.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    Ok(out)
}
