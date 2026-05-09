mod ask;
mod audio;
mod chunker;
mod connectors;
mod dates;
mod diarize;
mod index;
mod keychain;
mod notes;
mod paths;
mod reconcile;
mod reminders;
mod sharing;
mod sysaudio;
mod team;
mod transcribe;
mod voice;

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
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

struct AudioState {
    recording: Option<audio::Recording>,
}

struct VoiceState {
    recording: Option<voice::VoiceRecording>,
}

/// Start recording into a Margin note bundle. The note_path must be an owned
/// `~/.margin/notes/<uuid>/note.md`; the audio backend resolves the bundle
/// dir and writes audio.wav alongside the note.
#[tauri::command]
async fn start_meeting_recording(
    app: AppHandle,
    state: State<'_, Mutex<AudioState>>,
    note_path: String,
    with_system_audio: Option<bool>,
    glossary: Option<Vec<String>>,
    model: Option<String>,
) -> Result<String, String> {
    // Fetch the Silero VAD model so the streaming chunker (#21) can cut on
    // silence boundaries. Failure here must not block recording — the
    // chunker degrades to forced time-based cuts.
    let vad_model = match chunker::ensure_vad_model(&app).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("[audio] VAD model unavailable, time-only chunking: {e}");
            None
        }
    };

    // Pre-load the user's Whisper model so the streaming worker (#22) can
    // open it instantly when the first chunk arrives. Failure must not
    // block recording — the worker drains chunks and #24's fallback path
    // re-transcribes the master WAV at Stop.
    let resolved_model = transcribe::resolve_model(model.as_deref());
    let whisper_model = match transcribe::ensure_model(&app, &resolved_model).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("[audio] Whisper model unavailable, streaming disabled: {e}");
            None
        }
    };

    let mut s = state.lock().map_err(|e| e.to_string())?;
    if s.recording.is_some() {
        return Err("already recording".into());
    }
    let r = audio::start(
        app,
        PathBuf::from(&note_path),
        with_system_audio.unwrap_or(false),
        vad_model.as_deref(),
        whisper_model,
        glossary.unwrap_or_default(),
    )?;
    let path = r.note_path.to_string_lossy().into_owned();
    s.recording = Some(r);
    Ok(path)
}

/// Kick off an AI Q&A turn over the user's notes (#31 follow-up).
/// Retrieves candidate notes via FTS, streams Anthropic's response back
/// as `ai-stream` events keyed by `turn_id`. The frontend generates the
/// `turn_id` so it can tag the in-flight assistant message *before*
/// the first event lands — otherwise the listener races the invoke
/// response and loses the `Sources` event.
#[tauri::command]
async fn ask_notes_start(
    app: AppHandle,
    turn_id: String,
    query: String,
    history: Vec<ask::ChatTurn>,
    model: Option<String>,
) -> Result<(), String> {
    ask::start(app, turn_id, query, history, model).await
}

#[tauri::command]
fn stop_meeting_recording(state: State<'_, Mutex<AudioState>>) -> Result<String, String> {
    let r = {
        let mut s = state.lock().map_err(|e| e.to_string())?;
        s.recording.take().ok_or("not recording")?
    };
    let path = r.stop()?;
    Ok(path.to_string_lossy().into_owned())
}

