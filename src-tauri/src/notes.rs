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

use serde::{Deserialize, Serialize};
use serde_yml::Mapping;

use crate::paths;

/// Per-bundle filename for the note's markdown body.
pub const NOTE_FILENAME: &str = "note.md";
/// Per-bundle filename for the recorded audio (only if a recording exists).
pub const AUDIO_FILENAME: &str = "audio.wav";
/// Per-bundle filename for the transcript sidecar (only if transcribed).
pub const TRANSCRIPT_FILENAME: &str = "transcript.json";
/// In-progress streaming transcript written by the chunked Whisper worker
/// during a meeting. Promoted to `TRANSCRIPT_FILENAME` at end-of-meeting (#24).
pub const TRANSCRIPT_PARTIAL_FILENAME: &str = "transcript-partial.json";

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
    pub preview: String,
    pub tags: Vec<String>,
    pub favorite: bool,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum NoteScope {
    #[default]
    Active,
    Archived,
    Favorites,
    All,
}

#[derive(Serialize)]
pub struct ActionListItem {
    pub id: String,
    pub note_path: String,
    pub note_title: String,
    pub text: String,
    pub done: bool,
    pub line: i64,
    pub created_ms: i64,
    /// Absolute due-date timestamp (Unix ms) parsed from a trailing
    /// `@YYYY-MM-DD[ HH:MM]` token on the action line. `None` means the
    /// action has no due date.
    pub due_ms: Option<i64>,
    /// `team_members.id` when the leading `Owner — ` segment in `text`
    /// matched exactly one team member (#49). `None` when the action has
    /// no owner candidate, the candidate didn't match any member, or
    /// the candidate matched multiple members ambiguously.
    pub assignee_id: Option<String>,
    /// Canonical display name from `team_members`, joined for render so
    /// the frontend can surface an avatar chip without a second
    /// round-trip (#50/#51).
    pub assignee_display_name: Option<String>,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ActionScope {
    #[default]
    Open,
    Done,
    All,
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
pub fn create_note(
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = paths::notes_dir().join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let note_path = dir.join(NOTE_FILENAME);
    fs::write(&note_path, "").map_err(|e| e.to_string())?;
    touch_index(&conn, &note_path, false);
    Ok(new_note_ref(id, note_path))
}

/// Reserved bundle id for the catch-all "Inbox" note that holds quick
/// todos created without a source note. Stable across sessions so the
/// frontend can find-or-create with a single call.
pub const INBOX_BUNDLE_ID: &str = "inbox";

/// Find-or-create the Inbox bundle and return its NoteRef. Quick todos
/// from the Action items page get appended to this note's body via the
/// normal `write_note` round-trip.
#[tauri::command]
pub fn ensure_inbox_note(
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    let dir = paths::notes_dir().join(INBOX_BUNDLE_ID);
    let note_path = dir.join(NOTE_FILENAME);
    if !note_path.exists() {
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        fs::write(&note_path, "# Inbox\n").map_err(|e| e.to_string())?;
        touch_index(&conn, &note_path, false);
    }
    Ok(new_note_ref(INBOX_BUNDLE_ID.to_string(), note_path))
}

/// Promote an external markdown file to an owned note by copying it into
/// a fresh bundle. The original file is left in place.
#[tauri::command]
pub fn convert_external(
    source_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
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
    touch_index(&conn, &note_path, false);
    Ok(new_note_ref(id, note_path))
}

/// Clone an owned note into a fresh bundle. Title and tags carry over;
/// `archived` and `favorite` flags are stripped (they're state, not
/// content — duplicating shouldn't bury the clone in Archive). The
/// audio.wav / transcript.json sidecars are intentionally not copied
/// since a duplicate is for editorial work, not a bit-for-bit clone of
/// a recording. Title-suffix convention: verbatim, matching macOS
/// Notes / Bear (no "(copy)").
#[tauri::command]
pub fn duplicate_note(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    let src = PathBuf::from(&note_path);
    if !is_owned_note_in(&src, &paths::notes_dir()) {
        return Err("Refusing to duplicate: not an owned note path".into());
    }
    if !src.is_file() {
        return Err("Source note not found".into());
    }
    let raw = fs::read_to_string(&src).map_err(|e| e.to_string())?;
    let (yaml, body) = split_frontmatter(&raw);
    let mut map = yaml.map(parse_frontmatter).unwrap_or_default();
    set_bool_key(&mut map, "archived", false);
    set_bool_key(&mut map, "favorite", false);
    let merged = write_with_frontmatter(&map, body);

    let id = uuid::Uuid::new_v4().to_string();
    let dir = paths::notes_dir().join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(NOTE_FILENAME);
    fs::write(&dest, merged).map_err(|e| e.to_string())?;
    touch_index(&conn, &dest, false);
    Ok(new_note_ref(id, dest))
}

/// True iff `path` is `~/.margin/notes/<uuid>/note.md`.
#[tauri::command]
pub fn is_owned_note(path: String) -> bool {
    is_owned_note_in(Path::new(&path), &paths::notes_dir())
}

pub(crate) fn is_owned_note_in(path: &Path, notes_dir: &Path) -> bool {
    if path.file_name().and_then(|s| s.to_str()) != Some(NOTE_FILENAME) {
        return false;
    }
    match path.parent().and_then(|p| p.parent()) {
        Some(gp) => gp == notes_dir,
        None => false,
    }
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
    bundle_dir_for_in(note_path, &paths::notes_dir())
}

pub(crate) fn bundle_dir_for_in(note_path: &Path, notes_dir: &Path) -> Option<PathBuf> {
    let parent = note_path.parent()?;
    if parent.parent()? == notes_dir {
        Some(parent.to_path_buf())
    } else {
        None
    }
}

/// Return all owned notes, newest-first by `modified_ms`. Default scope
/// is `Active` (excludes archived). Reads from the SQLite index — see
/// `index.rs`. The index stays in sync via the recursive notes-dir
/// watcher in `lib.rs` plus the per-command upsert calls below.
#[tauri::command]
pub fn list_notes(
    scope: Option<NoteScope>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<NoteListItem>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    crate::index::list_all(&c, scope.unwrap_or_default()).map_err(|e| e.to_string())
}

/// Return action items across all non-archived owned notes, scoped to
/// open / done / all. Default `Open`. Joins on the notes table for the
/// source note's title.
#[tauri::command]
pub fn list_actions(
    scope: Option<ActionScope>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<ActionListItem>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    crate::index::list_actions(&c, scope.unwrap_or_default()).map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct NoteMeta {
    pub modified_ms: i64,
}

/// Read mtime for a single note path. Used by the note header's date chip.
/// Cheaper than calling `list_notes` and filtering on the JS side.
#[tauri::command]
pub fn note_meta(note_path: String) -> Result<NoteMeta, String> {
    let p = PathBuf::from(&note_path);
    let meta = fs::metadata(&p).map_err(|e| e.to_string())?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(NoteMeta { modified_ms })
}

// ---------- Frontmatter ---------------------------------------------------

const TAG_MAX_LEN: usize = 32;
const TAGS_MAX_PER_NOTE: usize = 16;

#[derive(Serialize, Deserialize)]
pub struct NoteContent {
    pub body: String,
    pub tags: Vec<String>,
    pub archived: bool,
    pub favorite: bool,
    /// Frontmatter keys other than `tags`/`archived`/`favorite`, preserved
    /// verbatim. Round-trips through the frontend so user-added YAML
    /// survives a save.
    pub frontmatter_extras: Mapping,
}

/// Split a leading YAML frontmatter block off the raw note text. Returns
/// `(yaml_chunk_without_delimiters, body)`. If no frontmatter is present
/// (or the closing `---` is missing), returns `(None, raw)`.
pub(crate) fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    // Be lenient about a leading BOM, but don't otherwise allow whitespace
    // before the opening delimiter.
    let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    let after_open = match stripped.strip_prefix("---\n") {
        Some(rest) => rest,
        None => return (None, raw),
    };
    // Find the next `---` on its own line.
    let mut search_from = 0usize;
    while search_from < after_open.len() {
        let idx = match after_open[search_from..].find("\n---") {
            Some(i) => search_from + i,
            None => return (None, raw), // no closing delimiter; not frontmatter
        };
        let after_delim = idx + "\n---".len();
        // Accept either `\n---\n…` or `\n---` at EOF.
        let body_start = if after_open[after_delim..].starts_with('\n') {
            after_delim + 1
        } else if after_open.len() == after_delim {
            after_delim
        } else {
            // `---` followed by other characters (e.g. `---foo`) — not a
            // closer; keep searching.
            search_from = after_delim;
            continue;
        };
        return (Some(&after_open[..idx]), &after_open[body_start..]);
    }
    (None, raw)
}

pub(crate) fn parse_frontmatter(yaml: &str) -> Mapping {
    serde_yml::from_str::<Mapping>(yaml).unwrap_or_default()
}

pub(crate) fn read_archived(map: &Mapping) -> bool {
    read_bool_flag(map, "archived")
}

pub(crate) fn read_favorite(map: &Mapping) -> bool {
    read_bool_flag(map, "favorite")
}

fn read_bool_flag(map: &Mapping, key: &str) -> bool {
    match map.get(serde_yml::Value::String(key.into())) {
        Some(serde_yml::Value::Bool(b)) => *b,
        // Tolerate `<key>: "true"` / `"yes"` / `"1"` from hand-edited
        // frontmatter; everything else is false.
        Some(serde_yml::Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1")
        }
        _ => false,
    }
}

pub(crate) fn read_tags(map: &Mapping) -> Vec<String> {
    let raw = match map.get(serde_yml::Value::String("tags".into())) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let items: Vec<&str> = match raw {
        serde_yml::Value::Sequence(seq) => seq.iter().filter_map(|v| v.as_str()).collect(),
        // Tolerate a single string for `tags: foo` style.
        serde_yml::Value::String(s) => vec![s.as_str()],
        _ => return Vec::new(),
    };
    normalize_tags(items.iter().map(|s| s.to_string()))
}

fn normalize_tags<I: IntoIterator<Item = String>>(input: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in input {
        let trimmed = raw.trim().to_lowercase();
        if trimmed.is_empty() || trimmed.len() > TAG_MAX_LEN {
            continue;
        }
        if seen.insert(trimmed.clone()) {
            out.push(trimmed);
            if out.len() >= TAGS_MAX_PER_NOTE {
                break;
            }
        }
    }
    out
}

/// Re-emit a note with the given frontmatter mapping prepended to `body`.
/// Empty mapping → no frontmatter block at all.
fn write_with_frontmatter(map: &Mapping, body: &str) -> String {
    if map.is_empty() {
        return body.to_string();
    }
    let yaml = serde_yml::to_string(map).unwrap_or_default();
    format!("---\n{yaml}---\n{body}")
}

/// Parse a note from disk into body + tags + archived + extras. Used by
/// the editor flow so the textarea never sees the YAML.
#[tauri::command]
pub fn read_note(note_path: String) -> Result<NoteContent, String> {
    let raw = fs::read_to_string(&note_path).map_err(|e| e.to_string())?;
    let (yaml, body) = split_frontmatter(&raw);
    let mut map = yaml.map(parse_frontmatter).unwrap_or_default();
    let tags = read_tags(&map);
    let archived = read_archived(&map);
    let favorite = read_favorite(&map);
    map.remove(serde_yml::Value::String("tags".into()));
    map.remove(serde_yml::Value::String("archived".into()));
    map.remove(serde_yml::Value::String("favorite".into()));
    Ok(NoteContent {
        body: body.to_string(),
        tags,
        archived,
        favorite,
        frontmatter_extras: map,
    })
}

/// Result envelope for `write_note`. `rewritten_body` is `Some` when the
/// Rust side rewrote relative due-date tokens (`@today`, `@tomorrow`,
/// `@<weekday>`) to their absolute `@YYYY-MM-DD` forms — the frontend
/// uses it to swap the editor's in-memory text so it stays in sync with
/// disk.
#[derive(Serialize)]
pub struct WriteNoteResult {
    pub rewritten_body: Option<String>,
}

/// Write a note by merging tags + extras into a frontmatter block above
/// the body. The caller must pass back any `frontmatter_extras` it got
/// from `read_note` so unknown keys round-trip unchanged.
#[tauri::command]
pub fn write_note(
    note_path: String,
    body: String,
    tags: Vec<String>,
    archived: bool,
    favorite: bool,
    frontmatter_extras: Mapping,
    guard: tauri::State<'_, crate::WriteGuard>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<WriteNoteResult, String> {
    let today = chrono::Local::now().date_naive();
    let (final_body, rewritten_body) = match rewrite_relative_due_tokens(&body, today) {
        Some(new_body) => {
            let echo = new_body.clone();
            (new_body, Some(echo))
        }
        None => (body, None),
    };

    let normalized = normalize_tags(tags);
    let mut map = frontmatter_extras;
    if !normalized.is_empty() {
        let seq: Vec<serde_yml::Value> = normalized
            .into_iter()
            .map(serde_yml::Value::String)
            .collect();
        map.insert(
            serde_yml::Value::String("tags".into()),
            serde_yml::Value::Sequence(seq),
        );
    } else {
        map.remove(serde_yml::Value::String("tags".into()));
    }
    set_bool_key(&mut map, "archived", archived);
    set_bool_key(&mut map, "favorite", favorite);
    let merged = write_with_frontmatter(&map, &final_body);
    fs::write(&note_path, merged).map_err(|e| e.to_string())?;
    *guard.last_write.lock().map_err(|e| e.to_string())? = Some(std::time::Instant::now());
    touch_index(&conn, Path::new(&note_path), false);
    Ok(WriteNoteResult { rewritten_body })
}

fn set_bool_key(map: &mut Mapping, key: &str, value: bool) {
    if value {
        map.insert(
            serde_yml::Value::String(key.into()),
            serde_yml::Value::Bool(true),
        );
    } else {
        map.remove(serde_yml::Value::String(key.into()));
    }
}

/// Convenience for header chip mutations: read → replace tags → write.
/// Doesn't touch the body, so an in-flight editor buffer isn't disturbed.
#[tauri::command]
pub fn set_note_tags(
    note_path: String,
    tags: Vec<String>,
    guard: tauri::State<'_, crate::WriteGuard>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let raw = fs::read_to_string(&note_path).map_err(|e| e.to_string())?;
    let (yaml, body) = split_frontmatter(&raw);
    let mut map = yaml.map(parse_frontmatter).unwrap_or_default();
    let normalized = normalize_tags(tags);
    if normalized.is_empty() {
        map.remove(serde_yml::Value::String("tags".into()));
    } else {
        let seq: Vec<serde_yml::Value> = normalized
            .into_iter()
            .map(serde_yml::Value::String)
            .collect();
        map.insert(
            serde_yml::Value::String("tags".into()),
            serde_yml::Value::Sequence(seq),
        );
    }
    let merged = write_with_frontmatter(&map, body);
    fs::write(&note_path, merged).map_err(|e| e.to_string())?;
    *guard.last_write.lock().map_err(|e| e.to_string())? = Some(std::time::Instant::now());
    touch_index(&conn, Path::new(&note_path), false);
    Ok(())
}

/// Flip the archived flag on a note's frontmatter. Doesn't disturb the
/// body, tags, or any other frontmatter — same surgical pattern as
/// `set_note_tags`.
#[tauri::command]
pub fn set_archived(
    note_path: String,
    archived: bool,
    guard: tauri::State<'_, crate::WriteGuard>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    set_bool_in_frontmatter(&note_path, "archived", archived, &guard, &conn)
}

/// Toggle the done state of an action item by its derived id. Looks up
/// the action's source note, finds the line (via cached line number
/// first, then by re-scanning the body for the text-hash), flips the
/// `[ ]`/`[x]` marker, and writes the file back through the existing
/// frontmatter round-trip. Index refresh happens via `touch_index`.
#[tauri::command]
pub fn set_action_done(
    id: String,
    done: bool,
    guard: tauri::State<'_, crate::WriteGuard>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let (note_path, cached_line, want_text) = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.query_row(
            "SELECT note_path, line, text FROM actions WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?
    };

    let raw = fs::read_to_string(&note_path).map_err(|e| e.to_string())?;
    let (yaml, body) = split_frontmatter(&raw);
    let mut lines: Vec<String> = body.split('\n').map(|s| s.to_string()).collect();
    let want_hash = action_text_hash(&want_text);

    let mut target_idx: Option<usize> = None;
    if cached_line >= 1 && cached_line <= lines.len() {
        if let Some((line_text, _, _)) = parse_action_line(lines[cached_line - 1].trim_start()) {
            if action_text_hash(&line_text) == want_hash {
                target_idx = Some(cached_line - 1);
            }
        }
    }
    if target_idx.is_none() {
        for (i, line) in lines.iter().enumerate() {
            if let Some((line_text, _, _)) = parse_action_line(line.trim_start()) {
                if action_text_hash(&line_text) == want_hash {
                    target_idx = Some(i);
                    break;
                }
            }
        }
    }
    let idx = target_idx.ok_or_else(|| {
        "Action not found in note (index may be stale; reload to refresh)".to_string()
    })?;
    lines[idx] = toggle_checkbox_marker(&lines[idx], done);
    let new_body = lines.join("\n");

    let map = yaml.map(parse_frontmatter).unwrap_or_default();
    let merged = write_with_frontmatter(&map, &new_body);
    fs::write(&note_path, merged).map_err(|e| e.to_string())?;
    *guard.last_write.lock().map_err(|e| e.to_string())? = Some(std::time::Instant::now());
    touch_index(&conn, Path::new(&note_path), false);
    Ok(())
}

/// Replace the character between `[` and `]` on the first checkbox the
/// line contains. Preserves indentation, bullet character, and
/// trailing text/whitespace. Always normalizes done to lowercase `x`.
fn toggle_checkbox_marker(line: &str, done: bool) -> String {
    if let Some(open) = line.find('[') {
        let close = open + 2;
        if line.as_bytes().get(close) == Some(&b']') {
            let mut out = String::with_capacity(line.len());
            out.push_str(&line[..open + 1]);
            out.push(if done { 'x' } else { ' ' });
            out.push_str(&line[open + 2..]);
            return out;
        }
    }
    line.to_string()
}

/// Flip the favorite flag on a note's frontmatter. Surgical, same shape
/// as `set_archived`.
#[tauri::command]
pub fn set_favorite(
    note_path: String,
    favorite: bool,
    guard: tauri::State<'_, crate::WriteGuard>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    set_bool_in_frontmatter(&note_path, "favorite", favorite, &guard, &conn)
}

fn set_bool_in_frontmatter(
    note_path: &str,
    key: &str,
    value: bool,
    guard: &tauri::State<'_, crate::WriteGuard>,
    conn: &tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let raw = fs::read_to_string(note_path).map_err(|e| e.to_string())?;
    let (yaml, body) = split_frontmatter(&raw);
    let mut map = yaml.map(parse_frontmatter).unwrap_or_default();
    set_bool_key(&mut map, key, value);
    let merged = write_with_frontmatter(&map, body);
    fs::write(note_path, merged).map_err(|e| e.to_string())?;
    *guard.last_write.lock().map_err(|e| e.to_string())? = Some(std::time::Instant::now());
    touch_index(conn, Path::new(note_path), false);
    Ok(())
}

// ---------- Action items (markdown checkboxes) ---------------------------

#[derive(Clone)]
pub(crate) struct ParsedAction {
    pub line: usize,
    pub text: String,
    pub done: bool,
    /// Absolute due-date timestamp (Unix ms) if a recognized
    /// `@YYYY-MM-DD[ HH:MM]` token trails the action text. Relative
    /// tokens (`@today`, `@tomorrow`, `@<weekday>`) are stripped from
    /// `text` but leave `due_ms` as `None`; they get normalized to
    /// absolute on the next `write_note` call.
    pub due_ms: Option<i64>,
    /// Leading `Owner — ` segment extracted from `text`, or `None` when
    /// the line has no recognizable space-flanked separator (#49). The
    /// candidate name is preserved verbatim — case + accents — so the
    /// resolver in `team::OwnerResolver` can normalize once and the raw
    /// form is still available for diagnostics.
    pub owner_candidate: Option<String>,
}

/// Walk a note body and return every markdown task line as a
/// ParsedAction. Lines inside fenced code blocks are skipped (mirrors
/// the heuristic used by `extract_preview` for prose extraction).
pub(crate) fn parse_actions(body: &str) -> Vec<ParsedAction> {
    let mut out = Vec::new();
    let mut in_code_fence = false;
    for (i, raw) in body.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        if let Some((text, done, due_ms)) = parse_action_line(trimmed) {
            let owner_candidate = extract_owner_candidate(&text);
            out.push(ParsedAction {
                line: i + 1,
                text,
                done,
                due_ms,
                owner_candidate,
            });
        }
    }
    out
}

