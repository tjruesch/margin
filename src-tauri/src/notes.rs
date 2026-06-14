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

fn new_note_ref(id: String) -> NoteRef {
    NoteRef {
        // After #112 the `note_path`-named field carries the note id,
        // not a filesystem path. Keeping the field name is a known
        // legacy debt; values are bundle-id-shaped throughout.
        note_path: id.clone(),
        id,
    }
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Create a new note row and return its id (#112). No disk write
/// happens at create time — the per-note bundle directory under
/// `~/.margin/notes/<id>/` is only created when audio recording
/// starts and needs a place for `audio.wav`.
#[tauri::command]
pub fn create_note(
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now_ms = current_unix_ms();
    // Default title is the creation timestamp, formatted locally
    // (e.g. "Tuesday, 12.05, 13:20"). The body seeds with the same
    // string as an H1 so the editor's deriveTitle path picks it up
    // — typing over the H1 renames the note via the normal save loop.
    let title = chrono::Local::now()
        .format("%A, %d.%m, %H:%M")
        .to_string();
    let body_md = format!("# {title}\n");
    let body_size = body_md.len() as i64;
    let preview = extract_preview(&body_md);
    let c = conn.lock().map_err(|e| e.to_string())?;
    c.execute(
        "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                           preview, body_size, created_ms) \
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?4)",
        rusqlite::params![id, title, body_md, now_ms, preview, body_size],
    )
    .map_err(|e| e.to_string())?;
    c.execute(
        "INSERT INTO notes_fts(note_id, title, body) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, title, body_md],
    )
    .map_err(|e| e.to_string())?;
    Ok(new_note_ref(id))
}

/// Clone a note into a new row (#112). Title and tags carry over;
/// `archived` and `favorite` flags are stripped (they're state, not
/// content). Audio/transcript sidecars are intentionally not copied.
#[tauri::command]
pub fn duplicate_note(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    // The `note_path` parameter name survives from the pre-#112 IPC
    // shape; the value flowing through is the source note's id.
    let src_id = note_path;
    let c = conn.lock().map_err(|e| e.to_string())?;
    let (title, body_md): (String, String) = c
        .query_row(
            "SELECT title, body_md FROM notes WHERE id = ?1",
            rusqlite::params![src_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| format!("source note not found: {e}"))?;

    let new_id = uuid::Uuid::new_v4().to_string();
    let now = current_unix_ms();
    let body_size = body_md.len() as i64;
    c.execute(
        "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                           preview, body_size, created_ms) \
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?4)",
        rusqlite::params![
            new_id,
            title,
            body_md,
            now,
            extract_preview(&body_md),
            body_size,
        ],
    )
    .map_err(|e| e.to_string())?;
    c.execute(
        "INSERT INTO notes_fts(note_id, title, body) VALUES (?1, ?2, ?3)",
        rusqlite::params![new_id, title, body_md],
    )
    .map_err(|e| e.to_string())?;
    // Tags carry over verbatim.
    c.execute(
        "INSERT INTO tags(note_id, tag) \
         SELECT ?1, tag FROM tags WHERE note_id = ?2",
        rusqlite::params![new_id, src_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(new_note_ref(new_id))
}

/// Per-note bundle directory under `~/.margin/notes/<id>/`. The
/// directory hosts audio/transcript sidecars; after #112 the
/// markdown body lives in the DB instead.
pub fn bundle_dir_for(note_id: &str) -> PathBuf {
    paths::notes_dir().join(note_id)
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

/// Resolve a note's bundle id to the absolute filesystem path of its
/// `transcript.json` sidecar. Returns `None` when the file isn't on
/// disk (no recording yet, transcript pending, etc.).
///
/// After #112 notes live in the DB by `note_id` (= bundle id) and the
/// frontend no longer holds filesystem paths. Audio + transcripts
/// still live on disk under `<notes_dir>/<note_id>/`, so any code
/// path that needs to read or reference the transcript by absolute
/// path (reconcile prompt, transcript viewer, "Generate notes"
/// affordance) routes through this helper.
#[tauri::command]
pub fn transcript_path_for(note_id: String) -> Option<String> {
    let path = crate::paths::notes_dir()
        .join(&note_id)
        .join(TRANSCRIPT_FILENAME);
    if path.exists() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Search across all non-archived owned notes (titles + bodies via the
/// FTS5 index, plus per-bundle transcript.json segments). Returns a
/// ranked list of `SearchHit`s. `limit` defaults to 20 and is clamped
/// to 50 by the index layer.
#[tauri::command]
pub fn search_notes(
    query: String,
    limit: Option<usize>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<crate::index::SearchHit>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    crate::index::search_notes(&c, &query, limit.unwrap_or(20))
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct NoteMeta {
    pub modified_ms: i64,
}

/// Read `modified_ms` for a single note (#112). DB-only.
#[tauri::command]
pub fn note_meta(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteMeta, String> {
    let note_id = note_path;
    let c = conn.lock().map_err(|e| e.to_string())?;
    let modified_ms: i64 = c
        .query_row(
            "SELECT modified_ms FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("note not found: {e}"))?;
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

/// Load a note's body + flags + tags from the DB (#112). Any leading
/// YAML frontmatter in `body_md` is split off — the editor sees only
/// the prose. The non-managed keys (everything except `tags`,
/// `archived`, `favorite`, which live in their own columns/tables)
/// land in `frontmatter_extras` so `write_note` can prepend them back
/// on save. This keeps calendar-event notes' `calendar_event_id` /
/// `meeting_start_ms` / `meeting_end_ms` / `location` metadata alive
/// across edits without showing it to the user as visible content.
#[tauri::command]
pub fn read_note(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteContent, String> {
    let note_id = note_path;
    let c = conn.lock().map_err(|e| e.to_string())?;
    let (raw, archived, favorite): (String, bool, bool) = c
        .query_row(
            "SELECT body_md, archived, favorite FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get::<_, i64>(2)? != 0,
                ))
            },
        )
        .map_err(|e| format!("note not found: {e}"))?;
    let (fm_yaml, body_after_fm) = split_frontmatter(&raw);
    let frontmatter_extras = match fm_yaml {
        Some(yaml) => {
            let mut map = parse_frontmatter(yaml);
            // `tags` / `archived` / `favorite` are owned by the DB
            // columns + tags table — don't echo them back to the
            // frontend as user-managed extras. Strip them so the
            // round-trip on write doesn't double-write managed flags.
            map.remove(serde_yml::Value::String("tags".into()));
            map.remove(serde_yml::Value::String("archived".into()));
            map.remove(serde_yml::Value::String("favorite".into()));
            map
        }
        None => Mapping::new(),
    };
    let mut stmt = c
        .prepare("SELECT tag FROM tags WHERE note_id = ?1 ORDER BY tag")
        .map_err(|e| e.to_string())?;
    let tags: Vec<String> = stmt
        .query_map(rusqlite::params![note_id], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    Ok(NoteContent {
        body: body_after_fm.to_string(),
        tags,
        archived,
        favorite,
        frontmatter_extras,
    })
}

/// Result envelope for `write_note`. `rewritten_body` is `Some` when
/// the Rust side rewrote relative due-date tokens (`@today`,
/// `@tomorrow`, `@<weekday>`) to their absolute `@YYYY-MM-DD` forms
/// — the frontend uses it to swap the editor's in-memory text so it
/// stays in sync with the persisted body.
#[derive(Serialize)]
pub struct WriteNoteResult {
    pub rewritten_body: Option<String>,
}

/// Persist a note body and refresh derived state in one transaction
/// (#112). Re-derives title from `body`, refreshes FTS, reparses
/// `- [?]` lines into the open-questions table, emits the
/// `note_modified` event — all atomically.
///
/// `tags` / `archived` / `favorite` are NOT touched here; those
/// surfaces have their own DB-only IPCs (`set_note_tags` /
/// `set_archived` / `set_favorite`). The pre-#112 `write_note` IPC
/// took all of them in one call; we kept the parameter list compatible
/// so existing frontend wiring doesn't have to branch.
#[tauri::command]
pub fn write_note(
    note_path: String,
    body: String,
    tags: Vec<String>,
    archived: bool,
    favorite: bool,
    frontmatter_extras: Mapping,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<WriteNoteResult, String> {
    let note_id = note_path;

    // Run due-token rewrites on the editor-visible body (no
    // frontmatter), then restore the frontmatter `read_note` split
    // off so non-managed metadata (calendar_event_id, etc.) survives
    // the round-trip. The returned `rewritten_body` echoes only the
    // editor-visible portion so the frontend can swap its buffer
    // without re-introducing the hidden YAML.
    let today = chrono::Local::now().date_naive();
    let (visible_body, rewritten_body) = match rewrite_relative_due_tokens(&body, today) {
        Some(new_body) => {
            let echo = new_body.clone();
            (new_body, Some(echo))
        }
        None => (body, None),
    };
    let final_body = write_with_frontmatter(&frontmatter_extras, &visible_body);

    let now = current_unix_ms();
    let normalized = normalize_tags(tags);
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    {
        let tx = c.transaction().map_err(|e| e.to_string())?;
        // Body + derived columns + FTS in one go.
        let parsed = crate::index::parse_indexable_from_body(&note_id, &final_body, now);
        crate::index::upsert_in_tx(&tx, &note_id, &parsed).map_err(|e| e.to_string())?;
        // Flag + tag side-effects share the same transaction so a
        // crash mid-write rolls back the whole save.
        tx.execute(
            "UPDATE notes SET archived = ?2, favorite = ?3 WHERE id = ?1",
            rusqlite::params![note_id, archived as i64, favorite as i64],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM tags WHERE note_id = ?1",
            rusqlite::params![note_id],
        )
        .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached("INSERT INTO tags(note_id, tag) VALUES (?1, ?2)")
                .map_err(|e| e.to_string())?;
            for tag in &normalized {
                stmt.execute(rusqlite::params![note_id, tag])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
    }
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

/// Replace the tag set for a note (#112). DB-only — the body is
/// not touched, so an in-flight editor buffer survives.
#[tauri::command]
pub fn set_note_tags(
    note_path: String,
    tags: Vec<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let note_id = note_path;
    let normalized = normalize_tags(tags);
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM tags WHERE note_id = ?1",
        rusqlite::params![note_id],
    )
    .map_err(|e| e.to_string())?;
    {
        let mut stmt = tx
            .prepare_cached("INSERT INTO tags(note_id, tag) VALUES (?1, ?2)")
            .map_err(|e| e.to_string())?;
        for tag in &normalized {
            stmt.execute(rusqlite::params![note_id, tag])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

/// Flip the archived flag on a note (#112). DB-only.
#[tauri::command]
pub fn set_archived(
    note_path: String,
    archived: bool,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let note_id = note_path;
    let c = conn.lock().map_err(|e| e.to_string())?;
    c.execute(
        "UPDATE notes SET archived = ?2 WHERE id = ?1",
        rusqlite::params![note_id, archived as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Flip the favorite flag on a note (#112). DB-only.
#[tauri::command]
pub fn set_favorite(
    note_path: String,
    favorite: bool,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let note_id = note_path;
    let c = conn.lock().map_err(|e| e.to_string())?;
    c.execute(
        "UPDATE notes SET favorite = ?2 WHERE id = ?1",
        rusqlite::params![note_id, favorite as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- Markdown checkbox parsing (due tokens) -----------------------

/// Resolve any trailing relative `@<token>` (today/tomorrow/weekday) on
/// checkbox lines to its absolute `@YYYY-MM-DD` form, against `today`.
/// Returns `Some(new_body)` if at least one substitution happened,
/// `None` if the body was already canonical. Code-fenced lines are
/// skipped via the same fence-aware iteration the body parser uses.
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

/// Remove `audio.wav` and `transcript.json` from the bundle (#112).
/// The note row's `body_md` is untouched; only the on-disk sidecars
/// get cleaned. `duration_ms` is also zeroed since the recording is
/// gone.
#[tauri::command]
pub fn discard_recording(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let note_id = note_path; // legacy field name; value is a note id
    let dir = bundle_dir_for(&note_id);
    for name in [AUDIO_FILENAME, TRANSCRIPT_FILENAME] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
    }
    let c = conn.lock().map_err(|e| e.to_string())?;
    c.execute(
        "UPDATE notes SET duration_ms = NULL WHERE id = ?1",
        rusqlite::params![note_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Delete a note row + its on-disk audio/transcript sidecars (#112).
/// FK ON DELETE CASCADE handles tags / meeting_attendees;
/// the FTS row is dropped explicitly inside the transaction.
#[tauri::command]
pub fn delete_note(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let note_id = note_path;
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    crate::index::remove(&mut c, &note_id).map_err(|e| e.to_string())?;
    // Clean up audio/transcript siblings best-effort. Empty bundle
    // directories left behind are harmless (re-recording will
    // recreate the dir).
    let dir = bundle_dir_for(&note_id);
    if dir.is_dir() {
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(())
}

/// One-time disk → DB body backfill (#112). Reads every legacy
/// `~/.margin/notes/<id>/note.md` and populates the corresponding
/// `notes.body_md` column, then renames the legacy notes folder to
/// `notes-archive-pre-v26/` and recreates an empty `notes_dir` for
/// future audio/transcript sidecars.
///
/// Idempotent: gated by the `notes_body_backfill_done` meta flag set
/// to `'0'` by migration 026 and flipped to `'1'` here on success.
pub fn body_backfill_if_pending(
    conn: &mut rusqlite::Connection,
    notes_dir: &Path,
) -> Result<(), String> {
    let done: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'notes_body_backfill_done'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "1".to_string());
    if done == "1" {
        return Ok(());
    }

    let ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT id FROM notes")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let now = current_unix_ms();
    for id in &ids {
        let file = notes_dir.join(id).join(NOTE_FILENAME);
        let raw = match fs::read_to_string(&file) {
            Ok(s) => s,
            Err(_) => continue, // missing file — leave body_md=''
        };
        let (yaml, body) = split_frontmatter(&raw);
        // Frontmatter `archived` / `favorite` / `tags` already mirror
        // into columns via the pre-#112 indexer; we read them here and
        // patch the columns to match disk state, then write the body.
        // Other YAML keys (`frontmatter_extras`) are intentionally
        // dropped — documented breaking change.
        let map = yaml.map(parse_frontmatter).unwrap_or_default();
        let archived = read_archived(&map);
        let favorite = read_favorite(&map);
        let tags = read_tags(&map);
        let modified_ms: i64 = tx
            .query_row(
                "SELECT modified_ms FROM notes WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap_or(now);
        let parsed = crate::index::parse_indexable_from_body(id, body, modified_ms);
        crate::index::upsert_in_tx(&tx, id, &parsed)
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE notes SET archived = ?2, favorite = ?3 WHERE id = ?1",
            rusqlite::params![id, archived as i64, favorite as i64],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM tags WHERE note_id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached("INSERT INTO tags(note_id, tag) VALUES (?1, ?2)")
                .map_err(|e| e.to_string())?;
            for tag in &tags {
                stmt.execute(rusqlite::params![id, tag])
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    // Move the legacy notes folder out of the way and recreate an
    // empty one for audio/transcript siblings.
    let archive = notes_dir.with_file_name("notes-archive-pre-v26");
    if notes_dir.exists() && !archive.exists() {
        if let Err(e) = fs::rename(notes_dir, &archive) {
            eprintln!("[notes] archive rename failed: {e}");
        }
    }
    fs::create_dir_all(notes_dir).ok();
    // Carry audio/transcript siblings over so playback still works.
    if let Ok(entries) = fs::read_dir(&archive) {
        for entry in entries.flatten() {
            let from = entry.path();
            if !from.is_dir() {
                continue;
            }
            let id = entry.file_name();
            let to = notes_dir.join(&id);
            fs::create_dir_all(&to).ok();
            for name in [AUDIO_FILENAME, TRANSCRIPT_FILENAME, TRANSCRIPT_PARTIAL_FILENAME] {
                let f = from.join(name);
                if f.exists() {
                    let _ = fs::rename(&f, to.join(name));
                }
            }
        }
    }

    conn.execute(
        "UPDATE meta SET value = '1' WHERE key = 'notes_body_backfill_done'",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Export every note in the DB to `dir_path/<bundle_id>/note.md`
/// using the legacy frontmatter format (#112). Round-trippable: the
/// resulting tree can be re-read by the migration's
/// `split_frontmatter` / `parse_frontmatter` helpers. Returns the
/// count of files written.
#[tauri::command]
pub fn export_notes(
    dir_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<usize, String> {
    let root = PathBuf::from(&dir_path);
    if !root.is_dir() {
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    }
    let c = conn.lock().map_err(|e| e.to_string())?;
    let mut stmt = c
        .prepare(
            "SELECT id, bundle_id, body_md, archived, favorite FROM notes",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, bool, bool)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, i64>(4)? != 0,
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    let mut written = 0usize;
    for (id, bundle_id, body_md, archived, favorite) in rows {
        let mut stmt = c
            .prepare("SELECT tag FROM tags WHERE note_id = ?1 ORDER BY tag")
            .map_err(|e| e.to_string())?;
        let tags: Vec<String> = stmt
            .query_map(rusqlite::params![id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        let mut map = Mapping::new();
        if !tags.is_empty() {
            let seq: Vec<serde_yml::Value> =
                tags.into_iter().map(serde_yml::Value::String).collect();
            map.insert(
                serde_yml::Value::String("tags".into()),
                serde_yml::Value::Sequence(seq),
            );
        }
        set_bool_key(&mut map, "archived", archived);
        set_bool_key(&mut map, "favorite", favorite);
        let merged = write_with_frontmatter(&map, &body_md);

        let bundle_dir = root.join(&bundle_id);
        fs::create_dir_all(&bundle_dir).map_err(|e| e.to_string())?;
        let target = bundle_dir.join(NOTE_FILENAME);
        fs::write(&target, merged).map_err(|e| e.to_string())?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn make_bundle(notes_dir: &Path, id: &str) -> PathBuf {
        let dir = notes_dir.join(id);
        fs::create_dir_all(&dir).unwrap();
        let note = dir.join(NOTE_FILENAME);
        fs::write(&note, "# hi\n").unwrap();
        note
    }

    /// Open a DB at the latest schema version and seed a single empty
    /// note row so the body backfill / export tests have a target.
    fn fresh_db_with_note(note_id: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                               preview, body_size, created_ms) \
             VALUES (?1, ?1, 'Untitled', '', 1000, '', 0, 1000)",
            rusqlite::params![note_id],
        )
        .unwrap();
        // Clear the backfill flag so the test can drive it.
        conn.execute(
            "UPDATE meta SET value = '0' WHERE key = 'notes_body_backfill_done'",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn body_backfill_reads_disk_and_flags_meta() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        let bundle = notes.join("abc");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            bundle.join(NOTE_FILENAME),
            "---\ntags: [work]\nfavorite: true\n---\n# Plan\n\n- [ ] task\n",
        )
        .unwrap();

        let mut conn = fresh_db_with_note("abc");
        body_backfill_if_pending(&mut conn, &notes).unwrap();

        let body: String = conn
            .query_row(
                "SELECT body_md FROM notes WHERE id = 'abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.contains("# Plan"));
        assert!(body.contains("- [ ] task"));
        // Frontmatter is stripped — body_md is just the markdown body.
        assert!(!body.contains("tags:"));

        let favorite: i64 = conn
            .query_row(
                "SELECT favorite FROM notes WHERE id = 'abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(favorite, 1);

        let tags: Vec<String> = conn
            .prepare("SELECT tag FROM tags WHERE note_id = 'abc'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tags, vec!["work".to_string()]);

        let flag: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'notes_body_backfill_done'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, "1");
    }

    #[test]
    fn body_backfill_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        let mut conn = fresh_db_with_note("xyz");
        // First run.
        body_backfill_if_pending(&mut conn, &notes).unwrap();
        // Mutate the row, then re-run. The flag is now '1', so the
        // second pass must be a no-op and leave the row alone.
        conn.execute(
            "UPDATE notes SET body_md = 'manual' WHERE id = 'xyz'",
            [],
        )
        .unwrap();
        body_backfill_if_pending(&mut conn, &notes).unwrap();
        let body: String = conn
            .query_row(
                "SELECT body_md FROM notes WHERE id = 'xyz'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body, "manual");
    }

    #[test]
    fn body_backfill_moves_audio_transcript_siblings() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        let bundle = notes.join("mtg");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join(NOTE_FILENAME), "# Meeting\n").unwrap();
        fs::write(bundle.join(AUDIO_FILENAME), b"wav-data").unwrap();
        fs::write(bundle.join(TRANSCRIPT_FILENAME), b"{}").unwrap();

        let mut conn = fresh_db_with_note("mtg");
        body_backfill_if_pending(&mut conn, &notes).unwrap();

        // Legacy folder renamed; audio/transcript moved into the new
        // empty notes_dir/<id>/ layout so the audio playback path
        // keeps working.
        let archive = notes.with_file_name("notes-archive-pre-v26");
        assert!(archive.exists(), "archive folder must exist");
        assert!(
            notes.join("mtg").join(AUDIO_FILENAME).exists(),
            "audio.wav must land under the new notes_dir/<id>/"
        );
        assert!(
            notes.join("mtg").join(TRANSCRIPT_FILENAME).exists(),
            "transcript.json must land under the new notes_dir/<id>/"
        );
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
    fn read_favorite_tolerates_string_yes() {
        let map: Mapping = serde_yml::from_str("favorite: \"yes\"").unwrap();
        assert!(read_favorite(&map));
    }

    /// Regression: pre-#155 `read_note` returned `body_md` raw with no
    /// frontmatter handling, so calendar-event notes leaked their
    /// `calendar_event_id` / `meeting_*` YAML into the editor view.
    /// This test asserts the split + parse + re-emit path the fixed
    /// `read_note`/`write_note` rely on round-trips cleanly for a
    /// connector-produced body.
    #[test]
    fn event_frontmatter_round_trips_via_extras() {
        let raw = "---\n\
                   calendar_event_id: \"mg::evt-1\"\n\
                   meeting_start_ms: 1779442200000\n\
                   meeting_end_ms: 1779444000000\n\
                   location: Microsoft Teams Meeting\n\
                   ---\n\n\
                   # memoq-bridge integration\n\n";

        // read_note side: split frontmatter off, parse extras minus
        // the managed keys.
        let (fm_yaml, body) = split_frontmatter(raw);
        let yaml = fm_yaml.expect("event note must have frontmatter");
        let mut extras = parse_frontmatter(yaml);
        for managed in ["tags", "archived", "favorite"] {
            extras.remove(serde_yml::Value::String(managed.into()));
        }
        assert!(
            !body.contains("calendar_event_id"),
            "editor-visible body must NOT include the frontmatter"
        );
        assert!(
            body.trim_start().starts_with("# memoq-bridge integration"),
            "body starts with the H1: got {body:?}"
        );
        assert_eq!(extras.len(), 4, "all four event keys land in extras");

        // write_note side: extras + edited body merge back into a
        // body_md that preserves the YAML keys.
        let edited = format!("{body}Some notes the user typed.\n");
        let merged = write_with_frontmatter(&extras, &edited);
        assert!(merged.starts_with("---\n"));
        assert!(merged.contains("calendar_event_id"));
        assert!(merged.contains("Some notes the user typed."));

        // And the next read_note pass yields the same extras + edited
        // body — idempotent round-trip.
        let (fm2, body2) = split_frontmatter(&merged);
        assert!(fm2.is_some());
        assert_eq!(body2, edited);
    }

    /// Round-trip with an empty extras map must NOT prepend a
    /// frontmatter block (regression against double-blank-lines or
    /// stray `---` markers on plain notes).
    #[test]
    fn empty_extras_writes_body_unchanged() {
        let extras = Mapping::new();
        let body = "# Plain note\n\nNo frontmatter here.\n";
        let merged = write_with_frontmatter(&extras, body);
        assert_eq!(merged, body);
    }

    // The pre-#112 `delete_note_in` path-validation tests are gone:
    // delete_note now operates on a `note_id` against the DB. The
    // FK CASCADE in migration 026 handles the dependent-rows cleanup
    // and there's no filesystem path to validate anymore.

}