/// Voice query result reported back to the frontend after stop. The
/// `status` discriminator drives the palette UI: "ok" populates the
/// input with `text`, "silent" shows "Didn't catch that", "error"
/// shows the error message and stays in voice mode.
#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
struct VoiceTranscript {
    status: VoiceStatus,
    text: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
enum VoiceStatus {
    Ok,
    Silent,
    Error,
}

/// Peak amplitude floor below which we treat a recording as silence
/// and skip the (~1-2s) Whisper inference. Tuned low because the cpal
/// stream takes ~50-200ms to start delivering frames after `play()` —
/// short voice queries can capture only a few hundred ms of real audio
/// after that warm-up, so we err on the side of running Whisper rather
/// than dropping a real attempt. Whisper's own silence handling
/// catches the truly empty case.
const VOICE_SILENCE_THRESHOLD: f32 = 0.01;

/// Start mic capture for a one-shot voice query (#57). Errors out if a
/// meeting recording is already running — sharing the input device
/// across two recorders is technically possible but UX-confusing.
#[tauri::command]
fn start_voice_recording(
    app: AppHandle,
    voice_state: State<'_, Mutex<VoiceState>>,
    audio_state: State<'_, Mutex<AudioState>>,
) -> Result<(), String> {
    {
        let a = audio_state.lock().map_err(|e| e.to_string())?;
        if a.recording.is_some() {
            return Err("A meeting is recording — stop it first.".to_string());
        }
    }
    let mut v = voice_state.lock().map_err(|e| e.to_string())?;
    if v.recording.is_some() {
        // Idempotent: already recording counts as success. Avoids races
        // between the keyboard listener's autorepeat and the React
        // state updater.
        return Ok(());
    }
    let r = voice::start(app)?;
    v.recording = Some(r);
    Ok(())
}

/// Stop mic capture, finalize the temp WAV, run silence detection,
/// transcribe via the existing Whisper helper if non-silent, and
/// return the result. Always cleans up the temp WAV before returning.
#[tauri::command]
async fn stop_voice_recording(
    app: AppHandle,
    voice_state: State<'_, Mutex<VoiceState>>,
    model: Option<String>,
) -> Result<VoiceTranscript, String> {
    let r = {
        let mut v = voice_state.lock().map_err(|e| e.to_string())?;
        v.recording.take().ok_or("not recording")?
    };
    let stop = r.stop()?;
    let wav = stop.wav_path.clone();

    if stop.max_amplitude < VOICE_SILENCE_THRESHOLD {
        let _ = std::fs::remove_file(&wav);
        return Ok(VoiceTranscript {
            status: VoiceStatus::Silent,
            text: String::new(),
        });
    }

    let result = transcribe::transcribe_wav_to_transcript(
        app,
        wav.clone(),
        model,
        None,
        None,
    )
    .await;

    // Always clean up the temp WAV — voice queries are ephemeral.
    let _ = std::fs::remove_file(&wav);

    match result {
        Ok(t) => Ok(VoiceTranscript {
            status: VoiceStatus::Ok,
            text: t.full_text.trim().to_string(),
        }),
        Err(e) => Ok(VoiceTranscript {
            status: VoiceStatus::Error,
            text: e,
        }),
    }
}

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

pub struct WriteGuard {
    pub last_write: Mutex<Option<Instant>>,
}

/// Recursive watcher over `~/.margin/notes/` that keeps the SQLite index
/// in sync with on-disk state. Distinct from `WatcherState`, which is
/// per-open-file and surfaces `external-change`/`external-delete` to the
/// editor.
struct NotesIndexWatcher(Mutex<Debouncer<RecommendedWatcher, RecommendedCache>>);

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

    let app_settings = MenuItemBuilder::with_id("app_settings", "Settings\u{2026}")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;