/// Extract a leading `{Owner} {sep} ` segment from action text where
/// `{sep}` is one of `—` (em-dash), `–` (en-dash), or `--` (double
/// hyphen). The separator must be flanked by spaces — bare hyphens in
/// natural language (`self-driving`, `Tom—task`) are never treated as
/// owner separators (#49). Returns the trimmed owner candidate, or
/// `None`.
pub(crate) fn extract_owner_candidate(text: &str) -> Option<String> {
    const SEPARATORS: &[&str] = &[" — ", " – ", " -- "];
    let mut earliest: Option<usize> = None;
    for sep in SEPARATORS {
        if let Some(idx) = text.find(sep) {
            earliest = Some(match earliest {
                Some(cur) => cur.min(idx),
                None => idx,
            });
        }
    }
    let cut = earliest?;
    let owner = text[..cut].trim();
    if owner.is_empty() {
        return None;
    }
    Some(owner.to_string())
}

fn parse_action_line(line: &str) -> Option<(String, bool, Option<i64>)> {
    let after_bullet = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;
    let bytes = after_bullet.as_bytes();
    // Need `[X] x` (4 ASCII bytes plus the body) — the body itself must
    // be non-empty after trimming.
    if bytes.len() < 4 || bytes[0] != b'[' || bytes[2] != b']' || bytes[3] != b' ' {
        return None;
    }
    let done = match bytes[1] {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    let raw_text = after_bullet[4..].trim();
    if raw_text.is_empty() {
        return None;
    }
    let (text, due_ms) = strip_trailing_due_token(raw_text);
    if text.is_empty() {
        // Token consumed the entire body. Treat as a no-text action and
        // skip — same rule as the bare `- [ ]` case.
        return None;
    }
    Some((text, done, due_ms))
}

/// If `s` ends with a recognized ` @<token>`, return `(text_without_token,
/// due_ms)` where `due_ms` is `Some` for absolute ISO tokens and `None`
/// for relative ones (still stripped from text). If no recognizable token
/// trails, returns the input unchanged with `due_ms = None`.
fn strip_trailing_due_token(s: &str) -> (String, Option<i64>) {
    let Some((prefix, token)) = take_trailing_at_token(s) else {
        return (s.to_string(), None);
    };
    if let Some(due) = crate::dates::try_parse_absolute(&token) {
        return (prefix, Some(due.timestamp_ms));
    }
    if crate::dates::is_relative(&token) {
        return (prefix, None);
    }
    // Unrecognized → leave the @token in the text so the user can see
    // and fix the typo. Don't punish them with a vanishing date.
    (s.to_string(), None)
}

/// Resolve any trailing relative `@<token>` (today/tomorrow/weekday) on
/// checkbox lines to its absolute `@YYYY-MM-DD` form, against `today`.
/// Returns `Some(new_body)` if at least one substitution happened,
/// `None` if the body was already canonical. Code-fenced lines are
/// skipped — same heuristic as `parse_actions`.
pub(crate) fn rewrite_relative_due_tokens(
    body: &str,
    today: chrono::NaiveDate,
) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut in_code_fence = false;
    let mut changed = false;
    let mut first = true;
    for raw in body.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            out.push_str(raw);
            continue;
        }
        if in_code_fence {
            out.push_str(raw);
            continue;
        }
        let Some(rewritten) = rewrite_checkbox_line(raw, today) else {
            out.push_str(raw);
            continue;
        };
        out.push_str(&rewritten);
        changed = true;
    }
    if changed {
        Some(out)
    } else {
        None
    }
}

