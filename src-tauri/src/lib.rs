use serde::Serialize;
use std::path::Path;
use tauri::Emitter;

#[derive(Serialize, Clone)]
struct FileContents {
    path: String,
    content: String,
}

#[tauri::command]
fn read_file(path: String) -> Result<FileContents, String> {
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(FileContents { path, content })
}

#[tauri::command]
fn write_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

#[tauri::command]
fn file_exists(path: String) -> bool {
    Path::new(&path).is_file()
}

/// Returns the path passed via "Open With…" at cold start, if any.
/// macOS passes it as argv[1]. Frontend calls this once on mount.
#[tauri::command]
fn initial_file() -> Option<String> {
    std::env::args()
        .nth(1)
        .filter(|p| Path::new(p).is_file())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            read_file,
            write_file,
            file_exists,
            initial_file
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Runtime "Open With…" on macOS (app already running).
            if let tauri::RunEvent::Opened { urls } = event {
                for url in urls {
                    if let Ok(path) = url.to_file_path() {
                        if let Some(s) = path.to_str() {
                            let _ = app.emit("open-file", s.to_string());
                        }
                    }
                }
            }
        });
}
