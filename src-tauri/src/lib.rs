use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{
    AboutMetadata, CheckMenuItem, CheckMenuItemBuilder, Menu, MenuBuilder, MenuEvent,
    MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{AppHandle, Emitter, Manager, State, Wry};

#[derive(Serialize, Clone)]
struct FileContents {
    path: String,
    content: String,
}

struct ViewModeItems {
    edit: CheckMenuItem<Wry>,
    preview: CheckMenuItem<Wry>,
}

struct WatcherState {
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
    target: Option<PathBuf>,
}

struct WriteGuard {
    last_write: Mutex<Option<Instant>>,
}

#[tauri::command]
fn read_file(path: String) -> Result<FileContents, String> {
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(FileContents { path, content })
}

#[tauri::command]
fn write_file(
    path: String,
    content: String,
    guard: State<'_, WriteGuard>,
) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    *guard
        .last_write
        .lock()
        .map_err(|e| e.to_string())? = Some(Instant::now());
    Ok(())
}

#[tauri::command]
fn watch_file(
    app: AppHandle,
    state: State<'_, Mutex<WatcherState>>,
    path: String,
) -> Result<(), String> {
    let target = PathBuf::from(&path);
    let parent = target
        .parent()
        .ok_or("file has no parent directory")?
        .to_path_buf();
    let target_cb = target.clone();
    let path_cb = path.clone();
    let app_cb = app.clone();

    let mut deb = new_debouncer(
        Duration::from_millis(200),
        None,
        move |res: DebounceEventResult| {
            let Ok(events) = res else { return };
            for ev in events {
                if !ev.paths.iter().any(|p| p == &target_cb) {
                    continue;
                }
                // Suppress events caused by our own write_file calls.
                let guard = app_cb.state::<WriteGuard>();
                if let Ok(g) = guard.last_write.lock() {
                    if let Some(t) = *g {
                        if t.elapsed() < Duration::from_millis(500) {
                            continue;
                        }
                    }
                }
                use notify::EventKind::*;
                match ev.kind {
                    Remove(_) => {
                        let _ = app_cb.emit("external-delete", &path_cb);
                    }
                    Modify(_) | Create(_) | Any => {
                        let _ = app_cb.emit("external-change", &path_cb);
                    }
                    _ => {}
                }
            }
        },
    )
    .map_err(|e| e.to_string())?;

    deb.watch(&parent, RecursiveMode::NonRecursive)
        .map_err(|e| e.to_string())?;

    let mut s = state.lock().map_err(|e| e.to_string())?;
    s.debouncer = Some(deb); // dropping the previous Debouncer stops its watch
    s.target = Some(target);
    Ok(())
}

#[tauri::command]
fn unwatch_file(state: State<'_, Mutex<WatcherState>>) -> Result<(), String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    s.debouncer = None;
    s.target = None;
    Ok(())
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

#[tauri::command]
fn set_mode_check(state: State<'_, Mutex<ViewModeItems>>, mode: String) -> Result<(), String> {
    let items = state.lock().map_err(|e| e.to_string())?;
    items
        .edit
        .set_checked(mode == "edit")
        .map_err(|e| e.to_string())?;
    items
        .preview
        .set_checked(mode == "preview")
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let about_md = AboutMetadata {
        name: Some("Margin".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        ..Default::default()
    };

    // Slot 0 — macOS treats this as the application menu and substitutes the app name.
    let app_sub = SubmenuBuilder::new(app, "Margin")
        .item(&PredefinedMenuItem::about(app, None, Some(about_md))?)
        .separator()
        .item(&PredefinedMenuItem::services(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::hide(app, None)?)
        .item(&PredefinedMenuItem::hide_others(app, None)?)
        .item(&PredefinedMenuItem::show_all(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::quit(app, None)?)
        .build()?;

    let open = MenuItemBuilder::with_id("file_open", "Open\u{2026}")
        .accelerator("CmdOrCtrl+O")
        .build(app)?;
    let save = MenuItemBuilder::with_id("file_save", "Save")
        .accelerator("CmdOrCtrl+S")
        .build(app)?;
    let save_as = MenuItemBuilder::with_id("file_save_as", "Save As\u{2026}")
        .accelerator("CmdOrCtrl+Shift+S")
        .build(app)?;
    let file_sub = SubmenuBuilder::new(app, "File")
        .item(&open)
        .separator()
        .item(&save)
        .item(&save_as)
        .separator()
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;

    let edit_sub = SubmenuBuilder::new(app, "Edit")
        .item(&PredefinedMenuItem::undo(app, None)?)
        .item(&PredefinedMenuItem::redo(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&PredefinedMenuItem::select_all(app, None)?)
        .build()?;

    let mode_edit = CheckMenuItemBuilder::with_id("view_edit", "Edit")
        .accelerator("CmdOrCtrl+E")
        .checked(true)
        .build(app)?;
    let mode_preview = CheckMenuItemBuilder::with_id("view_preview", "Preview")
        .accelerator("CmdOrCtrl+P")
        .checked(false)
        .build(app)?;
    let view_sub = SubmenuBuilder::new(app, "View")
        .item(&mode_edit)
        .item(&mode_preview)
        .build()?;

    // Stash the check-item handles so set_mode_check can find them in O(1)
    // without traversing the menu tree (Menu::get only walks the top level).
    app.manage(Mutex::new(ViewModeItems {
        edit: mode_edit.clone(),
        preview: mode_preview.clone(),
    }));

    let window_sub = SubmenuBuilder::new(app, "Window")
        .item(&PredefinedMenuItem::minimize(app, None)?)
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;

    Ok(MenuBuilder::new(app)
        .items(&[&app_sub, &file_sub, &edit_sub, &view_sub, &window_sub])
        .build()?)
}

fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    let _ = app.emit("menu", event.id().as_ref().to_string());
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .menu(build_menu)
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            app.manage(Mutex::new(WatcherState {
                debouncer: None,
                target: None,
            }));
            app.manage(WriteGuard {
                last_write: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            read_file,
            write_file,
            file_exists,
            initial_file,
            set_mode_check,
            watch_file,
            unwatch_file
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