/// If `line` is a checkbox line with a trailing relative `@<token>`,
/// return the line with that token swapped for `@<absolute>`. Returns
/// `None` if the line is not a checkbox, has no token, or the token is
/// already absolute / unrecognized.
fn rewrite_checkbox_line(line: &str, today: chrono::NaiveDate) -> Option<String> {
    // Detect leading whitespace and bullet so we can preserve them.
    let leading_ws_len = line.len() - line.trim_start().len();
    let trimmed = &line[leading_ws_len..];
    let (bullet_len, after_bullet) = if let Some(rest) = trimmed.strip_prefix("- ") {
        (2, rest)
    } else if let Some(rest) = trimmed.strip_prefix("* ") {
        (2, rest)
    } else if let Some(rest) = trimmed.strip_prefix("+ ") {
        (2, rest)
    } else {
        return None;
    };
    let bytes = after_bullet.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'[' || bytes[2] != b']' || bytes[3] != b' ' {
        return None;
    }
    if !matches!(bytes[1], b' ' | b'x' | b'X') {
        return None;
    }
    let body_after_checkbox = &after_bullet[4..];
    // Preserve trailing whitespace verbatim (we only rewrite the token).
    let body_trimmed = body_after_checkbox.trim_end();
    let trailing_ws = &body_after_checkbox[body_trimmed.len()..];

    let (prefix, token) = take_trailing_at_token(body_trimmed)?;
    if !crate::dates::is_relative(&token) {
        return None;
    }
    let due = crate::dates::parse_due_token(&token, today)?;
    let absolute = crate::dates::render_absolute(&due);

    let mut out = String::with_capacity(line.len() + 8);
    out.push_str(&line[..leading_ws_len]);
    out.push_str(&trimmed[..bullet_len]);
    out.push_str(&after_bullet[..4]);
    out.push_str(&prefix);
    out.push(' ');
    out.push('@');
    out.push_str(&absolute);
    out.push_str(trailing_ws);
    Some(out)
}

