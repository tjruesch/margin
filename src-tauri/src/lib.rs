mod action_deletions;
mod activity;
mod actions_migration;
mod anthropic;
mod ask;
mod audio;
mod chat;
mod chunker;
mod connectors;
mod dates;
mod diarize;
mod edges;
mod embeddings;
mod events;
mod index;
mod keychain;
mod notes;
mod observations;
mod paths;
mod profiles;
mod reconcile;
mod reconcile_rejected;
mod reminders;
mod sharing;
mod sysaudio;
mod team;
mod transcribe;
mod voice;
mod workstreams;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

struct ViewModeItems {
    edit: CheckMenuItem<Wry>,
    preview: CheckMenuItem<Wry>,
}

// The filesystem watcher infrastructure (#112) was removed: notes
// live in the DB now, so there's no on-disk state to monitor.
// `WatcherState` / `WriteGuard` / `NotesIndexWatcher` and the
// `watch_file` / `unwatch_file` / `read_file` / `write_file` IPCs
// all went with it. The audio/transcript sidecars still live on
// disk but are written exclusively by Margin's own audio worker.

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
            app.manage(Mutex::new(AudioState { recording: None }));
            app.manage(Mutex::new(VoiceState { recording: None }));

            // Open the SQLite index and apply migrations. After #112
            // notes live in the DB; the filesystem reconcile pass is
            // gone. The one-time disk → DB body backfill runs once
            // (gated by the `notes_body_backfill_done` meta flag) to
            // populate `notes.body_md` for users upgrading from a v25
            // install, then renames the legacy notes folder to
            // `notes-archive-pre-v26/`.
            let mut conn = index::open_or_init(&paths::index_db_path())
                .map_err(|e| format!("open index db: {e}"))?;
            if let Err(e) = notes::body_backfill_if_pending(
                &mut conn,
                &paths::notes_dir(),
            ) {
                eprintln!("notes body backfill failed at boot: {e}");
            }
            // #113: one-shot reparse so existing `- [?]` lines populate
            // the new note_open_questions table.
            if let Err(e) = notes::questions_backfill_if_pending(&mut conn) {
                eprintln!("questions backfill failed at boot: {e}");
            }
            if let Err(e) = team::bootstrap_self_if_missing(&mut conn) {
                eprintln!("team bootstrap failed at boot: {e}");
            }
            // #117: one-shot sweep of orphan ~/.margin/team/<id>/profile.md
            // files. Gated by the profile_md_purged meta flag so it
            // only runs once per install.
            if let Err(e) = team::purge_profile_md_if_pending(&conn) {
                eprintln!("profile.md purge failed at boot: {e}");
            }

            // #146: one-shot backfill of pre-#144 reconciled notes —
            // moves inline `## Action items` blocks into reconcile-origin
            // rows + writes per-note backups. Gated by the
            // actions_migration_v1_completed meta flag.
            actions_migration::run_if_pending(&mut conn);

            // Connector registry: holds kind-factory mappings + live
            // connector instances. Real connector modules register
            // their factories at boot (Microsoft Graph in #63;
            // Google Calendar in a future #61). `rebuild_instances`
            // then hydrates `Arc<dyn Connector>` instances from the
            // persisted `connectors` table — kinds without registered
            // factories are skipped with a warning.
            let registry = std::sync::Arc::new(connectors::ConnectorRegistry::new());
            connectors::microsoft_graph::register(&registry);
            connectors::google::register(&registry);
            if let Err(e) = registry.rebuild_instances(app.handle(), &conn) {
                eprintln!("connector registry rebuild failed at boot: {e}");
            }
            app.manage(registry);

            app.manage(Mutex::new(conn));

            // The recursive notes-dir file watcher used to live here.
            // After #112 it's gone: notes are the DB's responsibility
            // and the `write_note` IPC is the only writer.

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

            // Workstream synthesizer boot tick (#70). Stale-checks
            // last_clustered_ms inside; no-op if a fresh pass landed
            // within the last 6h. Edge synth chains onto the same task
            // so fresh workstream signals immediately feed INCLUDES /
            // CO_ATTENDED / MENTIONED edge derivation (#103).
            let app_for_cluster = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if let Err(e) =
                    workstreams::synthesizer::maybe_cluster(&app_for_cluster, false).await
                {
                    eprintln!("[workstreams] boot cluster failed: {e}");
                }
                if let Err(e) = edges::synthesizer::maybe_run(&app_for_cluster, false).await {
                    eprintln!("[edges] boot synth failed: {e}");
                }
            });

            // Embeddings polling worker (#104). Ticks every 15s; idle
            // when no rows are stale. Bails until a Voyage API key is
            // configured (emits `embed-status: needs_key`).
            embeddings::start_worker(app.handle().clone());

            // Profile snapshot worker (#107). Ticks every 60s,
            // dirty-tracked by the events table. Idle until the
            // Anthropic API key is configured.
            profiles::start_worker(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            initial_file,
            set_mode_check,
            keychain::set_anthropic_api_key,
            keychain::delete_anthropic_api_key,
            keychain::has_anthropic_api_key,
            keychain::set_firecrawl_api_key,
            keychain::delete_firecrawl_api_key,
            keychain::has_firecrawl_api_key,
            keychain::set_voyage_api_key,
            keychain::delete_voyage_api_key,
            keychain::has_voyage_api_key,
            embeddings::commands::force_reindex_embeddings,
            profiles::commands::get_profile_snapshot,
            profiles::commands::get_profile_snapshot_at,
            profiles::commands::get_first_profile_snapshot,
            profiles::commands::count_profile_snapshots,
            profiles::commands::force_recompute_profile,
            profiles::commands::team_waiting_counts,
            observations::commands::list_profile_observations,
            observations::commands::pending_observation_counts,
            observations::commands::accept_profile_observation,
            observations::commands::reject_profile_observation,
            observations::commands::delete_profile_observation,
            start_meeting_recording,
            stop_meeting_recording,
            start_voice_recording,
            stop_voice_recording,
            ask_notes_start,
            transcribe::transcribe,
            reconcile::reconcile_notes,
            notes::create_note,
            notes::ensure_inbox_note,
            notes::duplicate_note,
            notes::list_notes,
            notes::export_notes,
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
            notes::list_actions_for_note,
            notes::migrate_reconciled_notes_to_action_rows,
            notes::set_action_done,
            notes::undo_auto_resolved_action,
            notes::set_action_assignee,
            notes::set_action_workstream,
            notes::delete_action,
            notes::dismiss_waiting_action,
            notes::list_open_questions,
            notes::resolve_open_question,
            notes::reopen_open_question,
            notes::set_open_question_asked_of,
            notes::delete_open_question,
            sharing::share_note,
            team::list_team_members,
            team::get_team_member,
            team::create_team_member,
            team::update_team_member,
            team::delete_team_member,
            team::set_meeting_attendees,
            team::get_meeting_attendees,
            connectors::commands::list_connectors,
            connectors::commands::list_oauth_providers,
            connectors::commands::start_oauth_connector,
            connectors::commands::delete_connector,
            connectors::commands::sync_connector_now,
            connectors::commands::list_calendar_events,
            connectors::commands::get_event_details,
            connectors::commands::open_or_create_event_note,
            connectors::commands::list_email_messages,
            connectors::commands::get_email_body,
            workstreams::commands::synthesize_workstreams,
            workstreams::commands::attach_signal_to_workstream,
            workstreams::commands::detach_signal_from_workstream,
            workstreams::commands::list_unassigned_items,
            chat::get_active_conversation,
            chat::list_chat_messages,
            chat::append_chat_message,
            chat::clear_active_conversation,
            ask::get_prompt_dump,
            ask::list_chat_turn_metrics,
            edges::commands::synthesize_edges,
            workstreams::commands::list_workstreams,
            workstreams::commands::create_workstream,
            workstreams::commands::get_workstream_details,
            workstreams::commands::set_workstream_status,
            workstreams::commands::set_workstream_user_notes,
            workstreams::commands::list_archived_workstreams,
            workstreams::commands::mark_workstream_seen,
            workstreams::commands::set_workstream_owner,
            workstreams::commands::list_workstream_links,
            workstreams::commands::add_workstream_link,
            workstreams::commands::add_workstream_link_from_url,
            workstreams::commands::remove_workstream_link,
            workstreams::commands::set_workstream_parent,
            activity::get_daily_activity,
            activity::list_recent_activity,
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
