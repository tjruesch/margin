//! Note bundle abstraction.
//!
//! An owned Margin note lives at `~/.margin/notes/<uuid>/note.md`.
//! Sibling files in the bundle (audio.wav, transcript.json, etc.)
//! carry supporting context for that note.
//!
//! External markdown files (anything outside `~/.margin/notes/`) open in
//! restricted mode in the UI; the user can promote them to owned notes
//! via `convert_external`, which copies the file into a fresh bundle.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::paths;

/// Per-bundle filename for the note's markdown body.
pub const NOTE_FILENAME: &str = "note.md";
/// Per-bundle filename for the recorded audio (only if a recording exists).
pub const AUDIO_FILENAME: &str = "audio.wav";
/// Per-bundle filename for the transcript sidecar (only if transcribed).
pub const TRANSCRIPT_FILENAME: &str = "transcript.json";

#[derive(Serialize)]
pub struct NoteRef {
    pub id: String,
    pub note_path: String,
}

#[derive(Serialize)]
pub struct NoteListItem {
    pub note_path: String,
    pub title: String,
    pub modified_ms: i64,
    pub duration_ms: Option<u64>,
}

fn new_note_ref(id: String, note_path: PathBuf) -> NoteRef {
    NoteRef {
        id,
        note_path: note_path.to_string_lossy().into_owned(),
    }
}

/// Returns the canonical `~/.margin/notes/` path, exposed to JS so the
/// frontend can determine `isOwned` from a path.
#[tauri::command]
pub fn notes_dir() -> String {
    paths::notes_dir().to_string_lossy().into_owned()
}

/// Create a new owned bundle and return the path to the empty `note.md`.
#[tauri::command]
pub fn create_note() -> Result<NoteRef, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = paths::notes_dir().join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let note_path = dir.join(NOTE_FILENAME);
    fs::write(&note_path, "").map_err(|e| e.to_string())?;
    Ok(new_note_ref(id, note_path))
}

/// Promote an external markdown file to an owned note by copying it into
/// a fresh bundle. The original file is left in place.
#[tauri::command]
pub fn convert_external(source_path: String) -> Result<NoteRef, String> {
    let src = PathBuf::from(&source_path);
    if !src.is_file() {
        return Err("Source file not found".into());
    }
    if src.extension().and_then(|s| s.to_str()) != Some("md") {
        return Err("Only markdown (.md) files can be converted".into());
    }
    if is_under_notes_dir(&src) {
        return Err("This file is already a Margin note".into());
    }

    let id = uuid::Uuid::new_v4().to_string();
    let dir = paths::notes_dir().join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let note_path = dir.join(NOTE_FILENAME);
    fs::copy(&src, &note_path).map_err(|e| e.to_string())?;
    Ok(new_note_ref(id, note_path))
}

/// True iff `path` is `~/.margin/notes/<uuid>/note.md`.
#[tauri::command]
pub fn is_owned_note(path: String) -> bool {
    let p = PathBuf::from(&path);
    if p.file_name().and_then(|s| s.to_str()) != Some(NOTE_FILENAME) {
        return false;
    }
    let parent = match p.parent() {
        Some(p) => p,
        None => return false,
    };
    let grandparent = match parent.parent() {
        Some(p) => p,
        None => return false,
    };
    grandparent == paths::notes_dir().as_path()
}

fn is_under_notes_dir(path: &Path) -> bool {
    let notes = paths::notes_dir();
    path.canonicalize()
        .ok()
        .zip(notes.canonicalize().ok())
        .map(|(p, n)| p.starts_with(n))
        .unwrap_or(false)
}

/// Given any path, return the bundle directory if it lives under
/// `~/.margin/notes/<uuid>/...`. Used to resolve where audio.wav and
/// transcript.json should live for the active note.
pub fn bundle_dir_for(note_path: &Path) -> Option<PathBuf> {
    let parent = note_path.parent()?;
    if parent.parent()? == paths::notes_dir().as_path() {
        Some(parent.to_path_buf())
    } else {
        None
    }
}

/// Scan `~/.margin/notes/*/note.md` and return one item per bundle.
/// Sorted newest-first by mtime.
#[tauri::command]
pub fn list_notes() -> Result<Vec<NoteListItem>, String> {
    let dir = paths::notes_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let bundle_dir = entry.path();
        if !bundle_dir.is_dir() {
            continue;
        }
        let note_path = bundle_dir.join(NOTE_FILENAME);
        if !note_path.exists() {
            continue;
        }

        let meta = match fs::metadata(&note_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let body = fs::read_to_string(&note_path).unwrap_or_default();
        let title = body
            .lines()
            .find_map(|l| {
                let trimmed = l.trim_start();
                trimmed
                    .strip_prefix("# ")
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
            })
            .unwrap_or_else(|| "Untitled note".to_string());

        let transcript_path = bundle_dir.join(TRANSCRIPT_FILENAME);
        let duration_ms = if transcript_path.exists() {
            fs::read_to_string(&transcript_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("duration_ms").and_then(|d| d.as_u64()))
        } else {
            None
        };

        out.push(NoteListItem {
            note_path: note_path.to_string_lossy().into_owned(),
            title,
            modified_ms,
            duration_ms,
        });
    }

    out.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    Ok(out)
}

/// Remove `audio.wav` and `transcript.json` from the bundle. Leaves
/// `note.md` intact (the user might still want their hand-notes minus
/// the recording).
#[tauri::command]
pub fn discard_recording(note_path: String) -> Result<(), String> {
    let p = PathBuf::from(&note_path);
    let dir = bundle_dir_for(&p).ok_or("Not an owned note")?;
    for name in [AUDIO_FILENAME, TRANSCRIPT_FILENAME] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