/// Find the last `@` in `s` that's at the start or preceded by whitespace,
/// and return `(text_before_token, token_after_at)`. The token may itself
/// contain a single space (for the `@YYYY-MM-DD HH:MM` form). Returns
/// `None` if no candidate `@` is present, the token is empty, or the
/// prefix collapses to empty.
fn take_trailing_at_token(s: &str) -> Option<(String, String)> {
    let bytes = s.as_bytes();
    let mut at_pos: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            at_pos = Some(i);
        }
    }
    let pos = at_pos?;
    let prefix = s[..pos].trim_end().to_string();
    let token = s[pos + 1..].trim().to_string();
    if prefix.is_empty() || token.is_empty() {
        return None;
    }
    Some((prefix, token))
}

/// Stable per-text hash for action IDs. FNV-1a 64-bit, keep low 32 bits
/// as 8 hex chars. No new dep; deterministic across builds.
///
/// The hash is computed over the *stripped* `text` (trailing `@<token>`
/// already removed by `parse_action_line`), so insert-side and
/// `set_action_done` lookup-side hashes always match. Any future change
/// to what gets stripped from the action body must be applied
/// symmetrically on both sides or row identity will drift.
pub(crate) fn action_text_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", h as u32)
}

pub(crate) fn action_id(bundle_id: &str, text: &str) -> String {
    format!("{bundle_id}:{}", action_text_hash(text))
}

