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

use rusqlite::OptionalExtension;
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

#[derive(Debug, Clone, Serialize)]
pub struct ActionListItem {
    pub id: String,
    /// Origin discriminator (#111): `"note"` for markdown-checkbox-backed
    /// rows, `"synth"` for synthesizer-emitted rows. Drives row click-
    /// through and the per-origin write dispatch inside the unified
    /// IPCs.
    pub origin_kind: String,
    /// Source note path for note-origin rows; `None` for synth rows
    /// (no underlying file).
    pub origin_note_path: Option<String>,
    /// 1-based source-line for note-origin rows; `None` for synth rows.
    pub origin_line: Option<i64>,
    /// Note title when `origin_note_path` resolves, `None` otherwise.
    pub note_title: Option<String>,
    /// Synth source kind (`"email" | "event" | "note"`) when the
    /// synthesizer paraphrased this row (#111). `None` for note-origin
    /// rows. Powers the workstream detail's per-row "open source"
    /// affordance and the AI ask prompt's "from {kind}" label.
    pub origin_synth_kind: Option<String>,
    /// Connector-qualified id of the synth source row. `None` for
    /// note-origin rows.
    pub origin_synth_id: Option<String>,
    /// Direct workstream attachment id. Set by the synthesizer on a
    /// `'synth'` row or by the user via `set_action_workstream` on any
    /// origin (#111).
    pub workstream_id: Option<String>,
    /// Workstream title joined from `workstream_id` for render.
    pub workstream_title: Option<String>,
    pub text: String,
    pub done: bool,
    pub created_ms: i64,
    /// Absolute due-date timestamp (Unix ms). For note-origin rows,
    /// parsed from a trailing `@YYYY-MM-DD[ HH:MM]` token; for synth
    /// rows, set by the synthesizer.
    pub due_ms: Option<i64>,
    /// `team_members.id` when the action has a resolved owner. Note-
    /// origin: matched the leading `Owner — ` segment (#49). Synth:
    /// stamped by the synthesizer or manually set via
    /// `set_action_assignee` (#111).
    pub assignee_id: Option<String>,
    /// Canonical display name from `team_members`, joined for render so
    /// the frontend can surface an avatar chip without a second
    /// round-trip (#50/#51).
    pub assignee_display_name: Option<String>,
    /// For waiting-extracted synth rows, points at the *other* person
    /// in the conversation (counterparty of the assignee). NULL for
    /// note-origin rows and any synth row with no single counterparty.
    pub subject_member_id: Option<String>,
    /// 1 once the user has touched this synth row; the profile worker
    /// stops auto-modifying it after that. Avoids the user-unchecks /
    /// worker-rechecks loop.
    pub manual_override: bool,
    /// Stamped by the worker (NOT the user) when auto-resolve flipped
    /// `done` after the hysteresis threshold (#124). Drives the
    /// "Margin auto-resolved" pill on the action row; clicking the
    /// pill calls `undo_auto_resolved_action` to reopen + lock.
    pub auto_resolved_ms: Option<i64>,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ActionScope {
    #[default]
    Open,
    Done,
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

/// Reserved id for the catch-all "Inbox" note that holds quick
/// todos created without a source note. Stable across sessions so the
/// frontend can find-or-create with a single call.
pub const INBOX_BUNDLE_ID: &str = "inbox";

/// Create a new note row and return its id (#112). No disk write
/// happens at create time — the per-note bundle directory under
/// `~/.margin/notes/<id>/` is only created when audio recording
/// starts and needs a place for `audio.wav`.
#[tauri::command]
pub fn create_note(
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = current_unix_ms();
    let c = conn.lock().map_err(|e| e.to_string())?;
    c.execute(
        "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                           preview, body_size, created_ms) \
         VALUES (?1, ?1, 'Untitled note', '', ?2, '', 0, ?2)",
        rusqlite::params![id, now],
    )
    .map_err(|e| e.to_string())?;
    c.execute(
        "INSERT INTO notes_fts(note_id, title, body) VALUES (?1, 'Untitled note', '')",
        rusqlite::params![id],
    )
    .map_err(|e| e.to_string())?;
    Ok(new_note_ref(id))
}

/// Find-or-create the Inbox note and return its NoteRef. Quick todos
/// from the Action items page get appended to this note's body via the
/// normal `write_note` round-trip (#112).
#[tauri::command]
pub fn ensure_inbox_note(
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteRef, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let exists: bool = c
        .query_row(
            "SELECT 1 FROM notes WHERE id = ?1",
            rusqlite::params![INBOX_BUNDLE_ID],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .is_some();
    if !exists {
        let now = current_unix_ms();
        c.execute(
            "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                               preview, body_size, created_ms) \
             VALUES (?1, ?1, 'Inbox', '# Inbox\n', ?2, '', 8, ?2)",
            rusqlite::params![INBOX_BUNDLE_ID, now],
        )
        .map_err(|e| e.to_string())?;
        c.execute(
            "INSERT INTO notes_fts(note_id, title, body) \
             VALUES (?1, 'Inbox', '# Inbox\n')",
            rusqlite::params![INBOX_BUNDLE_ID],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(new_note_ref(INBOX_BUNDLE_ID.to_string()))
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

/// Return action items from the unified `actions` table (#111), scoped
/// by done-state, optional assignee, and optional workstream
/// attachment. Default scope is `Open`. Joins surface the source note's
/// or workstream's title for display without a second round-trip.
#[tauri::command]
pub fn list_actions(
    scope: Option<ActionScope>,
    assignee_id: Option<String>,
    workstream_id: Option<String>,
    subject_member_id: Option<String>,
    origin_synth_kinds: Option<Vec<String>>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<ActionListItem>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let kinds_json = origin_synth_kinds
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()));
    crate::index::list_actions(
        &c,
        scope.unwrap_or_default(),
        assignee_id.as_deref(),
        workstream_id.as_deref(),
        subject_member_id.as_deref(),
        kinds_json.as_deref(),
    )
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

/// Load a note's body + flags + tags from the DB (#112). The
/// `frontmatter_extras` field survives in the type for compatibility
/// with the pre-#112 editor; values are always an empty mapping after
/// the migration (free-form YAML keys aren't preserved post-#112).
#[tauri::command]
pub fn read_note(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<NoteContent, String> {
    let note_id = note_path;
    let c = conn.lock().map_err(|e| e.to_string())?;
    let (body, archived, favorite): (String, bool, bool) = c
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
    let mut stmt = c
        .prepare("SELECT tag FROM tags WHERE note_id = ?1 ORDER BY tag")
        .map_err(|e| e.to_string())?;
    let tags: Vec<String> = stmt
        .query_map(rusqlite::params![note_id], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    Ok(NoteContent {
        body,
        tags,
        archived,
        favorite,
        frontmatter_extras: Mapping::new(),
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
/// `- [ ]` lines into the unified `actions` table, emits
/// `note_modified` and `action_*` events — all atomically.
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
    let _ = frontmatter_extras; // dropped after #112; see read_note
    let note_id = note_path;

    let today = chrono::Local::now().date_naive();
    let (final_body, rewritten_body) = match rewrite_relative_due_tokens(&body, today) {
        Some(new_body) => {
            let echo = new_body.clone();
            (new_body, Some(echo))
        }
        None => (body, None),
    };

    let now = current_unix_ms();
    let normalized = normalize_tags(tags);
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    {
        let tx = c.transaction().map_err(|e| e.to_string())?;
        // Body + derived columns + FTS + actions in one go.
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

/// Read just the dispatch fields for an action: origin_kind +
/// origin_note_id + origin_line + text + assignee_id. For synth-
/// origin rows the note locator fields are NULL.
struct ActionDispatch {
    origin_kind: String,
    origin_note_id: Option<String>,
    origin_line: Option<usize>,
    text: String,
    assignee_id: Option<String>,
}

fn load_action_dispatch(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<ActionDispatch, String> {
    conn.query_row(
        "SELECT origin_kind, origin_note_id, origin_line, text, assignee_id \
           FROM actions WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            Ok(ActionDispatch {
                origin_kind: r.get::<_, String>(0)?,
                origin_note_id: r.get::<_, Option<String>>(1)?,
                origin_line: r
                    .get::<_, Option<i64>>(2)?
                    .map(|n| n as usize),
                text: r.get::<_, String>(3)?,
                assignee_id: r.get::<_, Option<String>>(4)?,
            })
        },
    )
    .map_err(|e| e.to_string())
}

/// Toggle the done state of an action item by its derived id (#111).
///
/// Dispatches on `origin_kind`:
///   - `'note'`: round-trip through the source markdown — find the line
///     (cached index first, then text-hash scan), flip the
///     `[ ]`/`[x]` marker, write the file back. `touch_index` queues
///     a reindex which republishes the row via `upsert_in_tx`.
///   - everything else (`'synth'`, future `'inbox'`): pure DB write
///     against the `actions` table; emits `action_completed` on a 0→1
///     transition.
/// Locate the action's line in `body_md`. Tries the cached line
/// index first, then falls back to a full hash scan. Returns the
/// 0-based line index when found.
fn locate_action_line(
    body_md: &str,
    cached_line: usize,
    want_text: &str,
) -> Option<usize> {
    let lines: Vec<&str> = body_md.split('\n').collect();
    let want_hash = action_text_hash(want_text);
    if cached_line >= 1 && cached_line <= lines.len() {
        if let Some((line_text, _, _)) =
            parse_action_line(lines[cached_line - 1].trim_start())
        {
            if action_text_hash(&line_text) == want_hash {
                return Some(cached_line - 1);
            }
        }
    }
    for (i, line) in lines.iter().enumerate() {
        if let Some((line_text, _, _)) = parse_action_line(line.trim_start()) {
            if action_text_hash(&line_text) == want_hash {
                return Some(i);
            }
        }
    }
    None
}

/// Mutate the body of a note-origin action by `mutate` and write the
/// new body back atomically through `upsert_in_tx`. Common scaffold
/// for `set_action_done` / `set_action_assignee` / `delete_action`.
///
/// `mutate` returns `Ok(Some(new_line))` to replace the line,
/// `Ok(None)` to delete it, or `Err` to abort.
fn mutate_note_action_body<F>(
    note_id: &str,
    cached_line: usize,
    want_text: &str,
    conn: &tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
    mutate: F,
) -> Result<(), String>
where
    F: FnOnce(&str) -> Result<Option<String>, String>,
{
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let body: String = c
        .query_row(
            "SELECT body_md FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| format!("note not found: {e}"))?;
    let idx = locate_action_line(&body, cached_line, want_text).ok_or_else(|| {
        "Action not found in note (index may be stale; reload to refresh)".to_string()
    })?;
    let mut lines: Vec<String> = body.split('\n').map(|s| s.to_string()).collect();
    match mutate(&lines[idx])? {
        Some(new_line) => {
            if new_line == lines[idx] {
                return Ok(());
            }
            lines[idx] = new_line;
        }
        None => {
            lines.remove(idx);
        }
    }
    let new_body = lines.join("\n");
    let now = current_unix_ms();
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let parsed = crate::index::parse_indexable_from_body(note_id, &new_body, now);
    crate::index::upsert_in_tx(&tx, note_id, &parsed).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn set_action_done(
    id: String,
    done: bool,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let dispatch = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        load_action_dispatch(&c, &id)?
    };

    if dispatch.origin_kind != "note" {
        let c = conn.lock().map_err(|e| e.to_string())?;
        return crate::workstreams::persist::set_action_done(&c, &id, done)
            .map_err(|e| e.to_string());
    }

    let note_id = dispatch.origin_note_id.ok_or_else(|| {
        "note-origin action has no origin_note_id (corrupt row)".to_string()
    })?;
    let cached_line = dispatch.origin_line.unwrap_or(0);
    mutate_note_action_body(
        &note_id,
        cached_line,
        &dispatch.text,
        &conn,
        |line| Ok(Some(toggle_checkbox_marker(line, done))),
    )
}

/// Undo a worker auto-resolution (#124). Reopens the row, locks it
/// with `manual_override = 1` so the worker can't re-resolve, and
/// clears the hysteresis bookkeeping. Guarded by
/// `auto_resolved_ms IS NOT NULL` so misfires from the frontend
/// can't accidentally reopen a user-completed action.
#[tauri::command]
pub fn undo_auto_resolved_action(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    crate::workstreams::persist::undo_auto_resolved_action(&c, &id)
        .map_err(|e| e.to_string())
}

/// Reassign an action item to a different team member, or unassign
/// (#51, #111). Note-origin rows write through the markdown — the
/// leading `Owner — ` prefix is replaced/prepended/stripped via
/// `rewrite_action_owner`; `assignee_id` is re-resolved on the next
/// reindex pass. Synth-origin rows write the column directly. No-op
/// when the new assignee already matches the current one.
#[tauri::command]
pub fn set_action_assignee(
    action_id: String,
    member_id: Option<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let dispatch = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        load_action_dispatch(&c, &action_id)?
    };

    if member_id == dispatch.assignee_id {
        return Ok(());
    }

    if dispatch.origin_kind != "note" {
        let c = conn.lock().map_err(|e| e.to_string())?;
        return crate::workstreams::persist::set_action_assignee(
            &c,
            &action_id,
            member_id.as_deref(),
        )
        .map_err(|e| e.to_string());
    }

    let note_id = dispatch.origin_note_id.ok_or_else(|| {
        "note-origin action has no origin_note_id (corrupt row)".to_string()
    })?;
    let cached_line = dispatch.origin_line.unwrap_or(0);

    // Resolve the new member's canonical display name (if assigning).
    // None member_id = unassign.
    let new_owner_name: Option<String> = if let Some(id) = member_id.as_deref() {
        let c = conn.lock().map_err(|e| e.to_string())?;
        let members =
            crate::team::list_team_members_raw(&c).map_err(|e| e)?;
        match members.into_iter().find(|m| m.id == id) {
            Some(m) => Some(m.display_name),
            None => return Err(format!("team member not found: {id}")),
        }
    } else {
        None
    };

    mutate_note_action_body(&note_id, cached_line, &dispatch.text, &conn, |line| {
        rewrite_action_owner(line, new_owner_name.as_deref())
            .map(Some)
            .ok_or_else(|| "action line is not a recognizable checkbox".to_string())
    })
}

/// "Ignore" a worker-extracted waiting action: record the source in
/// `dismissed_action_sources` so the profile worker doesn't recreate
/// it, then delete the action row. Idempotent — re-dismissing the
/// same source is a no-op.
#[tauri::command]
pub fn dismiss_waiting_action(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let row: Option<(Option<String>, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT origin_synth_kind, origin_synth_id, assignee_id \
               FROM actions WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let now = crate::events::current_unix_ms();
    if let Some((Some(kind), Some(ref_id), assignee_id)) = row {
        tx.execute(
            "INSERT OR IGNORE INTO dismissed_action_sources \
                (origin_synth_kind, origin_synth_id, assignee_id, dismissed_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![kind, ref_id, assignee_id, now],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.execute("DELETE FROM actions WHERE id = ?1", rusqlite::params![id])
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Delete an action item (#111). Note-origin rows drop the literal
/// `- [ ]` line from `body_md` and let `upsert_in_tx` cull the row on
/// the next reparse. Synth-origin rows are deleted directly from the
/// `actions` table.
#[tauri::command]
pub fn delete_action(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let dispatch = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        load_action_dispatch(&c, &id)?
    };

    if dispatch.origin_kind != "note" {
        let c = conn.lock().map_err(|e| e.to_string())?;
        return crate::workstreams::persist::delete_action(&c, &id)
            .map_err(|e| e.to_string());
    }

    let note_id = dispatch.origin_note_id.ok_or_else(|| {
        "note-origin action has no origin_note_id (corrupt row)".to_string()
    })?;
    let cached_line = dispatch.origin_line.unwrap_or(0);

    mutate_note_action_body(&note_id, cached_line, &dispatch.text, &conn, |_| Ok(None))
}

/// Attach an action to a workstream (or clear the attachment when
/// `workstream_id` is `None`) (#111). Works for any `origin_kind` — a
/// note-origin row keeps its markdown line untouched; only the
/// `actions.workstream_id` column changes. The next synthesizer pass
/// treats a non-null attachment as a fixed pin and does not re-cluster
/// it.
#[tauri::command]
pub fn set_action_workstream(
    action_id: String,
    workstream_id: Option<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    let changed = c
        .execute(
            "UPDATE actions SET workstream_id = ?2 WHERE id = ?1",
            rusqlite::params![action_id, workstream_id],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("action not found: {action_id}"));
    }
    Ok(())
}

// ===== Open questions (#113) =====================================

#[derive(Debug, Clone, Serialize)]
pub struct OpenQuestionItem {
    pub id: String,
    /// Source note id. Field name preserved from the action surface
    /// for legacy compatibility.
    pub origin_note_path: String,
    pub origin_line: i64,
    pub note_title: Option<String>,
    pub workstream_id: Option<String>,
    pub workstream_title: Option<String>,
    pub text: String,
    pub resolved: bool,
    pub resolved_ms: Option<i64>,
    pub resolved_note: Option<String>,
    pub asked_of_id: Option<String>,
    pub asked_of_display_name: Option<String>,
    pub created_ms: i64,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum QuestionScope {
    #[default]
    Open,
    Resolved,
    All,
}

/// Plain connection-taking variant for callers outside the Tauri
/// command layer (e.g. `workstreams::persist::get_workstream_detail`).
pub fn list_open_questions_for(
    conn: &rusqlite::Connection,
    scope: QuestionScope,
    asked_of_id: Option<&str>,
    workstream_id: Option<&str>,
) -> rusqlite::Result<Vec<OpenQuestionItem>> {
    let resolved_filter: Option<i64> = match scope {
        QuestionScope::Open => Some(0),
        QuestionScope::Resolved => Some(1),
        QuestionScope::All => None,
    };
    let mut stmt = conn.prepare(
        "SELECT q.id, q.origin_note_id, q.origin_line, q.text, q.resolved, \
                q.resolved_ms, q.resolved_note, q.asked_of_id, q.created_ms, \
                n.title AS note_title, \
                w.id    AS workstream_id, \
                w.title AS workstream_title, \
                t.display_name AS asked_of_display_name, \
                n.modified_ms AS order_ms \
           FROM note_open_questions q \
           LEFT JOIN notes              n  ON n.id = q.origin_note_id \
           LEFT JOIN workstream_signals ws ON ws.kind = 'note' \
                                          AND ws.item_id = q.origin_note_id \
                                          AND ws.manual_detached_ms IS NULL \
           LEFT JOIN workstreams        w  ON w.id   = ws.workstream_id \
                                          AND w.status = 'active' \
           LEFT JOIN team_members       t  ON t.id   = q.asked_of_id \
          WHERE (n.id IS NULL OR n.archived = 0) \
            AND (?1 IS NULL OR q.resolved = ?1) \
            AND (?2 IS NULL OR q.asked_of_id = ?2) \
            AND (?3 IS NULL OR w.id = ?3) \
          ORDER BY q.resolved ASC, order_ms DESC, q.origin_line ASC",
    )?;
    let mut rows = stmt.query_map(
        rusqlite::params![resolved_filter, asked_of_id, workstream_id],
        |r| {
            Ok(OpenQuestionItem {
                id: r.get(0)?,
                origin_note_path: r.get(1)?,
                origin_line: r.get(2)?,
                text: r.get(3)?,
                resolved: r.get::<_, i64>(4)? != 0,
                resolved_ms: r.get(5)?,
                resolved_note: r.get(6)?,
                asked_of_id: r.get(7)?,
                created_ms: r.get(8)?,
                note_title: r.get(9)?,
                workstream_id: r.get(10)?,
                workstream_title: r.get(11)?,
                asked_of_display_name: r.get(12)?,
            })
        },
    )?;
    // De-dupe by question id: a note pinned to N workstreams produces
    // N JOIN rows. Keep the first per id (the JOIN's ORDER BY puts
    // the most-recently-active workstream first).
    let mut seen = std::collections::HashSet::<String>::new();
    let mut out: Vec<OpenQuestionItem> = Vec::new();
    while let Some(row) = rows.next() {
        let item = row?;
        if seen.insert(item.id.clone()) {
            out.push(item);
        }
    }
    Ok(out)
}

/// Return open-question rows joined to their parent note and (when
/// the note is attached to a workstream via `workstream_signals`)
/// the workstream's title.
#[tauri::command]
pub fn list_open_questions(
    scope: Option<QuestionScope>,
    asked_of_id: Option<String>,
    workstream_id: Option<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<OpenQuestionItem>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    list_open_questions_for(
        &c,
        scope.unwrap_or_default(),
        asked_of_id.as_deref(),
        workstream_id.as_deref(),
    )
    .map_err(|e| e.to_string())
}

/// Locate a `- [?]` or `- [x]` line matching `want_text` (#113). The
/// hash compares against the line's parsed question text with any
/// trailing ` → answer: …` segment stripped, so we can find the line
/// before AND after resolution.
fn locate_question_line(
    body_md: &str,
    cached_line: usize,
    want_text: &str,
) -> Option<usize> {
    let lines: Vec<&str> = body_md.split('\n').collect();
    let want_hash = action_text_hash(want_text);
    let try_line = |line: &str| -> Option<String> {
        let trimmed = line.trim_start();
        let after_bullet = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))?;
        let bytes = after_bullet.as_bytes();
        if bytes.len() < 4 || bytes[0] != b'[' || bytes[2] != b']' || bytes[3] != b' ' {
            return None;
        }
        if !matches!(bytes[1], b'?' | b'x' | b'X') {
            return None;
        }
        let raw_text = after_bullet[4..].trim();
        if raw_text.is_empty() {
            return None;
        }
        Some(strip_trailing_answer_segment(raw_text).to_string())
    };
    if cached_line >= 1 && cached_line <= lines.len() {
        if let Some(line_text) = try_line(lines[cached_line - 1]) {
            if action_text_hash(&line_text) == want_hash {
                return Some(cached_line - 1);
            }
        }
    }
    for (i, line) in lines.iter().enumerate() {
        if let Some(line_text) = try_line(line) {
            if action_text_hash(&line_text) == want_hash {
                return Some(i);
            }
        }
    }
    None
}

/// Generic body-mutate scaffold for question IPCs (#113). Mirrors
/// `mutate_note_action_body`. `mutate` returns `Some(new_line)` to
/// replace the line, `None` to delete it, `Err` to abort.
fn mutate_note_question_body<F>(
    note_id: &str,
    cached_line: usize,
    want_text: &str,
    conn: &tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
    mutate: F,
) -> Result<(), String>
where
    F: FnOnce(&str) -> Result<Option<String>, String>,
{
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let body: String = c
        .query_row(
            "SELECT body_md FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| format!("note not found: {e}"))?;
    let idx = locate_question_line(&body, cached_line, want_text).ok_or_else(|| {
        "Question not found in note (index may be stale; reload to refresh)".to_string()
    })?;
    let mut lines: Vec<String> = body.split('\n').map(|s| s.to_string()).collect();
    match mutate(&lines[idx])? {
        Some(new_line) => {
            if new_line == lines[idx] {
                return Ok(());
            }
            lines[idx] = new_line;
        }
        None => {
            lines.remove(idx);
        }
    }
    let new_body = lines.join("\n");
    let now = current_unix_ms();
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let parsed = crate::index::parse_indexable_from_body(note_id, &new_body, now);
    crate::index::upsert_in_tx(&tx, note_id, &parsed).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Resolve a question (#113). Marks the row `resolved=1` first so the
/// subsequent body-mutate + `upsert_in_tx` pass doesn't blow it away
/// (the wholesale-replace inside `upsert_in_tx` is scoped to
/// `resolved = 0`). Body line flips `[?]` → `[x]`; if `answer` is
/// supplied, it's appended as ` → answer: <text>`.
#[tauri::command]
pub fn resolve_open_question(
    id: String,
    answer: Option<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let (note_id, origin_line, text, was_resolved): (String, usize, String, bool) = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.query_row(
            "SELECT origin_note_id, origin_line, text, resolved \
               FROM note_open_questions WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
            },
        )
        .map_err(|e| format!("question not found: {e}"))?
    };
    if was_resolved {
        return Ok(());
    }
    let answer_clean = answer.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let now = current_unix_ms();
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.execute(
            "UPDATE note_open_questions \
                SET resolved = 1, resolved_ms = ?2, resolved_note = ?3 \
              WHERE id = ?1",
            rusqlite::params![id, now, answer_clean],
        )
        .map_err(|e| e.to_string())?;
    }
    let answer_for_body = answer_clean.clone();
    mutate_note_question_body(&note_id, origin_line, &text, &conn, |line| {
        let flipped = flip_question_marker_to_resolved(line);
        let with_answer = match &answer_for_body {
            Some(a) => format!("{flipped} \u{2192} answer: {a}"),
            None => flipped,
        };
        Ok(Some(with_answer))
    })
}

/// Reopen a resolved question (#113). Body line flips `[x]` → `[?]`
/// and any trailing ` → answer: …` segment is dropped; the
/// `upsert_in_tx` ON CONFLICT branch picks up the now-`[?]` line and
/// resets `resolved` / `resolved_ms` / `resolved_note` on the row.
#[tauri::command]
pub fn reopen_open_question(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let (note_id, origin_line, text, was_resolved): (String, usize, String, bool) = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.query_row(
            "SELECT origin_note_id, origin_line, text, resolved \
               FROM note_open_questions WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
            },
        )
        .map_err(|e| format!("question not found: {e}"))?
    };
    if !was_resolved {
        return Ok(());
    }
    mutate_note_question_body(&note_id, origin_line, &text, &conn, |line| {
        let dropped = drop_trailing_answer(line);
        let flipped = flip_resolved_marker_to_question(&dropped);
        Ok(Some(flipped))
    })
}

/// Reassign a question to a different team member, or unassign with
/// `None` (#113). Rewrites the leading `Asked-of — ` prefix on the
/// markdown line — same machinery as `set_action_assignee`.
#[tauri::command]
pub fn set_open_question_asked_of(
    id: String,
    member_id: Option<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let (note_id, origin_line, text, current_asked_of): (
        String,
        usize,
        String,
        Option<String>,
    ) = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.query_row(
            "SELECT origin_note_id, origin_line, text, asked_of_id \
               FROM note_open_questions WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|e| format!("question not found: {e}"))?
    };
    if member_id == current_asked_of {
        return Ok(());
    }
    let new_owner_name: Option<String> = if let Some(m) = member_id.as_deref() {
        let c = conn.lock().map_err(|e| e.to_string())?;
        let members =
            crate::team::list_team_members_raw(&c).map_err(|e| e)?;
        match members.into_iter().find(|tm| tm.id == m) {
            Some(tm) => Some(tm.display_name),
            None => return Err(format!("team member not found: {m}")),
        }
    } else {
        None
    };
    mutate_note_question_body(&note_id, origin_line, &text, &conn, |line| {
        rewrite_action_owner(line, new_owner_name.as_deref())
            .map(Some)
            .ok_or_else(|| "question line is not a recognizable checkbox".to_string())
    })
}

/// Delete a question (#113). Drops the `- [?]`/`- [x]` line from the
/// body; the reindex either skips it (the line is gone) or naturally
/// re-emits any other unresolved questions. The row is removed
/// directly first so a stale `cached_line` failure doesn't leave the
/// row around.
#[tauri::command]
pub fn delete_open_question(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let (note_id, origin_line, text): (String, usize, String) = {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.query_row(
            "SELECT origin_note_id, origin_line, text \
               FROM note_open_questions WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|e| format!("question not found: {e}"))?
    };
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        c.execute(
            "DELETE FROM note_open_questions WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| e.to_string())?;
    }
    // Best-effort: remove the line from the body. If the line is
    // already gone (user edited it away), the locate-step error is
    // swallowed so the row deletion still stands.
    let _ = mutate_note_question_body(&note_id, origin_line, &text, &conn, |_| Ok(None));
    Ok(())
}

/// Flip `[?]` → `[x]` on a question line, preserving indent,
/// bullet, and trailing text.
fn flip_question_marker_to_resolved(line: &str) -> String {
    flip_marker_at(line, b'?', b'x')
}

/// Flip `[x]`/`[X]` → `[?]` on a question line.
fn flip_resolved_marker_to_question(line: &str) -> String {
    flip_marker_at(line, b'x', b'?')
}

fn flip_marker_at(line: &str, _from: u8, to: u8) -> String {
    if let Some(open) = line.find('[') {
        let close = open + 2;
        if line.as_bytes().get(close) == Some(&b']') {
            let mut out = String::with_capacity(line.len());
            out.push_str(&line[..open + 1]);
            out.push(to as char);
            out.push_str(&line[open + 2..]);
            return out;
        }
    }
    line.to_string()
}

/// Drop a trailing ` → answer: …` segment from a line, preserving
/// everything before it.
fn drop_trailing_answer(line: &str) -> String {
    for marker in [" \u{2192} answer:", " -> answer:"] {
        if let Some(idx) = line.find(marker) {
            return line[..idx].trim_end().to_string();
        }
    }
    line.to_string()
}

// ===== end open questions =========================================

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

/// One open-question line extracted from a note body (#113). Same shape
/// as `ParsedAction` minus the done/due fields — questions don't carry
/// per-line state in the source markdown beyond `[?]` / `[x]`.
#[derive(Clone, Debug)]
pub(crate) struct ParsedQuestion {
    pub line: usize,
    pub text: String,
    /// Leading `Asked-of — ` segment, same convention as action owners.
    pub owner_candidate: Option<String>,
}

/// Walk a note body and return every markdown task line as a
/// ParsedAction. Lines inside fenced code blocks are skipped (mirrors
/// the heuristic used by `extract_preview` for prose extraction).
pub(crate) fn parse_actions(body: &str) -> Vec<ParsedAction> {
    let mut out = Vec::new();
    walk_body_lines(body, |line_no, trimmed| {
        if let Some((text, done, due_ms)) = parse_action_line(trimmed) {
            let owner_candidate = extract_owner_candidate(&text);
            out.push(ParsedAction {
                line: line_no,
                text,
                done,
                due_ms,
                owner_candidate,
            });
        }
    });
    out
}

/// Walk a note body and return every `- [?]` line as a
/// `ParsedQuestion` (#113). Reuses `walk_body_lines` for the
/// code-fence-aware iteration so the two parsers stay in sync.
pub(crate) fn parse_open_questions(body: &str) -> Vec<ParsedQuestion> {
    let mut out = Vec::new();
    walk_body_lines(body, |line_no, trimmed| {
        if let Some(text) = parse_question_line(trimmed) {
            let owner_candidate = extract_owner_candidate(&text);
            out.push(ParsedQuestion {
                line: line_no,
                text,
                owner_candidate,
            });
        }
    });
    out
}

/// Iterate non-fenced body lines, yielding `(1-based line, trimmed)`.
fn walk_body_lines<F: FnMut(usize, &str)>(body: &str, mut yield_line: F) {
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
        yield_line(i + 1, trimmed);
    }
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

/// Return `text` with the leading `{Owner} {sep} ` segment removed, or
/// `text` unchanged when no recognized separator with a non-empty owner
/// prefix is present (#51). Counterpart to `extract_owner_candidate`.
pub(crate) fn strip_leading_owner_segment(text: &str) -> &str {
    const SEPARATORS: &[&str] = &[" — ", " – ", " -- "];
    // Pick the earliest separator that has a non-empty owner before it.
    let mut best: Option<(usize, usize)> = None; // (start_of_sep, sep_len)
    for sep in SEPARATORS {
        if let Some(idx) = text.find(sep) {
            if text[..idx].trim().is_empty() {
                continue;
            }
            match best {
                Some((cur, _)) if cur <= idx => {}
                _ => best = Some((idx, sep.len())),
            }
        }
    }
    match best {
        Some((idx, sep_len)) => &text[idx + sep_len..],
        None => text,
    }
}

/// Replace, prepend, or strip the leading `{Owner} — ` segment of an
/// action line (#51). Preserves the indentation, bullet character,
/// checkbox marker, and the rest of the line text (including any
/// trailing `@<token>`). Returns `None` when `line` is not a recognized
/// markdown checkbox.
///
/// - `Some(name)` → produce `{name} — {body without prior owner prefix}`.
/// - `None`       → strip any prior owner prefix.
///
/// Always emits the canonical em-dash separator on output regardless of
/// what was on input. Returns `Some(line.to_string())` unchanged when
/// the rewrite would be a no-op.
pub(crate) fn rewrite_action_owner(line: &str, new_owner: Option<&str>) -> Option<String> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let trimmed = &line[indent_len..];

    // Strip the bullet (one of `- `, `* `, `+ `) so we know the bullet
    // character; reattach with a single space separator below.
    let (bullet_char, after_bullet) = if let Some(rest) = trimmed.strip_prefix("- ") {
        ('-', rest)
    } else if let Some(rest) = trimmed.strip_prefix("* ") {
        ('*', rest)
    } else if let Some(rest) = trimmed.strip_prefix("+ ") {
        ('+', rest)
    } else {
        return None;
    };

    // Need `[X] x` (4 ASCII bytes plus the body).
    let bytes = after_bullet.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'[' || bytes[2] != b']' || bytes[3] != b' ' {
        return None;
    }
    let done_marker = &after_bullet[..4]; // "[ ] " or "[x] " or "[X] "
    let body_with_at = &after_bullet[4..];

    // Pure body manipulation: strip any prior owner prefix, then
    // optionally prepend the new one with the canonical em-dash.
    let stripped = strip_leading_owner_segment(body_with_at);
    let new_body = match new_owner {
        Some(name) => {
            let trimmed_name = name.trim();
            if trimmed_name.is_empty() {
                stripped.to_string()
            } else {
                format!("{trimmed_name} — {stripped}")
            }
        }
        None => stripped.to_string(),
    };

    Some(format!("{indent}{bullet_char} {done_marker}{new_body}"))
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

/// Match `- [?] question text` / `* [?] …` / `+ [?] …` lines (#113).
/// Returns the trimmed question text with any trailing
/// ` → answer: …` segment stripped — that segment only appears on
/// resolved-and-flipped lines, so an open `[?]` line shouldn't have
/// one, but be tolerant.
fn parse_question_line(line: &str) -> Option<String> {
    let after_bullet = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;
    let bytes = after_bullet.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'[' || bytes[2] != b']' || bytes[3] != b' ' {
        return None;
    }
    if bytes[1] != b'?' {
        return None;
    }
    let raw_text = after_bullet[4..].trim();
    if raw_text.is_empty() {
        return None;
    }
    Some(strip_trailing_answer_segment(raw_text).to_string())
}

/// Drop a trailing ` → answer: …` segment (the marker we append when
/// resolving a question via the IPC). U+2192 is `→`. Tolerates the
/// ASCII fallback `-> answer:` too in case a user types it by hand.
fn strip_trailing_answer_segment(text: &str) -> &str {
    for marker in [" \u{2192} answer:", " -> answer:"] {
        if let Some(idx) = text.find(marker) {
            return text[..idx].trim_end();
        }
    }
    text
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

/// Stable id for an open-question row (#113). The `q:` infix
/// distinguishes question ids from action ids ("<bundle>:<hash>") so
/// the same bundle can carry both without colliding in
/// `events.ref_id` / `embeddings.ref_id` payloads.
pub(crate) fn open_question_id(bundle_id: &str, text: &str) -> String {
    format!("{bundle_id}:q:{}", action_text_hash(text))
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
/// FK ON DELETE CASCADE handles tags / actions / meeting_attendees;
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

/// One-shot reparse for the open-questions migration (#113). On the
/// first boot after #027, walk every note row and run
/// `parse_indexable_from_body` + `upsert_in_tx`, which writes any
/// `- [?]` lines into the new `note_open_questions` table.
/// Idempotent: gated by the `questions_backfill_done` meta flag.
pub fn questions_backfill_if_pending(
    conn: &mut rusqlite::Connection,
) -> Result<(), String> {
    let done: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'questions_backfill_done'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "1".to_string());
    if done == "1" {
        return Ok(());
    }

    let rows: Vec<(String, String, String, i64)> = {
        let mut stmt = conn
            .prepare("SELECT id, bundle_id, body_md, modified_ms FROM notes")
            .map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        mapped.filter_map(|r| r.ok()).collect()
    };

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for (id, bundle_id, body_md, modified_ms) in &rows {
        let parsed = crate::index::parse_indexable_from_body(bundle_id, body_md, *modified_ms);
        crate::index::upsert_in_tx(&tx, id, &parsed).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;

    conn.execute(
        "UPDATE meta SET value = '1' WHERE key = 'questions_backfill_done'",
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

        let actions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions WHERE origin_note_id = 'abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actions, 1);

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

    // ----- #113 open questions -----

    #[test]
    fn parse_open_questions_basic() {
        let body = "intro\n- [?] foo\n- [?] Sarah — bar\n- [ ] not a question\n";
        let got = parse_open_questions(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].line, 2);
        assert_eq!(got[0].text, "foo");
        assert!(got[0].owner_candidate.is_none());
        assert_eq!(got[1].text, "Sarah — bar");
        assert_eq!(got[1].owner_candidate.as_deref(), Some("Sarah"));
    }

    #[test]
    fn parse_open_questions_alt_bullets() {
        let body = "* [?] starred\n+ [?] plussed\n";
        let got = parse_open_questions(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "starred");
        assert_eq!(got[1].text, "plussed");
    }

    #[test]
    fn parse_open_questions_skips_code_fences() {
        let body = "```\n- [?] inside fence\n```\n- [?] after fence\n";
        let got = parse_open_questions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "after fence");
    }

    #[test]
    fn parse_open_questions_ignores_action_marker() {
        // `- [x]` (resolved-as-action shape) shouldn't be picked up by
        // the question parser. The questions table tracks rows by id;
        // a flipped marker is identified via existing-row state, not
        // by re-parsing the new line as a question.
        let body = "- [x] looks resolved\n- [ ] open action\n";
        assert!(parse_open_questions(body).is_empty());
    }

    #[test]
    fn parse_open_questions_strips_trailing_answer() {
        // Tolerate ` → answer: …` on the line so a manually-edited
        // resolved question that the user typed back into `[?]` still
        // hashes to the original text.
        let body = "- [?] foo \u{2192} answer: yes\n";
        let got = parse_open_questions(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "foo");
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

    // The pre-#112 `delete_note_in` path-validation tests are gone:
    // delete_note now operates on a `note_id` against the DB. The
    // FK CASCADE in migration 026 handles the dependent-rows cleanup
    // and there's no filesystem path to validate anymore.

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

    // ---------- #51 owner rewrite -------------------------------------

    #[test]
    fn rewrite_action_owner_replaces_existing_prefix() {
        let got = rewrite_action_owner("- [ ] Heike — task", Some("Tom Ruesch"));
        assert_eq!(got.as_deref(), Some("- [ ] Tom Ruesch — task"));
    }

    #[test]
    fn rewrite_action_owner_prepends_when_absent() {
        let got = rewrite_action_owner("- [ ] task", Some("Tom Ruesch"));
        assert_eq!(got.as_deref(), Some("- [ ] Tom Ruesch — task"));
    }

    #[test]
    fn rewrite_action_owner_strips_when_unassigning() {
        let got = rewrite_action_owner("- [ ] Heike — task", None);
        assert_eq!(got.as_deref(), Some("- [ ] task"));
    }

    #[test]
    fn rewrite_action_owner_unassign_no_prefix_is_noop() {
        let got = rewrite_action_owner("- [ ] task", None);
        assert_eq!(got.as_deref(), Some("- [ ] task"));
    }

    #[test]
    fn rewrite_action_owner_canonicalizes_separator() {
        // En-dash + double-hyphen separators get rewritten as em-dash.
        let got = rewrite_action_owner("- [ ] Heike – task", Some("Tom Ruesch"));
        assert_eq!(got.as_deref(), Some("- [ ] Tom Ruesch — task"));
        let got = rewrite_action_owner("- [ ] Heike -- task", Some("Tom Ruesch"));
        assert_eq!(got.as_deref(), Some("- [ ] Tom Ruesch — task"));
    }

    #[test]
    fn rewrite_action_owner_preserves_due_token() {
        let got = rewrite_action_owner("- [ ] Heike — task @2026-05-15", Some("Tom"));
        assert_eq!(got.as_deref(), Some("- [ ] Tom — task @2026-05-15"));
    }

    #[test]
    fn rewrite_action_owner_preserves_done_marker_and_indent() {
        let got = rewrite_action_owner("\t- [x] Heike — task", Some("Tom"));
        assert_eq!(got.as_deref(), Some("\t- [x] Tom — task"));
        let got = rewrite_action_owner("  * [X] Heike — task", Some("Tom"));
        assert_eq!(got.as_deref(), Some("  * [X] Tom — task"));
        let got = rewrite_action_owner("+ [ ] Heike — task", None);
        assert_eq!(got.as_deref(), Some("+ [ ] task"));
    }

    #[test]
    fn rewrite_action_owner_returns_none_for_non_checkbox() {
        assert_eq!(rewrite_action_owner("plain text line", Some("Tom")), None);
        assert_eq!(rewrite_action_owner("- not a checkbox", Some("Tom")), None);
        assert_eq!(rewrite_action_owner("[x] bare bullet missing", Some("Tom")), None);
    }

    #[test]
    fn strip_leading_owner_segment_handles_all_separators() {
        assert_eq!(strip_leading_owner_segment("Tom — task"), "task");
        assert_eq!(strip_leading_owner_segment("Tom – task"), "task");
        assert_eq!(strip_leading_owner_segment("Tom -- task"), "task");
    }

    #[test]
    fn strip_leading_owner_segment_leaves_compact_dashes_alone() {
        assert_eq!(strip_leading_owner_segment("Tom—task"), "Tom—task");
        assert_eq!(
            strip_leading_owner_segment("Refactor self-driving cars"),
            "Refactor self-driving cars"
        );
    }

    #[test]
    fn strip_leading_owner_segment_no_separator_unchanged() {
        assert_eq!(strip_leading_owner_segment("write spec"), "write spec");
        assert_eq!(strip_leading_owner_segment(""), "");
    }
}