    // Slot 0 — macOS treats this as the application menu and substitutes the app name.
    let app_sub = SubmenuBuilder::new(app, "Margin")
        .item(&PredefinedMenuItem::about(app, None, Some(about_md))?)
        .separator()
        .item(&app_settings)
        .separator()
        .item(&PredefinedMenuItem::services(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::hide(app, None)?)
        .item(&PredefinedMenuItem::hide_others(app, None)?)
        .item(&PredefinedMenuItem::show_all(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::quit(app, None)?)
        .build()?;

    let home = MenuItemBuilder::with_id("file_home", "Home")
        .accelerator("CmdOrCtrl+0")
        .build(app)?;
    let new_note = MenuItemBuilder::with_id("file_new_note", "New Note")
        .accelerator("CmdOrCtrl+N")
        .build(app)?;
    let new_meeting = MenuItemBuilder::with_id("file_record", "Start Recording")
        .accelerator("CmdOrCtrl+Shift+M")
        .build(app)?;
    let open = MenuItemBuilder::with_id("file_open", "Open\u{2026}")
        .accelerator("CmdOrCtrl+O")
        .build(app)?;
    let save = MenuItemBuilder::with_id("file_save", "Save")
        .accelerator("CmdOrCtrl+S")
        .build(app)?;
    // Note: no accelerator — Cmd+Shift+S is taken by the editor's
    // strikethrough wrapper. With autosave enabled for owned notes,
    // Save As is rarely needed and stays accessible via the File menu.
    let save_as = MenuItemBuilder::with_id("file_save_as", "Save As\u{2026}")
        .build(app)?;
    let file_sub = SubmenuBuilder::new(app, "File")
        .item(&home)
        .separator()
        .item(&new_note)
        .item(&new_meeting)
        .separator()
        .item(&open)
        .separator()
        .item(&save)
        .item(&save_as)
        .separator()
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;

    // Custom Undo/Redo so they route to CodeMirror's history instead of
    // macOS's NSResponder undo: (which doesn't reach the editor).
    let edit_undo = MenuItemBuilder::with_id("edit_undo", "Undo")
        .accelerator("CmdOrCtrl+Z")
        .build(app)?;
    let edit_redo = MenuItemBuilder::with_id("edit_redo", "Redo")
        .accelerator("CmdOrCtrl+Shift+Z")
        .build(app)?;
    let edit_sub = SubmenuBuilder::new(app, "Edit")
        .item(&edit_undo)
        .item(&edit_redo)
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
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .menu(build_menu)
        .on_menu_event(handle_menu_event)
        // Standard macOS behavior: the red close button hides the window
        // instead of quitting. Cmd+Q (which routes through the app menu's
        // Quit item) still exits cleanly. Reopen handling below brings the
        // window back when the user clicks the dock icon.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                #[cfg(target_os = "macos")]
                {
                    api.prevent_close();
                    let _ = window.hide();
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (api, window);
                }
            }
        })
        .setup(|app| {
            paths::init().map_err(|e| e.to_string())?;
            app.manage(Mutex::new(WatcherState {
                debouncer: None,
                target: None,
            }));
            app.manage(WriteGuard {
                last_write: Mutex::new(None),
            });
            app.manage(Mutex::new(AudioState { recording: None }));
            app.manage(Mutex::new(VoiceState { recording: None }));

            // Open the SQLite index, run migrations, and reconcile against
            // disk. Reconcile is fast on the happy path (a single
            // count+max_mtime check); only diverging state triggers reads.
            let mut conn = index::open_or_init(&paths::index_db_path())
                .map_err(|e| format!("open index db: {e}"))?;
            if let Err(e) = index::reconcile(&mut conn, &paths::notes_dir()) {
                eprintln!("index reconcile failed at boot: {e}");
            }
            if let Err(e) = team::bootstrap_self_if_missing(&mut conn) {
                eprintln!("team bootstrap failed at boot: {e}");
            }

            // Connector registry: holds kind-factory mappings + live
            // connector instances. Real factories register themselves
            // in their own module's setup hook in future PRs (#61, #63).
            // For #59 the registry boots empty — `rebuild_instances`
            // is still called so any previously-persisted rows are
            // observed (and skipped with a warning if their kind has
            // no factory yet).
            let registry = std::sync::Arc::new(connectors::ConnectorRegistry::new());
            if let Err(e) = registry.rebuild_instances(app.handle(), &conn) {
                eprintln!("connector registry rebuild failed at boot: {e}");
            }
            app.manage(registry);

            app.manage(Mutex::new(conn));

            // Recursive watcher over `~/.margin/notes/`. Keeps the index
            // in sync when notes are touched outside the editor (external
            // edits, finder moves, sync clients). Distinct from the
            // per-file watcher above, which surfaces external-change to
            // the open editor.
            let app_handle = app.handle().clone();
            let notes_dir = paths::notes_dir();
            let deb = new_debouncer(
                Duration::from_millis(300),
                None,
                move |res: DebounceEventResult| {
                    let Ok(events) = res else { return };
                    let conn_state = app_handle.state::<Mutex<rusqlite::Connection>>();
                    let mut conn = match conn_state.lock() {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let notes_root = paths::notes_dir();
                    for ev in events {
                        for path in &ev.paths {
                            if path.file_name().and_then(|s| s.to_str())
                                != Some(notes::NOTE_FILENAME)
                            {
                                continue;
                            }
                            use notify::EventKind::*;
                            match ev.kind {
                                Remove(_) => {
                                    if let Err(e) = index::remove(&mut conn, path) {
                                        eprintln!("index remove failed: {e}");
                                    }
                                }
                                Modify(_) | Create(_) | Any => {
                                    if path.exists() {
                                        if let Err(e) = index::upsert(&mut conn, path) {
                                            eprintln!("index upsert failed: {e}");
                                        }
                                    } else if let Err(e) = index::remove(&mut conn, path) {
                                        eprintln!("index remove failed: {e}");
                                    }
                                }
                                _ => {
                                    let _ = notes_root;
                                }
                            }
                        }
                    }
                },
            )
            .map_err(|e| format!("notes-dir watcher: {e}"))?;
            app.manage(NotesIndexWatcher(Mutex::new(deb)));
            // Begin watching the notes dir recursively.
            {
                let watcher_state = app.state::<NotesIndexWatcher>();
                let mut guard = watcher_state.0.lock().map_err(|e| e.to_string())?;
                guard
                    .watch(&notes_dir, RecursiveMode::Recursive)
                    .map_err(|e| format!("watch notes dir: {e}"))?;
            }

            // macOS Liquid Glass / NSVisualEffectView under the window so
            // the sidebar can show real desktop blur. Failure is purely
            // cosmetic (older macOS, future API drift) — fall through to
            // the CSS fallback gradient.
            if let Some(win) = app.get_webview_window("main") {
                let _ = apply_vibrancy(&win, NSVisualEffectMaterial::Sidebar, None, None);
            }

            // Reminders ticker: polls the index every 60s and fires a
            // system notification per newly-due action item (#43). The
            // task lives until the app exits.
            reminders::start(app.handle().clone());

            // Connector sync runner (#59): ticks every 15s, syncs any
            // due connectors, emits `connector-status` events per pass.
            // Idle until a real connector is configured (#60+).
            connectors::runner::start(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            read_file,
            write_file,
            file_exists,
            initial_file,
            set_mode_check,
            watch_file,
            unwatch_file,
            keychain::set_anthropic_api_key,
            keychain::delete_anthropic_api_key,
            keychain::has_anthropic_api_key,
            start_meeting_recording,
            stop_meeting_recording,
            start_voice_recording,
            stop_voice_recording,
            ask_notes_start,
            transcribe::transcribe,
            reconcile::reconcile_notes,
            notes::notes_dir,
            notes::create_note,
            notes::ensure_inbox_note,
            notes::convert_external,
            notes::duplicate_note,
            notes::is_owned_note,
            notes::list_notes,
            notes::search_notes,
            notes::note_meta,
            notes::discard_recording,
            notes::delete_note,
            notes::read_note,
            notes::write_note,
            notes::set_note_tags,
            notes::set_archived,
            notes::set_favorite,
            notes::list_actions,
            notes::set_action_done,
            notes::set_action_assignee,
            sharing::share_note,
            team::list_team_members,
            team::get_team_member,
            team::create_team_member,
            team::update_team_member,
            team::delete_team_member,
            team::set_meeting_attendees,
            team::get_meeting_attendees,
            connectors::commands::list_connectors
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            // Runtime "Open With…" on macOS (app already running).
            tauri::RunEvent::Opened { urls } => {
                for url in urls {
                    if let Ok(path) = url.to_file_path() {
                        if let Some(s) = path.to_str() {
                            let _ = app.emit("open-file", s.to_string());
                        }
                    }
                }
            }
            // Dock-icon click after we've hidden the window via the red
            // button. macOS conventionally re-shows the main window in
            // this case.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen {
                has_visible_windows, ..
            } => {
                if !has_visible_windows {
                    if let Some(win) = app.get_webview_window("main") {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
            }
            _ => {}
        });
}