const PREVIEW_MAX_CHARS: usize = 160;

/// Best-effort plaintext snippet of the first non-empty paragraph of a
/// markdown body. Skips headings and code-fence delimiters; strips
/// list/quote markers and inline emphasis/links so the result reads as
/// prose. Truncates to ~160 chars at a word boundary with `…`.
pub(crate) fn extract_preview(body: &str) -> String {
    let mut paragraph: Vec<String> = Vec::new();
    let mut in_code_fence = false;
    let mut started = false;

    for raw in body.lines() {
        let line = raw.trim();

        if line.starts_with("```") || line.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        if line.is_empty() {
            if started {
                break;
            }
            continue;
        }
        if line.starts_with('#') {
            // Skip ATX headings of any level (#, ##, …).
            continue;
        }

        let cleaned = strip_inline_markdown(strip_block_markers(line));
        if cleaned.is_empty() {
            continue;
        }
        paragraph.push(cleaned);
        started = true;
    }

    let joined = collapse_whitespace(&paragraph.join(" "));
    truncate_with_ellipsis(&joined, PREVIEW_MAX_CHARS)
}

/// Trim leading list / blockquote / numbered-list markers from a line.
fn strip_block_markers(line: &str) -> &str {
    let l = line.trim_start();
    // Blockquote: `> `, possibly nested `> > `.
    let mut rest = l;
    while let Some(next) = rest.strip_prefix("> ").or_else(|| rest.strip_prefix(">")) {
        rest = next.trim_start();
    }
    // Bullet list: `- `, `* `, `+ `.
    if let Some(next) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        return next;
    }
    // Numbered list: `1. `, `12. `, etc.
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b' ' {
        return &rest[i + 2..];
    }
    rest
}

/// Remove inline markdown decorations: emphasis markers (`*`, `_`),
/// inline code backticks, and link/image syntax (keeping the visible
/// text only). Intentionally simple — this isn't a full parser.
fn strip_inline_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'*' | b'_' | b'`' => {
                // Skip the marker; surrounding spaces handle word boundaries.
                i += 1;
            }
            b'!' if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                // Image: `![alt](url)` → drop entirely.
                i += 2;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // consume ']'
                }
                if i < bytes.len() && bytes[i] == b'(' {
                    while i < bytes.len() && bytes[i] != b')' {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
            }
            b'[' => {
                // Link: `[text](url)` → keep text only.
                i += 1;
                let text_start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                let text_end = i;
                if i < bytes.len() {
                    i += 1; // consume ']'
                }
                if i < bytes.len() && bytes[i] == b'(' {
                    while i < bytes.len() && bytes[i] != b')' {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                out.push_str(&input[text_start..text_end]);
            }
            _ => {
                // UTF-8 safe: copy through using char iteration when
                // we hit a non-ASCII byte. The simple cases above are
                // all ASCII so this branch is the fallback.
                let ch_start = i;
                let ch_len = utf8_char_len(b);
                let ch_end = (i + ch_len).min(bytes.len());
                out.push_str(&input[ch_start..ch_end]);
                i = ch_end;
            }
        }
    }
    out
}

fn utf8_char_len(first: u8) -> usize {
    match first {
        0..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    // Find the byte index of the (max_chars+1)th char; back up to the
    // last word boundary at or before that point.
    let cut_byte = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let prefix = &s[..cut_byte];
    let trimmed = match prefix.rfind(|c: char| c.is_whitespace()) {
        Some(idx) if idx > 0 => &prefix[..idx],
        _ => prefix,
    };
    let mut out = trimmed.trim_end().to_string();
    out.push('…');
    out
}

/// Remove `audio.wav` and `transcript.json` from the bundle. Leaves
/// `note.md` intact (the user might still want their hand-notes minus
/// the recording).
#[tauri::command]
pub fn discard_recording(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let p = PathBuf::from(&note_path);
    let dir = bundle_dir_for(&p).ok_or("Not an owned note")?;
    for name in [AUDIO_FILENAME, TRANSCRIPT_FILENAME] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
    }
    // duration_ms cleared from the indexed row.
    touch_index(&conn, &p, false);
    Ok(())
}

/// Delete an owned note bundle entirely (note.md, audio.wav,
/// transcript.json, anything else under the bundle dir). Hard delete —
/// recoverability is the Archive feature's job (#17).
///
/// Refuses non-owned paths, so a path that slips through the IPC layer
/// can't ask us to nuke arbitrary directories.
#[tauri::command]
pub fn delete_note(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let p = PathBuf::from(&note_path);
    delete_note_in(&p, &paths::notes_dir())?;
    touch_index(&conn, &p, true);
    Ok(())
}

fn delete_note_in(p: &Path, notes_dir: &Path) -> Result<(), String> {
    if !is_owned_note_in(p, notes_dir) {
        return Err("Refusing to delete: not an owned note path".into());
    }
    let dir = bundle_dir_for_in(p, notes_dir).ok_or("Could not resolve bundle directory")?;
    if !dir.is_dir() {
        return Err("Bundle directory missing".into());
    }
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(())
}

/// Refresh the index for `note_path`. `removed=true` drops the row;
/// otherwise re-reads the file and upserts. Failures are logged so a
/// transient SQLite error doesn't surface as an IPC error to the user —
/// the next watcher event or boot reconcile heals.
fn touch_index(
    conn_state: &tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
    note_path: &Path,
    removed: bool,
) {
    let mut c = match conn_state.lock() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("index lock poisoned: {e}");
            return;
        }
    };
    let result = if removed {
        crate::index::remove(&mut c, note_path)
    } else {
        crate::index::upsert(&mut c, note_path)
    };
    if let Err(e) = result {
        eprintln!("index touch failed for {note_path:?}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_bundle(notes_dir: &Path, id: &str) -> PathBuf {
        let dir = notes_dir.join(id);
        fs::create_dir_all(&dir).unwrap();
        let note = dir.join(NOTE_FILENAME);
        fs::write(&note, "# hi\n").unwrap();
        note
    }

    #[test]
    fn read_archived_default_false() {
        let map: Mapping = serde_yml::from_str("tags: []").unwrap();
        assert!(!read_archived(&map));
    }

    #[test]
    fn read_archived_true() {
        let map: Mapping = serde_yml::from_str("archived: true").unwrap();
        assert!(read_archived(&map));
    }

    #[test]
    fn read_archived_false_explicit() {
        let map: Mapping = serde_yml::from_str("archived: false").unwrap();
        assert!(!read_archived(&map));
    }

    #[test]
    fn read_archived_tolerates_string_yes() {
        let map: Mapping = serde_yml::from_str("archived: \"yes\"").unwrap();
        assert!(read_archived(&map));
    }

    #[test]
    fn read_favorite_default_false() {
        let map: Mapping = serde_yml::from_str("tags: []").unwrap();
        assert!(!read_favorite(&map));
    }

    #[test]
    fn read_favorite_true() {
        let map: Mapping = serde_yml::from_str("favorite: true").unwrap();
        assert!(read_favorite(&map));
    }

    #[test]
    fn parse_actions_open_and_done() {
        let body = "intro\n- [ ] alpha\n- [x] beta\n- [X] gamma\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].line, 2);
        assert_eq!(got[0].text, "alpha");
        assert!(!got[0].done);
        assert_eq!(got[1].text, "beta");
        assert!(got[1].done);
        assert_eq!(got[2].text, "gamma");
        assert!(got[2].done);
    }

    #[test]
    fn parse_actions_alt_bullets() {
        let body = "* [ ] starred\n+ [x] plussed\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "starred");
        assert!(got[1].done);
    }

    #[test]
    fn parse_actions_skips_code_fences() {
        let body = "intro\n```\n- [ ] inside fence\n```\n- [ ] after fence\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "after fence");
    }

    #[test]
    fn parse_actions_skips_non_checkbox_lines() {
        let body = "- regular bullet\n- [text]\n- [ ]nospace\n- [ ]\n";
        let got = parse_actions(body);
        assert!(got.is_empty(), "got: {:?}", got.iter().map(|a| &a.text).collect::<Vec<_>>());
    }

    #[test]
    fn action_text_hash_stable_for_same_text() {
        assert_eq!(action_text_hash("hello"), action_text_hash("hello"));
    }

    #[test]
    fn parse_actions_user_repro_real_lines() {
        // Lines copied verbatim from a real note that wasn't getting
        // chips on the Action items page.
        let body = "- [ ] Follow up with Staatsanwaltschaft Heilbronn; propose Rahmenvertrag. @2026-05-11\n\
                    - [ ] Send SUND login by Friday (refine over weekend if needed). @2026-05-08\n\
                    - [x] Add upgrade button/page to Bridge app today (within 1\u{2013}2 hours), then notify so Siegfried email can go out. @2026-05-07 00:00\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 3, "expected three actions, got: {:?}", got.iter().map(|a| &a.text).collect::<Vec<_>>());
        for a in &got {
            assert!(a.due_ms.is_some(), "due_ms missing for: {:?}", a.text);
            assert!(!a.text.contains('@'), "text not stripped: {:?}", a.text);
        }
    }

    #[test]
    fn parse_actions_strips_absolute_due_token() {
        let body = "- [ ] Submit invoice @2026-05-15\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Submit invoice");
        assert!(got[0].due_ms.is_some());
    }

    #[test]
    fn parse_actions_strips_absolute_due_with_time() {
        let body = "- [ ] Stand-up @2026-05-15 09:00\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Stand-up");
        assert!(got[0].due_ms.is_some());
    }

    #[test]
    fn parse_actions_strips_relative_token_but_no_due_ms() {
        // Relative tokens are recognized and stripped from text, but due_ms
        // stays None until rewrite_relative_due_tokens runs at write_note time.
        let body = "- [ ] Schedule retro @tomorrow\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Schedule retro");
        assert!(got[0].due_ms.is_none());
    }

    #[test]
    fn parse_actions_leaves_unrecognized_token_in_text() {
        let body = "- [ ] Email someone@example.com\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Email someone@example.com");
        assert!(got[0].due_ms.is_none());
    }

    #[test]
    fn parse_actions_leaves_garbage_at_token_in_text() {
        let body = "- [ ] Task @notadate\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Task @notadate");
        assert!(got[0].due_ms.is_none());
    }

    #[test]
    fn rewrite_relative_due_tokens_substitutes_in_place() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let body = "# Plan\n\n- [ ] Schedule retro @tomorrow\n- [ ] Pay invoice @2026-06-01\n";
        let rewritten = rewrite_relative_due_tokens(body, today).unwrap();
        assert!(rewritten.contains("@2026-05-08"));
        assert!(rewritten.contains("@2026-06-01"));
        assert!(!rewritten.contains("@tomorrow"));
    }

    #[test]
    fn rewrite_relative_due_tokens_returns_none_when_already_canonical() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let body = "- [ ] Pay invoice @2026-06-01\n- [ ] No date\n";
        assert!(rewrite_relative_due_tokens(body, today).is_none());
    }

    #[test]
    fn rewrite_relative_due_tokens_skips_code_fences() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let body = "```\n- [ ] Inside fence @tomorrow\n```\n- [ ] After fence @tomorrow\n";
        let rewritten = rewrite_relative_due_tokens(body, today).unwrap();
        assert!(rewritten.contains("@tomorrow"), "fenced line keeps token");
        assert!(rewritten.contains("@2026-05-08"));
    }

    #[test]
    fn action_text_hash_distinct_for_different_text() {
        assert_ne!(action_text_hash("foo"), action_text_hash("bar"));
    }

    #[test]
    fn read_favorite_tolerates_string_yes() {
        let map: Mapping = serde_yml::from_str("favorite: \"yes\"").unwrap();
        assert!(read_favorite(&map));
    }

    #[test]
    fn rejects_path_with_wrong_filename() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let bundle = notes.join("abc");
        fs::create_dir_all(&bundle).unwrap();
        let bogus = bundle.join("audio.wav");
        fs::write(&bogus, b"").unwrap();
        assert!(delete_note_in(&bogus, &notes).is_err());
        assert!(bundle.exists(), "bundle must remain after rejection");
    }

    #[test]
    fn rejects_path_outside_notes_dir() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        let elsewhere = tmp.path().join("elsewhere").join("xyz");
        fs::create_dir_all(&elsewhere).unwrap();
        let stray = elsewhere.join(NOTE_FILENAME);
        fs::write(&stray, b"").unwrap();
        assert!(delete_note_in(&stray, &notes).is_err());
        assert!(stray.exists(), "stray file must remain after rejection");
    }

    #[test]
    fn rejects_path_with_no_grandparent() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let lone = PathBuf::from(NOTE_FILENAME);
        assert!(delete_note_in(&lone, &notes).is_err());
    }

    #[test]
    fn deletes_owned_bundle() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = make_bundle(&notes, "11111111-1111-1111-1111-111111111111");
        let bundle = note.parent().unwrap().to_path_buf();
        assert!(bundle.exists());
        delete_note_in(&note, &notes).unwrap();
        assert!(!bundle.exists(), "bundle dir should be gone");
    }

    #[test]
    fn errors_when_bundle_already_missing() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        fs::create_dir_all(&notes).unwrap();
        let phantom = notes.join("ghost").join(NOTE_FILENAME);
        assert!(delete_note_in(&phantom, &notes).is_err());
    }

    // ---------- #49 owner extraction ----------------------------------

    #[test]
    fn extract_owner_candidate_em_dash_with_spaces() {
        assert_eq!(
            extract_owner_candidate("Tom — write spec"),
            Some("Tom".into())
        );
    }

    #[test]
    fn extract_owner_candidate_en_dash() {
        assert_eq!(
            extract_owner_candidate("Tom – write spec"),
            Some("Tom".into())
        );
    }

    #[test]
    fn extract_owner_candidate_double_hyphen() {
        assert_eq!(
            extract_owner_candidate("Tom -- write spec"),
            Some("Tom".into())
        );
    }

    #[test]
    fn extract_owner_candidate_handles_full_names() {
        assert_eq!(
            extract_owner_candidate("Sarah Smith — review the deck"),
            Some("Sarah Smith".into())
        );
    }

    #[test]
    fn extract_owner_candidate_rejects_compact_dashes() {
        assert_eq!(extract_owner_candidate("Tom—task"), None);
        assert_eq!(extract_owner_candidate("Refactor self-driving cars"), None);
    }

    #[test]
    fn extract_owner_candidate_rejects_no_separator() {
        assert_eq!(extract_owner_candidate("write spec"), None);
        assert_eq!(extract_owner_candidate(""), None);
    }

    #[test]
    fn extract_owner_candidate_takes_first_separator_only() {
        assert_eq!(
            extract_owner_candidate("Tom — finalize the API — by Friday"),
            Some("Tom".into())
        );
    }

    #[test]
    fn parse_actions_populates_owner_candidate() {
        let body = "- [ ] Tom — write spec\n- [ ] no owner here\n";
        let got = parse_actions(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].owner_candidate.as_deref(), Some("Tom"));
        assert_eq!(got[0].text, "Tom — write spec");
        assert_eq!(got[1].owner_candidate, None);
        assert_eq!(got[1].text, "no owner here");
    }
}
