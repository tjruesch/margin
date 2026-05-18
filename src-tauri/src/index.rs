//! SQLite-backed index for the notes layer.
//!
//! The index is a derived cache: markdown bundles on disk are
//! source-of-truth for everything user-meaningful (body, tags, future
//! `favorite`/`archived` frontmatter flags). Wiping `index.db` is safe;
//! `reconcile()` rebuilds it by walking `~/.margin/notes/`.
//!
//! All write paths go through `upsert(...)` / `remove(...)` from a
//! single `Mutex<Connection>` held as Tauri state. Index errors are
//! logged and swallowed at the call site — the next watcher event or
//! boot reconcile will heal any drift.
//!
//! See `src/migrations/001_init.sql` for the schema.
//!
//! Search (#31) is exposed via `search_notes`: FTS5 over title+body
//! against `notes_fts` plus a per-bundle `transcript.json` substring scan
//! merged into the same ranked result list.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::{params, Connection, OptionalExtension, Result, Transaction};
use serde::Serialize;

use crate::notes::{
    action_id, extract_preview, open_question_id, parse_actions, parse_open_questions,
    ActionListItem, ActionScope, NoteListItem, NoteScope, ParsedAction, ParsedQuestion,
    NOTE_FILENAME, TRANSCRIPT_FILENAME,
};
use crate::paths;

const SCHEMA_V1: &str = include_str!("migrations/001_init.sql");
const SCHEMA_V2: &str = include_str!("migrations/002_archived.sql");
const SCHEMA_V3: &str = include_str!("migrations/003_favorite.sql");
const SCHEMA_V4: &str = include_str!("migrations/004_actions.sql");
const SCHEMA_V5: &str = include_str!("migrations/005_due_dates.sql");
const SCHEMA_V6: &str = include_str!("migrations/006_team_members.sql");
const SCHEMA_V7: &str = include_str!("migrations/007_action_owners.sql");
const SCHEMA_V8: &str = include_str!("migrations/008_connectors.sql");
const SCHEMA_V9: &str = include_str!("migrations/009_calendar.sql");
const SCHEMA_V10: &str = include_str!("migrations/010_event_note_link.sql");
const SCHEMA_V11: &str = include_str!("migrations/011_email.sql");
const SCHEMA_V12: &str = include_str!("migrations/012_workstreams.sql");
const SCHEMA_V13: &str = include_str!("migrations/013_workstream_user_notes.sql");
const SCHEMA_V14: &str = include_str!("migrations/014_workstream_archive_resurface.sql");
const SCHEMA_V15: &str = include_str!("migrations/015_workstream_owner.sql");
const SCHEMA_V16: &str = include_str!("migrations/016_workstream_signals.sql");
const SCHEMA_V17: &str = include_str!("migrations/017_typed_aliases.sql");
const SCHEMA_V18: &str = include_str!("migrations/018_workstream_links.sql");
const SCHEMA_V19: &str = include_str!("migrations/019_workstream_parent.sql");
const SCHEMA_V20: &str = include_str!("migrations/020_workstream_link_summary.sql");
const SCHEMA_V21: &str = include_str!("migrations/021_workstream_action_assignee.sql");
const SCHEMA_V22: &str = include_str!("migrations/022_events_edges.sql");
const SCHEMA_V23: &str = include_str!("migrations/023_embeddings.sql");
const SCHEMA_V24: &str = include_str!("migrations/024_teams.sql");
const SCHEMA_V25: &str = include_str!("migrations/025_unify_actions.sql");
const SCHEMA_V26: &str = include_str!("migrations/026_notes_to_db.sql");
const SCHEMA_V27: &str = include_str!("migrations/027_open_questions.sql");
const SCHEMA_V28: &str = include_str!("migrations/028_profile_snapshots.sql");
const SCHEMA_V29: &str = include_str!("migrations/029_profile_observations.sql");
const SCHEMA_V30: &str = include_str!("migrations/030_action_waiting.sql");
const SCHEMA_V31: &str = include_str!("migrations/031_auto_resolve_hysteresis.sql");
const SCHEMA_V32: &str = include_str!("migrations/032_drop_profile_md_path.sql");
const SCHEMA_V33: &str = include_str!("migrations/033_calendar_series_master_id.sql");
const SCHEMA_V34: &str = include_str!("migrations/034_workstream_signal_tombstone.sql");
const SCHEMA_V35: &str = include_str!("migrations/035_chat_conversations.sql");
const SCHEMA_V36: &str = include_str!("migrations/036_prompt_dumps.sql");
const SCHEMA_V37: &str = include_str!("migrations/037_prompt_dumps_telemetry.sql");
const SCHEMA_V38: &str = include_str!("migrations/038_prompt_cache_tokens.sql");
const SCHEMA_V39: &str = include_str!("migrations/039_reconcile_origin.sql");
const SCHEMA_V40: &str = include_str!("migrations/040_actions_migration_flag.sql");
const SCHEMA_VERSION: i64 = 40;

/// Register the sqlite-vec extension as an "auto extension" so every
/// future `Connection::open*` in this process loads `vec0` (#104).
/// Idempotent via `Once`; safe to call repeatedly.
fn ensure_sqlite_vec_auto_extension() {
    static VEC_INIT: Once = Once::new();
    VEC_INIT.call_once(|| unsafe {
        // sqlite-vec exposes its init as `extern "C" fn()` — the C ABI
        // underneath actually takes (sqlite3*, char**, sqlite3_api_routines*)
        // and returns int. We transmute the fn-pointer to the shape
        // sqlite3_auto_extension expects.
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// Load vec0 into the given connection. For new connections this is
/// covered by the auto-extension, but the in-memory `Connection::open`
/// done by some test helpers happens before any auto-extension call.
/// Calling this on an already-loaded connection is a cheap no-op.
pub(crate) fn ensure_vec_loaded_on(conn: &Connection) -> Result<()> {
    ensure_sqlite_vec_auto_extension();
    // Probe: does this connection already know `vec_version()`? If yes,
    // the auto-extension caught it on open and we're done.
    let probe: rusqlite::Result<String> =
        conn.query_row("SELECT vec_version()", [], |r| r.get(0));
    if probe.is_ok() {
        return Ok(());
    }
    // Otherwise the connection pre-dates auto-extension registration.
    // Invoke the init function directly via FFI on this conn's handle.
    type ExtInit = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
    unsafe {
        let entry: ExtInit =
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        let mut err: *mut std::os::raw::c_char = std::ptr::null_mut();
        let rc = entry(conn.handle(), &mut err, std::ptr::null());
        if rc != 0 {
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    Ok(())
}

/// Open the index DB at `db_path` (creating it if absent) and apply any
/// pending migrations.
pub fn open_or_init(db_path: &Path) -> Result<Connection> {
    ensure_sqlite_vec_auto_extension();
    if let Some(parent) = db_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            })?;
        }
    }
    let conn = Connection::open(db_path)?;
    apply_migrations(&conn)?;
    Ok(conn)
}

pub(crate) fn apply_migrations(conn: &Connection) -> Result<()> {
    // Ensure vec0 is available before SCHEMA_V23's CREATE VIRTUAL TABLE.
    ensure_vec_loaded_on(conn)?;
    // `meta` doesn't exist on a fresh DB — `query_row` returns
    // QueryReturnedNoRows in that case (mapped to None via `optional`),
    // but the table-missing error is a different shape and would surface
    // here. Keep the unwrap_or so a fresh DB falls into the V1 branch.
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);

    let mut version = current.unwrap_or(0);
    if version == 0 {
        conn.execute_batch(SCHEMA_V1)?;
        version = 1;
    }
    if version == 1 {
        conn.execute_batch(SCHEMA_V2)?;
        version = 2;
    }
    if version == 2 {
        conn.execute_batch(SCHEMA_V3)?;
        version = 3;
    }
    if version == 3 {
        conn.execute_batch(SCHEMA_V4)?;
        // The new `actions` table is empty but the existing `notes`
        // rows still match disk on mtime+size, so a vanilla reconcile
        // would skip them and the actions feed would stay empty until
        // each note is re-saved. Sentinel `-1` busts the cheap-check
        // so the very next reconcile re-reads every note and populates
        // actions for free.
        conn.execute("UPDATE notes SET body_size = -1", [])?;
        version = 4;
    }
    if version == 4 {
        // Adds `due_ms` and `reminder_sent_ms` to actions for inline
        // due-date scheduling (#43). The migration also sets
        // body_size = -1 so reconcile re-reads every note and back-fills
        // due_ms for any pre-existing absolute `@YYYY-MM-DD` tokens.
        conn.execute_batch(SCHEMA_V5)?;
        version = 5;
    }
    if version == 5 {
        conn.execute_batch(SCHEMA_V6)?;
        version = 6;
    }
    if version == 6 {
        conn.execute_batch(SCHEMA_V7)?;
        version = 7;
    }
    if version == 7 {
        conn.execute_batch(SCHEMA_V8)?;
        version = 8;
    }
    if version == 8 {
        conn.execute_batch(SCHEMA_V9)?;
        version = 9;
    }
    if version == 9 {
        conn.execute_batch(SCHEMA_V10)?;
        version = 10;
    }
    if version == 10 {
        conn.execute_batch(SCHEMA_V11)?;
        version = 11;
    }
    if version == 11 {
        conn.execute_batch(SCHEMA_V12)?;
        version = 12;
    }
    if version == 12 {
        conn.execute_batch(SCHEMA_V13)?;
        version = 13;
    }
    if version == 13 {
        conn.execute_batch(SCHEMA_V14)?;
        version = 14;
    }
    if version == 14 {
        conn.execute_batch(SCHEMA_V15)?;
        version = 15;
    }
    if version == 15 {
        conn.execute_batch(SCHEMA_V16)?;
        version = 16;
    }
    if version == 16 {
        conn.execute_batch(SCHEMA_V17)?;
        version = 17;
    }
    if version == 17 {
        conn.execute_batch(SCHEMA_V18)?;
        version = 18;
    }
    if version == 18 {
        conn.execute_batch(SCHEMA_V19)?;
        version = 19;
    }
    if version == 19 {
        conn.execute_batch(SCHEMA_V20)?;
        version = 20;
    }
    if version == 20 {
        conn.execute_batch(SCHEMA_V21)?;
        version = 21;
    }
    if version == 21 {
        conn.execute_batch(SCHEMA_V22)?;
        version = 22;
    }
    if version == 22 {
        conn.execute_batch(SCHEMA_V23)?;
        version = 23;
    }
    if version == 23 {
        conn.execute_batch(SCHEMA_V24)?;
        version = 24;
    }
    if version == 24 {
        conn.execute_batch(SCHEMA_V25)?;
        version = 25;
    }
    if version == 25 {
        conn.execute_batch(SCHEMA_V26)?;
        version = 26;
    }
    if version == 26 {
        conn.execute_batch(SCHEMA_V27)?;
        version = 27;
    }
    if version == 27 {
        conn.execute_batch(SCHEMA_V28)?;
        version = 28;
    }
    if version == 28 {
        conn.execute_batch(SCHEMA_V29)?;
        version = 29;
    }
    if version == 29 {
        conn.execute_batch(SCHEMA_V30)?;
        version = 30;
    }
    if version == 30 {
        conn.execute_batch(SCHEMA_V31)?;
        version = 31;
    }
    if version == 31 {
        conn.execute_batch(SCHEMA_V32)?;
        version = 32;
    }
    if version == 32 {
        conn.execute_batch(SCHEMA_V33)?;
        version = 33;
    }
    if version == 33 {
        conn.execute_batch(SCHEMA_V34)?;
        version = 34;
    }
    if version == 34 {
        conn.execute_batch(SCHEMA_V35)?;
        version = 35;
    }
    if version == 35 {
        conn.execute_batch(SCHEMA_V36)?;
        version = 36;
    }
    if version == 36 {
        conn.execute_batch(SCHEMA_V37)?;
        version = 37;
    }
    if version == 37 {
        conn.execute_batch(SCHEMA_V38)?;
        version = 38;
    }
    if version == 38 {
        conn.execute_batch(SCHEMA_V39)?;
        version = 39;
    }
    if version == 39 {
        conn.execute_batch(SCHEMA_V40)?;
        version = 40;
    }
    if version != SCHEMA_VERSION {
        // Future: bump SCHEMA_VERSION and add another step above.
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

/// Drop a note (and its tags + FTS rows) from the index. No-op if absent.
pub fn remove(conn: &mut Connection, note_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    remove_in_tx(&tx, note_id)?;
    tx.commit()
}

/// All indexed notes within `scope`, newest-first by `modified_ms`.
/// Same row shape as the pre-DB `notes::list_notes` so the frontend
/// doesn't need to change.
pub fn list_all(conn: &Connection, scope: NoteScope) -> Result<Vec<NoteListItem>> {
    let where_clause = match scope {
        // Active scope hides archived. Favorited notes that are also
        // archived stay hidden — archive takes precedence (a deliberate
        // choice; a user wanting them visible should unarchive).
        NoteScope::Active => "WHERE n.archived = 0",
        NoteScope::Archived => "WHERE n.archived = 1",
        NoteScope::Favorites => "WHERE n.favorite = 1 AND n.archived = 0",
        NoteScope::All => "",
    };
    let sql = format!(
        "SELECT n.id, n.title, n.modified_ms, n.duration_ms, n.preview, n.favorite \
         FROM notes n {where_clause} ORDER BY n.modified_ms DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(NoteRow {
            note_path: r.get(0)?,
            title: r.get(1)?,
            modified_ms: r.get(2)?,
            duration_ms: r.get(3)?,
            preview: r.get(4)?,
            favorite: r.get::<_, i64>(5)? != 0,
        })
    })?;

    let mut bare: Vec<NoteRow> = Vec::new();
    for row in rows {
        bare.push(row?);
    }

    let tags_by_path = load_tags_grouped(conn)?;

    Ok(bare
        .into_iter()
        .map(|r| NoteListItem {
            tags: tags_by_path.get(&r.note_path).cloned().unwrap_or_default(),
            note_path: r.note_path,
            title: r.title,
            modified_ms: r.modified_ms,
            duration_ms: r.duration_ms.map(|v| v as u64),
            preview: r.preview,
            favorite: r.favorite,
        })
        .collect())
}

/// Action items across both origins, served from the unified `actions`
/// table (#111). Each row carries an `origin_kind` so the frontend can
/// route click-through (note-origin → editor, synth → workstream
/// detail) and so the unified write IPCs can dispatch correctly.
///
/// Note-origin rows on archived notes and synth rows on non-active
/// workstreams are filtered out — their actions are out of sight.
pub fn list_actions(
    conn: &Connection,
    scope: ActionScope,
    assignee_id: Option<&str>,
    workstream_id: Option<&str>,
    subject_member_id: Option<&str>,
    origin_synth_kinds_json: Option<&str>,
) -> Result<Vec<ActionListItem>> {
    let where_done = match scope {
        ActionScope::Open => "AND a.done = 0",
        ActionScope::Done => "AND a.done = 1",
        ActionScope::All => "",
    };
    // `(?N IS NULL OR <col> = ?N)` lets us bind every optional filter
    // unconditionally; SQLite short-circuits when the bound value is
    // NULL. Avoids dynamic-params gymnastics.
    //
    // The `origin_synth_kinds_json` filter is a JSON array of strings
    // (or NULL); rows match when their `origin_synth_kind` appears in
    // the array. `json_each` makes the IN-list portable to any length
    // without dynamic placeholders.
    //
    // The visibility guard: a row is visible iff
    //   - it has no origin note OR its origin note is non-archived, AND
    //   - it has no workstream attachment OR its workstream is active.
    let sql = format!(
        "SELECT a.id, a.origin_kind, a.origin_note_id, a.origin_line, \
                a.origin_synth_kind, a.origin_synth_id, \
                a.workstream_id, a.text, a.done, a.due_ms, \
                a.assignee_id, a.created_ms, \
                n.title AS note_title, \
                w.title AS workstream_title, \
                t.display_name AS assignee_display_name, \
                a.subject_member_id, a.manual_override, a.auto_resolved_ms, \
                COALESCE(n.modified_ms, w.last_activity_ms, a.created_ms) AS order_ms \
           FROM actions a \
           LEFT JOIN notes        n ON n.id        = a.origin_note_id \
           LEFT JOIN workstreams  w ON w.id        = a.workstream_id \
           LEFT JOIN team_members t ON t.id        = a.assignee_id \
          WHERE (a.origin_note_id IS NULL OR n.archived = 0) \
            AND (a.workstream_id  IS NULL OR w.status   = 'active') \
            {where_done} \
            AND (?1 IS NULL OR a.assignee_id        = ?1) \
            AND (?2 IS NULL OR a.workstream_id      = ?2) \
            AND (?3 IS NULL OR a.subject_member_id  = ?3) \
            AND (?4 IS NULL OR a.origin_synth_kind IN (SELECT value FROM json_each(?4))) \
          ORDER BY (a.due_ms IS NULL), a.due_ms ASC, order_ms DESC, \
                   a.origin_line ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![assignee_id, workstream_id, subject_member_id, origin_synth_kinds_json],
        |r| {
            Ok(ActionListItem {
                id: r.get(0)?,
                origin_kind: r.get(1)?,
                origin_note_path: r.get(2)?,
                origin_line: r.get(3)?,
                origin_synth_kind: r.get(4)?,
                origin_synth_id: r.get(5)?,
                workstream_id: r.get(6)?,
                text: r.get(7)?,
                done: r.get::<_, i64>(8)? != 0,
                due_ms: r.get(9)?,
                assignee_id: r.get(10)?,
                created_ms: r.get(11)?,
                note_title: r.get(12)?,
                workstream_title: r.get(13)?,
                assignee_display_name: r.get(14)?,
                subject_member_id: r.get(15)?,
                manual_override: r.get::<_, i64>(16)? != 0,
                auto_resolved_ms: r.get(17)?,
            })
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Per-note actions for the note-view sidebar (#145). Returns every
/// row whose `origin_note_id` matches, regardless of `done`,
/// regardless of `origin_kind`, ignoring the archived-note /
/// inactive-workstream guards (the user is *looking at* the note —
/// they should see its actions). Ordered created_ms DESC with
/// `origin_line` as a stable tiebreaker for note-origin rows.
pub fn list_actions_for_note(
    conn: &Connection,
    note_id: &str,
) -> Result<Vec<ActionListItem>> {
    let sql = "SELECT a.id, a.origin_kind, a.origin_note_id, a.origin_line, \
                      a.origin_synth_kind, a.origin_synth_id, \
                      a.workstream_id, a.text, a.done, a.due_ms, \
                      a.assignee_id, a.created_ms, \
                      n.title AS note_title, \
                      w.title AS workstream_title, \
                      t.display_name AS assignee_display_name, \
                      a.subject_member_id, a.manual_override, a.auto_resolved_ms \
                 FROM actions a \
                 LEFT JOIN notes        n ON n.id = a.origin_note_id \
                 LEFT JOIN workstreams  w ON w.id = a.workstream_id \
                 LEFT JOIN team_members t ON t.id = a.assignee_id \
                WHERE a.origin_note_id = ?1 \
                ORDER BY a.created_ms DESC, a.origin_line ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![note_id], |r| {
        Ok(ActionListItem {
            id: r.get(0)?,
            origin_kind: r.get(1)?,
            origin_note_path: r.get(2)?,
            origin_line: r.get(3)?,
            origin_synth_kind: r.get(4)?,
            origin_synth_id: r.get(5)?,
            workstream_id: r.get(6)?,
            text: r.get(7)?,
            done: r.get::<_, i64>(8)? != 0,
            due_ms: r.get(9)?,
            assignee_id: r.get(10)?,
            created_ms: r.get(11)?,
            note_title: r.get(12)?,
            workstream_title: r.get(13)?,
            assignee_display_name: r.get(14)?,
            subject_member_id: r.get(15)?,
            manual_override: r.get::<_, i64>(16)? != 0,
            auto_resolved_ms: r.get(17)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// One ranked hit from `search_notes`. `source` carries which surface
/// the match came from so the UI can label rows ("Title", "Body",
/// "Transcript"). `snippet` is a short window around the match — already
/// truncated, ready to render.
#[derive(Serialize, Clone)]
pub struct SearchHit {
    pub note_path: String,
    pub bundle_id: String,
    pub title: String,
    pub modified_ms: i64,
    pub snippet: String,
    pub source: SearchSource,
    /// Lower is better (mirrors SQLite's bm25). Transcript hits get a
    /// synthetic score so they slot in alongside FTS rows.
    pub score: f64,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchSource {
    Title,
    Body,
    Transcript,
}

const SEARCH_SNIPPET_OPEN: &str = "\u{2068}";
const SEARCH_SNIPPET_CLOSE: &str = "\u{2069}";
const SEARCH_TRANSCRIPT_WINDOW: usize = 80;

/// Combined FTS + transcript search. Excludes archived notes (mirrors the
/// `Active` scope in `list_all`). Caller picks the per-source caps via
/// `limit`; total results = `limit` (FTS hits favored, transcript hits
/// fill the remainder).
pub fn search_notes(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let cap = limit.min(50);

    let fts_query = match build_fts_query(trimmed) {
        Some(q) => q,
        None => return Ok(Vec::new()),
    };

    let mut hits: Vec<SearchHit> = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // FTS pass — pull bm25() and a body snippet in one go. JOIN on
    // `notes` to drop archived rows and to fetch the canonical title /
    // mtime / bundle_id (the FTS row's `title` is duplicated for ranking
    // purposes; `notes.title` is the source-of-truth).
    let fts_sql = "\
        SELECT n.id, n.bundle_id, n.title, n.modified_ms, \
               snippet(notes_fts, 2, ?2, ?3, '…', 12) AS body_snip, \
               bm25(notes_fts) AS score \
        FROM notes_fts \
        JOIN notes n ON n.id = notes_fts.note_id \
        WHERE notes_fts MATCH ?1 AND n.archived = 0 \
        ORDER BY score ASC \
        LIMIT ?4";
    let mut stmt = conn.prepare(fts_sql)?;
    let rows = stmt.query_map(
        params![
            &fts_query,
            SEARCH_SNIPPET_OPEN,
            SEARCH_SNIPPET_CLOSE,
            cap as i64,
        ],
        |r| {
            Ok(FtsRow {
                note_path: r.get(0)?,
                bundle_id: r.get(1)?,
                title: r.get(2)?,
                modified_ms: r.get(3)?,
                body_snip: r.get(4)?,
                score: r.get(5)?,
            })
        },
    )?;

    let needle_lc = trimmed.to_lowercase();
    for row in rows {
        let row = row?;
        let title_lc = row.title.to_lowercase();
        let (source, snippet) = if title_lc.contains(&needle_lc) {
            (SearchSource::Title, row.title.clone())
        } else {
            (SearchSource::Body, row.body_snip.clone())
        };
        seen_paths.insert(row.note_path.clone());
        hits.push(SearchHit {
            note_path: row.note_path,
            bundle_id: row.bundle_id,
            title: row.title,
            modified_ms: row.modified_ms,
            snippet,
            source,
            score: row.score,
        });
    }

    // Transcript pass — we need the user's notes dir, but `index.rs`
    // doesn't take it as a parameter elsewhere. Use `paths::notes_dir()`
    // for symmetry with `upsert`. Walk every non-archived bundle whose
    // path we haven't already surfaced and substring-scan the segments.
    if hits.len() < cap {
        let archived_paths: std::collections::HashSet<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM notes WHERE archived = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = std::collections::HashSet::new();
            for r in rows {
                set.insert(r?);
            }
            set
        };
        let titles_by_path: HashMap<String, (String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT id, bundle_id, title, modified_ms FROM notes \
                 WHERE archived = 0",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            let mut map = HashMap::new();
            for row in rows {
                let (p, b, t, m) = row?;
                map.insert(p, (b, t, m));
            }
            map
        };

        let remaining = cap - hits.len();
        let transcript_hits = scan_transcripts(
            &paths::notes_dir(),
            trimmed,
            &seen_paths,
            &archived_paths,
            &titles_by_path,
            remaining,
        );
        hits.extend(transcript_hits);
    }

    Ok(hits)
}

struct FtsRow {
    note_path: String,
    bundle_id: String,
    title: String,
    modified_ms: i64,
    body_snip: String,
    score: f64,
}

/// One entry in the "all non-archived notes" directory the AI ask
/// command builds for citation. Lighter than `SearchHit` — just enough
/// to render a chip and ground the model on what exists.
#[derive(Serialize, Clone)]
pub struct DirectoryEntry {
    pub note_path: String,
    pub bundle_id: String,
    pub title: String,
    pub modified_ms: i64,
    pub preview: String,
}

/// All non-archived notes, newest-first, capped at `limit`. Used as the
/// "Notes directory" section of the AI ask prompt — every entry gets a
/// `[N]` label the model can cite even when its body wasn't deep-loaded
/// into the retrieved set.
pub fn list_directory(conn: &Connection, limit: usize) -> Result<Vec<DirectoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, bundle_id, title, modified_ms, preview \
         FROM notes WHERE archived = 0 \
         ORDER BY modified_ms DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(DirectoryEntry {
            note_path: r.get(0)?,
            bundle_id: r.get(1)?,
            title: r.get(2)?,
            modified_ms: r.get(3)?,
            preview: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// AI-question retrieval: stopword-filtered OR query over title+body
/// (and a per-token transcript scan to fill remaining slots). Used by
/// the Ask palette where the input is a full natural-language question
/// rather than a narrowing keyword filter — strict AND semantics from
/// the lexical search would reject most candidates because question
/// words like "what", "did", "is" rarely co-occur with topic terms in
/// the same note.
///
/// Falls back to the most-recently-modified non-archived notes when no
/// content tokens remain (e.g. user asked "what's new?") so the model
/// always has something to reason over.
pub fn retrieve_for_ask(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let cap = limit.min(50);
    let tokens = content_tokens(query);

    if tokens.is_empty() {
        return recent_notes(conn, cap);
    }

    // OR'd FTS query — bm25 ranks docs with more matching terms higher
    // automatically, so OR + bm25 ≈ best-effort topic recall.
    let fts_query = tokens
        .iter()
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");

    let mut hits: Vec<SearchHit> = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    let fts_sql = "\
        SELECT n.id, n.bundle_id, n.title, n.modified_ms, \
               snippet(notes_fts, 2, ?2, ?3, '…', 16) AS body_snip, \
               bm25(notes_fts) AS score \
        FROM notes_fts \
        JOIN notes n ON n.id = notes_fts.note_id \
        WHERE notes_fts MATCH ?1 AND n.archived = 0 \
        ORDER BY score ASC \
        LIMIT ?4";
    let mut stmt = conn.prepare(fts_sql)?;
    let rows = stmt.query_map(
        params![
            &fts_query,
            SEARCH_SNIPPET_OPEN,
            SEARCH_SNIPPET_CLOSE,
            cap as i64,
        ],
        |r| {
            Ok(FtsRow {
                note_path: r.get(0)?,
                bundle_id: r.get(1)?,
                title: r.get(2)?,
                modified_ms: r.get(3)?,
                body_snip: r.get(4)?,
                score: r.get(5)?,
            })
        },
    )?;
    for row in rows {
        let row = row?;
        seen_paths.insert(row.note_path.clone());
        let source = if tokens
            .iter()
            .any(|t| row.title.to_lowercase().contains(t.as_str()))
        {
            SearchSource::Title
        } else {
            SearchSource::Body
        };
        let snippet = if matches!(source, SearchSource::Title) {
            row.title.clone()
        } else {
            row.body_snip.clone()
        };
        hits.push(SearchHit {
            note_path: row.note_path,
            bundle_id: row.bundle_id,
            title: row.title,
            modified_ms: row.modified_ms,
            snippet,
            source,
            score: row.score,
        });
    }

    // Transcript pass — for ask retrieval we want any note whose
    // transcript contains *any* meaningful token, not just the full
    // query string. Cheap pre-filter: lowercased substring match per
    // token before JSON parse.
    if hits.len() < cap {
        let archived_paths: std::collections::HashSet<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM notes WHERE archived = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = std::collections::HashSet::new();
            for r in rows {
                set.insert(r?);
            }
            set
        };
        let titles_by_path: HashMap<String, (String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT id, bundle_id, title, modified_ms FROM notes \
                 WHERE archived = 0",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            let mut map = HashMap::new();
            for row in rows {
                let (p, b, t, m) = row?;
                map.insert(p, (b, t, m));
            }
            map
        };

        let remaining = cap - hits.len();
        let transcript_hits = scan_transcripts_any_token(
            &paths::notes_dir(),
            &tokens,
            &seen_paths,
            &archived_paths,
            &titles_by_path,
            remaining,
        );
        hits.extend(transcript_hits);
    }

    // Last-resort fallback: if even OR retrieval found nothing, hand
    // the model the most-recent notes so it can at least confirm
    // there's nothing relevant rather than refusing on empty context.
    if hits.is_empty() {
        return recent_notes(conn, cap);
    }

    Ok(hits)
}

/// Tokenize input the way the FTS tokenizer does, then drop stopwords
/// and very short tokens. Output is lowercased for consistent
/// case-insensitive matching downstream.
fn content_tokens(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-') {
        let lc = raw.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if lc.chars().count() < 3 {
            continue;
        }
        if STOPWORDS.contains(&lc.as_str()) {
            continue;
        }
        if !out.iter().any(|t| t == &lc) {
            out.push(lc);
        }
    }
    out
}

/// English stopwords — common question words, auxiliaries, and
/// determiners that almost never carry topical signal. Trimmed to
/// avoid removing too much.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can",
    "had", "has", "have", "her", "his", "him", "she", "they", "this", "that",
    "these", "those", "with", "from", "your", "yours", "ours", "their",
    "theirs", "what", "when", "where", "who", "whom", "why", "how", "which",
    "was", "were", "been", "being", "did", "does", "doing", "done", "would",
    "could", "should", "shall", "will", "may", "might", "must", "into",
    "about", "over", "under", "than", "then", "there", "here", "them",
    "some", "such", "very", "just", "also", "only", "more", "most", "much",
    "many", "few", "again", "still", "yet", "now", "before", "after",
    "between", "during", "while", "because", "though", "although", "even",
    "ever", "never", "always", "often", "sometimes", "really",
];

fn recent_notes(conn: &Connection, limit: usize) -> Result<Vec<SearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT id, bundle_id, title, modified_ms, preview \
         FROM notes WHERE archived = 0 \
         ORDER BY modified_ms DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (note_path, bundle_id, title, modified_ms, preview) = row?;
        out.push(SearchHit {
            note_path,
            bundle_id,
            title,
            modified_ms,
            snippet: preview,
            source: SearchSource::Body,
            score: 999.0,
        });
    }
    Ok(out)
}

fn scan_transcripts_any_token(
    notes_dir: &Path,
    tokens: &[String],
    seen_paths: &std::collections::HashSet<String>,
    archived_paths: &std::collections::HashSet<String>,
    titles_by_path: &HashMap<String, (String, String, i64)>,
    limit: usize,
) -> Vec<SearchHit> {
    if limit == 0 || tokens.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<SearchHit> = Vec::new();
    let read_dir = match fs::read_dir(notes_dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read_dir.flatten() {
        if out.len() >= limit {
            break;
        }
        let bundle = entry.path();
        if !bundle.is_dir() {
            continue;
        }
        let note_path = bundle.join(NOTE_FILENAME);
        let note_path_str = note_path.to_string_lossy().into_owned();
        if seen_paths.contains(&note_path_str)
            || archived_paths.contains(&note_path_str)
        {
            continue;
        }
        let transcript_path = bundle.join(TRANSCRIPT_FILENAME);
        if !transcript_path.exists() {
            continue;
        }
        let raw = match fs::read_to_string(&transcript_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let raw_lc = raw.to_lowercase();
        // Match any token (OR). First-token-wins for the snippet.
        let mut snippet: Option<String> = None;
        for tok in tokens {
            if raw_lc.contains(tok.as_str()) {
                let parsed: serde_json::Value =
                    match serde_json::from_str(&raw) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                let segments = match parsed.get("segments").and_then(|s| s.as_array()) {
                    Some(s) => s,
                    None => continue,
                };
                for seg in segments {
                    let text = seg.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if let Some((start, end)) = find_ci(text, tok) {
                        snippet = Some(transcript_snippet(text, start, end));
                        break;
                    }
                }
                if snippet.is_some() {
                    break;
                }
            }
        }
        let snippet = match snippet {
            Some(s) => s,
            None => continue,
        };
        let (bundle_id, title, modified_ms) = match titles_by_path.get(&note_path_str) {
            Some(v) => v.clone(),
            None => continue,
        };
        out.push(SearchHit {
            note_path: note_path_str,
            bundle_id,
            title,
            modified_ms,
            snippet,
            source: SearchSource::Transcript,
            score: 1.0,
        });
    }
    out
}

/// Translate user input into an FTS5 MATCH expression. Each
/// whitespace-separated token becomes a quoted prefix term so the query
/// never trips on FTS5 operators (`AND`, `NOT`, `*`, `:` etc) the user
/// happened to type. Returns `None` when the input has no usable tokens
/// (e.g. punctuation-only input).
fn build_fts_query(input: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for raw in input.split_whitespace() {
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'' || *c == '-' || *c == '_')
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        let escaped = cleaned.replace('"', "\"\"");
        parts.push(format!("\"{escaped}\"*"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn scan_transcripts(
    notes_dir: &Path,
    needle: &str,
    seen_paths: &std::collections::HashSet<String>,
    archived_paths: &std::collections::HashSet<String>,
    titles_by_path: &HashMap<String, (String, String, i64)>,
    limit: usize,
) -> Vec<SearchHit> {
    if limit == 0 {
        return Vec::new();
    }
    let needle_lc = needle.to_lowercase();
    let mut out: Vec<SearchHit> = Vec::new();

    let read_dir = match fs::read_dir(notes_dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read_dir.flatten() {
        if out.len() >= limit {
            break;
        }
        let bundle = entry.path();
        if !bundle.is_dir() {
            continue;
        }
        let note_path = bundle.join(NOTE_FILENAME);
        let note_path_str = note_path.to_string_lossy().into_owned();
        if seen_paths.contains(&note_path_str)
            || archived_paths.contains(&note_path_str)
        {
            continue;
        }
        let transcript_path = bundle.join(TRANSCRIPT_FILENAME);
        if !transcript_path.exists() {
            continue;
        }
        let raw = match fs::read_to_string(&transcript_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Cheap filter before parsing JSON — most transcripts won't
        // contain the needle at all.
        if !raw.to_lowercase().contains(&needle_lc) {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let segments = match parsed.get("segments").and_then(|s| s.as_array()) {
            Some(s) => s,
            None => continue,
        };
        let mut snippet: Option<String> = None;
        for seg in segments {
            let text = seg.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if let Some((start, end)) = find_ci(text, &needle_lc) {
                snippet = Some(transcript_snippet(text, start, end));
                break;
            }
        }
        let snippet = match snippet {
            Some(s) => s,
            None => continue,
        };
        let (bundle_id, title, modified_ms) = match titles_by_path.get(&note_path_str) {
            Some(v) => v.clone(),
            None => continue,
        };
        out.push(SearchHit {
            note_path: note_path_str,
            bundle_id,
            title,
            modified_ms,
            snippet,
            source: SearchSource::Transcript,
            // Transcript hits rank below FTS rows. bm25 returns negative
            // scores for stronger matches — pick a positive value to
            // place transcripts at the end deterministically.
            score: 1.0,
        });
    }
    out
}

/// Case-insensitive substring search returning a `(start, end)` byte
/// range in `haystack` aligned to char boundaries. `needle_lc` must
/// already be lowercase. Returns `None` if no match.
fn find_ci(haystack: &str, needle_lc: &str) -> Option<(usize, usize)> {
    let needle_chars: Vec<char> = needle_lc.chars().collect();
    if needle_chars.is_empty() {
        return None;
    }
    let hay: Vec<(usize, char)> = haystack.char_indices().collect();
    if hay.len() < needle_chars.len() {
        return None;
    }
    'outer: for i in 0..=hay.len() - needle_chars.len() {
        for (k, n) in needle_chars.iter().enumerate() {
            // Compare via single-char lowercase folding. This matches
            // ASCII and most Latin scripts; specialty cases like ß→SS
            // (which lowercases to two chars) won't align, and we
            // accept the false negative for v1.
            let lc = hay[i + k].1.to_lowercase().next().unwrap_or(hay[i + k].1);
            if lc != *n {
                continue 'outer;
            }
        }
        let start = hay[i].0;
        let end_idx = i + needle_chars.len();
        let end = if end_idx < hay.len() {
            hay[end_idx].0
        } else {
            haystack.len()
        };
        return Some((start, end));
    }
    None
}

/// Build a `…pre[needle]post…` snippet with bidirectional isolate marks
/// around the match. `start`/`end` are byte offsets into `text`; both
/// must be on char boundaries.
fn transcript_snippet(text: &str, start: usize, end: usize) -> String {
    let half = SEARCH_TRANSCRIPT_WINDOW / 2;

    let pre_byte = char_offset_back(&text[..start], half);
    let post_byte_rel = char_offset_forward(&text[end..], half);
    let post_byte = end + post_byte_rel;

    let mut snip = String::new();
    if pre_byte > 0 {
        snip.push('…');
    }
    snip.push_str(text[pre_byte..start].trim_start());
    snip.push_str(SEARCH_SNIPPET_OPEN);
    snip.push_str(&text[start..end]);
    snip.push_str(SEARCH_SNIPPET_CLOSE);
    snip.push_str(text[end..post_byte].trim_end());
    if post_byte < text.len() {
        snip.push('…');
    }
    snip
}

/// Byte index `n_chars` chars before the end of `prefix`, or 0 if
/// `prefix` is shorter than that.
fn char_offset_back(prefix: &str, n_chars: usize) -> usize {
    let total = prefix.chars().count();
    if total <= n_chars {
        return 0;
    }
    prefix
        .char_indices()
        .nth(total - n_chars)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Byte index `n_chars` chars into `suffix`, or `suffix.len()` if
/// `suffix` is shorter than that.
fn char_offset_forward(suffix: &str, n_chars: usize) -> usize {
    suffix
        .char_indices()
        .nth(n_chars)
        .map(|(i, _)| i)
        .unwrap_or(suffix.len())
}

// ---------- internals -----------------------------------------------------

struct NoteRow {
    note_path: String,
    title: String,
    modified_ms: i64,
    duration_ms: Option<i64>,
    preview: String,
    favorite: bool,
}

/// Pre-parsed view of a note's body for the upsert path (#112). Built
/// either from a `write_note` IPC call (body comes from the user) or
/// from the one-time disk-to-DB body backfill at boot (body comes
/// from the legacy `<bundle>/note.md` file).
pub(crate) struct Indexable {
    pub(crate) bundle_id: String,
    pub(crate) title: String,
    pub(crate) modified_ms: i64,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) preview: String,
    pub(crate) body_size: i64,
    pub(crate) actions: Vec<ParsedAction>,
    pub(crate) open_questions: Vec<ParsedQuestion>,
    pub(crate) body: String,
}

/// Build an `Indexable` from an in-memory `body_md` string. Title is
/// derived from the first `# Heading` line; actions parsed from
/// `- [ ]` lines. Duration is best-effort hydrated from
/// `<notes_dir>/<note_id>/transcript.json` when present — audio/
/// transcripts still live on disk after #112.
pub(crate) fn parse_indexable_from_body(
    bundle_id: &str,
    body_md: &str,
    modified_ms: i64,
) -> Indexable {
    let actions = parse_actions(body_md);
    let open_questions = parse_open_questions(body_md);
    let title = body_md
        .lines()
        .find_map(|l| {
            l.trim_start()
                .strip_prefix("# ")
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| "Untitled note".to_string());
    let preview = extract_preview(body_md);
    let body_size = body_md.len() as i64;

    let transcript_path = paths::notes_dir()
        .join(bundle_id)
        .join(TRANSCRIPT_FILENAME);
    let duration_ms = if transcript_path.exists() {
        fs::read_to_string(&transcript_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("duration_ms").and_then(|d| d.as_u64()))
    } else {
        None
    };

    Indexable {
        bundle_id: bundle_id.to_string(),
        title,
        modified_ms,
        duration_ms,
        preview,
        body_size,
        actions,
        open_questions,
        body: body_md.to_string(),
    }
}

/// Refresh the row for `note_id` (#112). UPDATEs the row in place,
/// re-derives title from the body, refreshes FTS, reparses
/// `- [ ]` lines into the unified `actions` table, and emits
/// `note_modified`/`action_created`/`action_completed` events — all
/// inside the supplied transaction.
///
/// `archived`/`favorite`/`tags` are *not* touched here. Those have
/// their own DB-only IPCs (`set_archived` / `set_favorite` /
/// `set_note_tags`). Use this on the body-change path only.
pub(crate) fn upsert_in_tx(
    tx: &Transaction<'_>,
    note_id: &str,
    p: &Indexable,
) -> Result<()> {
    // Snapshot pre-state for live event emission (#106) and the
    // action-diff that follows.
    let note_pre_existed: bool = tx
        .query_row(
            "SELECT 1 FROM notes WHERE id = ?1",
            params![note_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    let prior_actions: HashMap<String, bool> = {
        let mut stmt = tx.prepare(
            "SELECT id, done FROM actions \
              WHERE origin_kind = 'note' AND origin_note_id = ?1",
        )?;
        let rows = stmt.query_map(params![note_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let self_id: Option<String> = tx
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;

    // INSERT-on-create / UPDATE-body-only on existing. archived/
    // favorite live on their own IPCs and aren't refreshed here.
    tx.execute(
        "INSERT INTO notes(id, bundle_id, title, body_md, modified_ms, \
                           duration_ms, preview, body_size, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?5) \
         ON CONFLICT(id) DO UPDATE SET \
            bundle_id = excluded.bundle_id, \
            title = excluded.title, \
            body_md = excluded.body_md, \
            modified_ms = excluded.modified_ms, \
            duration_ms = excluded.duration_ms, \
            preview = excluded.preview, \
            body_size = excluded.body_size",
        params![
            note_id,
            p.bundle_id,
            p.title,
            p.body,
            p.modified_ms,
            p.duration_ms.map(|v| v as i64),
            p.preview,
            p.body_size,
        ],
    )?;

    // Emit the note event before tags/FTS/actions — keeps the events
    // table chronologically consistent with the notes table.
    let note_kind = if note_pre_existed {
        "note_modified"
    } else {
        "note_created"
    };
    let note_payload = serde_json::json!({
        "title": p.title,
        "bundle_id": p.bundle_id,
    });
    crate::events::emit(
        tx,
        p.modified_ms,
        note_kind,
        self_id.as_deref(),
        "note",
        note_id,
        &note_payload,
    )?;

    // FTS: rewrite the row from the new body_md.
    tx.execute(
        "DELETE FROM notes_fts WHERE note_id = ?1",
        params![note_id],
    )?;
    tx.execute(
        "INSERT INTO notes_fts(note_id, title, body) VALUES (?1, ?2, ?3)",
        params![note_id, p.title, p.body],
    )?;

    // Actions: replace wholesale (note-origin rows for this note
    // only). Synth-origin rows attached via workstream_id survive
    // because the DELETE is scoped by origin_kind + origin_note_id.
    let team_members = crate::team::list_team_members_raw(tx).unwrap_or_else(|e| {
        eprintln!("[index] list_team_members_raw failed: {e}");
        Vec::new()
    });
    let resolver = crate::team::OwnerResolver::from_members(&team_members);

    tx.execute(
        "DELETE FROM actions \
          WHERE origin_kind = 'note' AND origin_note_id = ?1",
        params![note_id],
    )?;
    let now_ms = current_unix_ms();
    let mut post_actions: Vec<(String, bool, String, Option<String>)> = Vec::new();
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO actions \
                (id, origin_kind, origin_note_id, origin_line, text, done, \
                 created_ms, due_ms, assignee_id) \
             VALUES (?1, 'note', ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO NOTHING",
        )?;
        for a in &p.actions {
            let id = action_id(&p.bundle_id, &a.text);
            let assignee_id = a
                .owner_candidate
                .as_deref()
                .and_then(|c| resolver.resolve(c));
            stmt.execute(params![
                id,
                note_id,
                a.line as i64,
                a.text,
                a.done as i64,
                now_ms,
                a.due_ms,
                assignee_id.clone(),
            ])?;
            post_actions.push((id, a.done, a.text.clone(), assignee_id));
        }
    }

    // Live action events (#106). Diff the post-state against the
    // pre-state captured at the top of this function.
    for (id, done, text, assignee_id) in &post_actions {
        let was_present = prior_actions.contains_key(id);
        let was_done = prior_actions.get(id).copied().unwrap_or(false);
        let actor = assignee_id.as_deref().or(self_id.as_deref());
        let payload = serde_json::json!({
            "text": text,
            "note_id": note_id,
        });
        if !was_present {
            crate::events::emit(
                tx,
                now_ms,
                "action_created",
                actor,
                "action",
                id,
                &payload,
            )?;
        }
        if *done && !was_done {
            crate::events::emit(
                tx,
                now_ms,
                "action_completed",
                actor,
                "action",
                id,
                &payload,
            )?;
        }
    }

    // Open questions (#113): wholesale-replace scoped to unresolved
    // rows. Resolved rows live forever (until the note is deleted via
    // FK CASCADE) so the "Resolved" tab on the Open Questions page
    // can show history. The ON CONFLICT branch reopens a resolved row
    // when the user manually edits `[x]` back to `[?]` in markdown.
    tx.execute(
        "DELETE FROM note_open_questions \
           WHERE origin_note_id = ?1 AND resolved = 0",
        params![note_id],
    )?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO note_open_questions \
                (id, origin_note_id, origin_line, text, resolved, \
                 created_ms, asked_of_id) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6) \
             ON CONFLICT(id) DO UPDATE SET \
                origin_line   = excluded.origin_line, \
                resolved      = 0, \
                resolved_ms   = NULL, \
                resolved_note = NULL, \
                asked_of_id   = excluded.asked_of_id",
        )?;
        for q in &p.open_questions {
            let id = open_question_id(&p.bundle_id, &q.text);
            let asked_of_id = q
                .owner_candidate
                .as_deref()
                .and_then(|c| resolver.resolve(c));
            stmt.execute(params![
                id,
                note_id,
                q.line as i64,
                q.text,
                now_ms,
                asked_of_id,
            ])?;
        }
    }
    Ok(())
}

pub(crate) fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn remove_in_tx(tx: &Transaction<'_>, note_id: &str) -> Result<()> {
    // FK ON DELETE CASCADE handles `tags`/`actions`/`meeting_attendees`;
    // FTS is a virtual table so we delete its row explicitly.
    tx.execute(
        "DELETE FROM notes_fts WHERE note_id = ?1",
        params![note_id],
    )?;
    tx.execute("DELETE FROM notes WHERE id = ?1", params![note_id])?;
    Ok(())
}

fn load_tags_grouped(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare("SELECT note_id, tag FROM tags ORDER BY note_id, tag")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (p, t) = row?;
        out.entry(p).or_default().push(t);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        conn
    }

    fn write_bundle(notes_dir: &Path, id: &str, body: &str) -> PathBuf {
        let dir = notes_dir.join(id);
        fs::create_dir_all(&dir).unwrap();
        let note = dir.join(NOTE_FILENAME);
        fs::write(&note, body).unwrap();
        note
    }

    #[test]
    fn open_or_init_creates_schema() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("idx.db");
        let conn = open_or_init(&db).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // FTS table reachable.
        conn.query_row("SELECT count(*) FROM notes_fts", [], |r| r.get::<_, i64>(0))
            .unwrap();
    }

    #[test]
    fn migration_v1_to_latest_adds_columns() {
        // Simulate an old install: a DB at schema_version = 1.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 1, "fixture must start at v1");

        apply_migrations(&conn).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        // archived + favorite columns exist and default to 0. After
        // #112 the PK is `id` (= the legacy bundle_id) and body_md is
        // a column on the table.
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms, body_size) \
             VALUES ('abc', 'abc', 't', 1, 0)",
            [],
        )
        .unwrap();
        let (archived, favorite): (i64, i64) = conn
            .query_row(
                "SELECT archived, favorite FROM notes WHERE id='abc'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived, 0);
        assert_eq!(favorite, 0);
    }

    #[test]
    fn migration_v2_to_v3_adds_favorite_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        // Now at v2.
        apply_migrations(&conn).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms, body_size) \
             VALUES ('zzz', 'zzz', 't', 1, 0)",
            [],
        )
        .unwrap();
        let favorite: i64 = conn
            .query_row(
                "SELECT favorite FROM notes WHERE id='zzz'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(favorite, 0);
    }

    #[test]
    fn migration_v4_to_v5_adds_due_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        // Pre-v5 schema: `notes.note_path` is still the PK here. The
        // 026 migration renames it to `id` later in the chain.
        conn.execute(
            "INSERT INTO notes(note_path, bundle_id, title, modified_ms, body_size) \
             VALUES ('/x/dd/note.md', 'dd', 't', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO actions(id, note_path, line, text, done, created_ms) \
             VALUES ('dd:00000000', '/x/dd/note.md', 1, 'old', 0, 1)",
            [],
        )
        .unwrap();

        apply_migrations(&conn).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        // due_ms column exists, defaults to NULL on the pre-existing row.
        let due_ms: Option<i64> = conn
            .query_row(
                "SELECT due_ms FROM actions WHERE id='dd:00000000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(due_ms.is_none());

        // body_size = -1 sentinel applied to all notes so reconcile re-reads.
        // After #112 the PK is `id` (set to the legacy bundle_id) but the
        // sentinel value persists through the migration.
        let bs: i64 = conn
            .query_row(
                "SELECT body_size FROM notes WHERE id='dd'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bs, -1);
    }

    #[test]
    fn open_or_init_idempotent_on_existing_db() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("idx.db");
        let _ = open_or_init(&db).unwrap();
        // Reopen; should not fail or wipe.
        let conn = open_or_init(&db).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    // The pre-#112 disk-reading reconcile + path-based upsert tests
    // were removed when notes moved into SQLite. Their semantics live
    // on in the new `write_note_atomic_tx` / migration tests below.

    #[test]
    fn list_all_returns_newest_first() {
        let mut conn = fresh_conn();
        let mk = |id: &str, mtime: i64| Indexable {
            bundle_id: id.into(),
            title: id.into(),
            modified_ms: mtime,
            duration_ms: None,
            preview: String::new(),
            body_size: 0,
            actions: vec![],
            open_questions: vec![],
            body: String::new(),
        };
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "old", &mk("old", 100)).unwrap();
        upsert_in_tx(&tx, "mid", &mk("mid", 500)).unwrap();
        upsert_in_tx(&tx, "new", &mk("new", 900)).unwrap();
        tx.commit().unwrap();

        let items = list_all(&conn, NoteScope::Active).unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["new", "mid", "old"]);
    }

    #[test]
    fn list_actions_excludes_archived_note() {
        let mut conn = fresh_conn();
        let tx = conn.transaction().unwrap();
        let p = Indexable {
            bundle_id: "active".into(),
            title: "A".into(),
            modified_ms: 100,
            duration_ms: None,
            preview: String::new(),
            body_size: 0,
            actions: vec![ParsedAction {
                line: 3,
                text: "visible".into(),
                done: false,
                due_ms: None,
                owner_candidate: None,
            }],
            open_questions: vec![],
            body: "# A\n\n- [ ] visible\n".into(),
        };
        upsert_in_tx(&tx, "active", &p).unwrap();
        let p2 = Indexable {
            bundle_id: "arc".into(),
            title: "Z".into(),
            modified_ms: 100,
            duration_ms: None,
            preview: String::new(),
            body_size: 0,
            actions: vec![ParsedAction {
                line: 3,
                text: "hidden".into(),
                done: false,
                due_ms: None,
                owner_candidate: None,
            }],
            open_questions: vec![],
            body: "# Z\n\n- [ ] hidden\n".into(),
        };
        upsert_in_tx(&tx, "arc", &p2).unwrap();
        tx.execute("UPDATE notes SET archived = 1 WHERE id = 'arc'", []).unwrap();
        tx.commit().unwrap();

        let opens = list_actions(&conn, ActionScope::Open, None, None, None, None).unwrap();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].text, "visible");
    }

    // ----- events + edges backfill (#102) -----------------------------------

    /// Seed two team members: a self row (id 'tm_self', alias 'me@x.io')
    /// and a teammate ('tm_bob', alias 'bob@x.io'). Required setup for
    /// every backfill test below.
    fn seed_self_and_teammate(conn: &Connection) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES ('tm_self', 'Me', '', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES ('tm_bob', 'Bob', '', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES ('tm_self', 'email', 'me@x.io')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES ('tm_bob', 'email', 'bob@x.io')",
            [],
        )
        .unwrap();
    }

    fn seed_connector(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES (?1, 'email', 'test', 1, '{}', 0, 0)",
            rusqlite::params![id],
        )
        .unwrap();
    }

    fn seed_email(conn: &Connection, id: &str, from: &str, sent_at: i64) {
        seed_connector(conn, "mg:test");
        conn.execute(
            "INSERT INTO email_messages(\
                id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 't1', 'Sub', ?2, NULL, ?3, NULL, NULL, 0, 0, NULL, ?3)",
            rusqlite::params![id, from, sent_at],
        )
        .unwrap();
    }

    fn seed_event_with_attendee(conn: &Connection, id: &str, member_id: &str, start: i64) {
        seed_connector(conn, "mg:test");
        conn.execute(
            "INSERT INTO calendar_events(\
                id, connector_id, external_id, title, start_ms, end_ms, all_day, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 'Sync', ?2, ?2, 0, ?2)",
            rusqlite::params![id, start],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_attendees(event_id, email, team_member_id, is_self, is_organizer) \
             VALUES (?1, ?2, ?3, 0, 0)",
            rusqlite::params![id, format!("{member_id}@x.io"), member_id],
        )
        .unwrap();
    }

    fn seed_note_row(conn: &Connection, note_id: &str, modified: i64) {
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms, body_size) \
             VALUES (?1, ?1, 'Title', ?2, 0)",
            rusqlite::params![note_id, modified],
        )
        .unwrap();
    }

    fn seed_action(conn: &Connection, id: &str, note_id: &str, assignee: Option<&str>) {
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_note_id, origin_line, \
                text, done, created_ms, assignee_id\
             ) VALUES (?1, 'note', ?2, 1, 'task', 0, 100, ?3)",
            rusqlite::params![id, note_id, assignee],
        )
        .unwrap();
    }

    fn seed_workstream(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO workstreams(id, title, summary, status, last_activity_ms, created_ms, updated_ms) \
             VALUES (?1, 'W', 'S', 'active', 100, 100, 100)",
            rusqlite::params![id],
        )
        .unwrap();
    }

    fn seed_workstream_signal(conn: &Connection, ws_id: &str, kind: &str, item_id: &str) {
        conn.execute(
            "INSERT INTO workstream_signals(workstream_id, kind, item_id, added_ms) \
             VALUES (?1, ?2, ?3, 100)",
            rusqlite::params![ws_id, kind, item_id],
        )
        .unwrap();
    }

    fn seed_workstream_action(conn: &Connection, id: &str, ws_id: &str, assignee: Option<&str>) {
        // Post-#111: synth-origin rows live in the unified `actions`
        // table; the legacy `workstream_actions` table is dropped by
        // migration 025. Seed via origin_kind='synth' instead.
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_synth_kind, origin_synth_id, \
                workstream_id, text, done, created_ms, assignee_id\
             ) VALUES (?1, 'synth', 'email', 'src', ?2, 'task', 0, 100, ?3)",
            rusqlite::params![id, ws_id, assignee],
        )
        .unwrap();
    }

    #[test]
    fn events_and_edges_backfill_from_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();

        seed_self_and_teammate(&conn);
        // Three emails: self → email_sent; teammate → email_received w/ actor; external → email_received w/ NULL actor.
        seed_email(&conn, "mg:test::msg-1", "me@x.io", 1_000);
        seed_email(&conn, "mg:test::msg-2", "bob@x.io", 2_000);
        seed_email(&conn, "mg:test::msg-3", "external@y.io", 3_000);
        // One calendar event with one resolved attendee.
        seed_event_with_attendee(&conn, "mg:test::evt-1", "tm_bob", 4_000);
        // One note.
        seed_note_row(&conn, "x", 5_000);
        // One note-backed action with an assignee.
        seed_action(&conn, "a-1", "x", Some("tm_bob"));
        // One workstream + one signal + one workstream-action with assignee.
        seed_workstream(&conn, "ws_1");
        seed_workstream_signal(&conn, "ws_1", "email", "mg:test::msg-1");
        seed_workstream_action(&conn, "wsa_1", "ws_1", Some("tm_bob"));

        // Re-run apply_migrations to confirm the version gate is idempotent.
        // No rows added on the second pass.
        apply_migrations(&conn).unwrap();

        // events: 3 emails + 1 meeting + 1 note + 2 actions = 7 rows.
        let total_events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_events, 0, "no rows yet — backfill ran during apply_migrations *before* we seeded");

        // The migration ran during apply_migrations() but BEFORE we seeded
        // the source rows. To verify the backfill SQL, simulate a fresh
        // upgrade: drop + recreate the tables and re-run just the
        // backfill block. Easier: re-run the migration's INSERTs manually.
        rerun_backfill_inserts(&conn);

        let total_events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_events, 7, "3 emails + 1 meeting + 1 note + 2 actions");

        let sent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'email_sent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sent, 1, "only the from=self email is 'email_sent'");

        let received: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'email_received'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(received, 2, "teammate + external both classify as received");

        let null_actor: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'email_received' AND actor_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_actor, 1, "external sender has no team_member → actor_id NULL");

        let bob_received: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'email_received' AND actor_id = 'tm_bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_received, 1);

        // edges
        let includes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE edge_kind = 'INCLUDES'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(includes, 1);

        let attended: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE edge_kind = 'ATTENDED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attended, 1, "1 resolved attendee");

        let owns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE edge_kind = 'OWNS'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owns, 2, "1 note action + 1 workstream action, both with assignees");
    }

    /// Mirrors the INSERT statements at the bottom of 022_events_edges.sql.
    /// Used by tests that seed data *after* the migration ran.
    fn rerun_backfill_inserts(conn: &Connection) {
        conn.execute_batch(
            r#"
            INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
            SELECT
              e.sent_at_ms,
              CASE WHEN EXISTS (
                SELECT 1 FROM team_member_aliases a
                JOIN team_members m ON m.id = a.member_id
                WHERE a.kind = 'email'
                  AND lower(a.value) = lower(e.from_email)
                  AND m.is_self = 1
              ) THEN 'email_sent' ELSE 'email_received' END,
              (SELECT a.member_id FROM team_member_aliases a
                WHERE a.kind = 'email' AND lower(a.value) = lower(e.from_email)
                LIMIT 1),
              'email', e.id,
              json_object('thread_id', e.thread_id, 'subject', e.subject),
              e.sent_at_ms
            FROM email_messages e;

            INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
            SELECT
              c.start_ms, 'meeting',
              (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1),
              'event', c.id,
              json_object('title', c.title, 'all_day', c.all_day),
              c.start_ms
            FROM calendar_events c;

            INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
            SELECT
              n.modified_ms, 'note_modified',
              (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1),
              'note', n.id,
              json_object('title', n.title, 'bundle_id', n.bundle_id),
              n.modified_ms
            FROM notes n;

            -- Unified action_created backfill (#111/#112): one INSERT
            -- for both note- and synth-origin rows. Payload carries
            -- whichever origin field is populated.
            INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
            SELECT
              a.created_ms, 'action_created',
              COALESCE(a.assignee_id, (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1)),
              'action', a.id,
              json_object(
                'text', a.text,
                'note_id', a.origin_note_id,
                'workstream_id', a.workstream_id
              ),
              a.created_ms
            FROM actions a;

            INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
            SELECT 'workstream', s.workstream_id, s.kind, s.item_id, 'INCLUDES',
                   s.added_ms, s.added_ms
            FROM workstream_signals s
            WHERE s.manual_detached_ms IS NULL
            ;

            INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
            SELECT 'person', ca.team_member_id, 'event', ca.event_id, 'ATTENDED',
                   ce.start_ms, ce.start_ms
            FROM calendar_attendees ca
            JOIN calendar_events ce ON ce.id = ca.event_id
            WHERE ca.team_member_id IS NOT NULL
            ;

            INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
            SELECT 'person', a.assignee_id, 'action', a.id, 'OWNS',
                   a.created_ms, a.created_ms
            FROM actions a
            WHERE a.assignee_id IS NOT NULL
            ;
            "#,
        )
        .unwrap();
    }

    #[test]
    fn empty_db_apply_migrations_creates_events_and_edges() {
        // Smoke test: fresh DB applies all migrations including 022;
        // both new tables exist and are empty (no source data to backfill).
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 0);
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 0);
    }

    // ----- #106 live event emission ----------------------------------------

    fn mk_indexable(title: &str, body: &str, actions: Vec<ParsedAction>, modified_ms: i64) -> Indexable {
        Indexable {
            bundle_id: "b1".into(),
            title: title.into(),
            modified_ms,
            duration_ms: None,
            preview: String::new(),
            body_size: body.len() as i64,
            actions,
            open_questions: vec![],
            body: body.into(),
        }
    }

    #[test]
    fn upsert_in_tx_emits_note_created_then_note_modified() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        // Self for actor_id is not required (column is nullable).

        let p1 = mk_indexable("First", "hello", vec![], 100);
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p1).unwrap();
        tx.commit().unwrap();
        let created: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'note_created'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, 1);

        // Re-upsert (simulates a save) → note_modified.
        let p2 = mk_indexable("First v2", "hello again", vec![], 200);
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p2).unwrap();
        tx.commit().unwrap();
        let modified: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'note_modified'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(modified, 1);
    }

    #[test]
    fn action_completed_fires_on_done_flip() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();

        // Insert with an open action.
        let p1 = mk_indexable(
            "T",
            "- [ ] task",
            vec![ParsedAction {
                line: 1,
                text: "task".into(),
                done: false,
                due_ms: None,
                owner_candidate: None,
            }],
            100,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p1).unwrap();
        tx.commit().unwrap();
        let created: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_created'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, 1);
        let completed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(completed, 0);

        // Re-upsert with done=true → action_completed event.
        let p2 = mk_indexable(
            "T",
            "- [x] task",
            vec![ParsedAction {
                line: 1,
                text: "task".into(),
                done: true,
                due_ms: None,
                owner_candidate: None,
            }],
            200,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p2).unwrap();
        tx.commit().unwrap();
        let completed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'action_completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(completed, 1);
    }

    // ----- #111 unified actions table -----------------------------------

    #[test]
    fn unify_actions_migration_workstream_actions_dropped() {
        // After 025 the legacy workstream_actions table is gone — but
        // any rows it held survive in the unified `actions` table with
        // origin_kind='synth'.
        let conn = fresh_conn();
        seed_workstream(&conn, "ws_x");
        seed_workstream_action(&conn, "wsa_1", "ws_x", None);
        let synth_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions WHERE origin_kind = 'synth'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(synth_rows, 1);

        let table_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                  WHERE type = 'table' AND name = 'workstream_actions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 0, "workstream_actions table must be dropped");
    }

    #[test]
    fn list_actions_filters_by_workstream() {
        let conn = fresh_conn();
        seed_workstream(&conn, "ws_a");
        seed_workstream(&conn, "ws_b");
        seed_workstream_action(&conn, "wsa_a", "ws_a", None);
        seed_workstream_action(&conn, "wsa_b", "ws_b", None);
        // A floating note-origin row pinned to ws_a.
        seed_note_row(&conn, "n", 100);
        conn.execute(
            "INSERT INTO actions \
                (id, origin_kind, origin_note_id, origin_line, text, done, \
                 created_ms, workstream_id) \
             VALUES ('n:1', 'note', 'n', 1, 'task', 0, 100, 'ws_a')",
            [],
        )
        .unwrap();

        let only_a = list_actions(&conn, ActionScope::All, None, Some("ws_a"), None, None)
            .unwrap();
        let ids_a: Vec<&str> = only_a.iter().map(|r| r.id.as_str()).collect();
        assert!(ids_a.contains(&"wsa_a"));
        assert!(ids_a.contains(&"n:1"));
        assert!(!ids_a.contains(&"wsa_b"));

        let only_b = list_actions(&conn, ActionScope::All, None, Some("ws_b"), None, None)
            .unwrap();
        let ids_b: Vec<&str> = only_b.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids_b, vec!["wsa_b"]);
    }

    #[test]
    fn upsert_in_tx_preserves_synth_rows_on_other_origins() {
        // Re-running upsert_in_tx on a note must only blow away that
        // note's own note-origin rows. Synth rows attached to other
        // workstreams survive untouched (#111).
        let mut conn = fresh_conn();
        seed_workstream(&conn, "ws_x");
        seed_workstream_action(&conn, "wsa_keep", "ws_x", None);

        let p = mk_indexable(
            "T",
            "- [ ] task",
            vec![ParsedAction {
                line: 1,
                text: "task".into(),
                done: false,
                due_ms: None,
                owner_candidate: None,
            }],
            100,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p).unwrap();
        tx.commit().unwrap();

        let synth_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions \
                  WHERE origin_kind = 'synth' AND id = 'wsa_keep'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(synth_count, 1, "synth row must survive note reindex");
        let note_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions \
                  WHERE origin_kind = 'note' AND origin_note_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(note_count, 1);
    }

    #[test]
    fn list_actions_for_note_returns_all_origins_regardless_of_done() {
        // The sidebar (#145) shows every action tied to the current
        // note: both origins (reconcile + note), both done states.
        // Synth rows on OTHER notes must not bleed in.
        let conn = fresh_conn();
        seed_note_row(&conn, "n_a", 100);
        seed_note_row(&conn, "n_b", 100);

        // Reconcile-origin row on n_a, done.
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_note_id, origin_line, \
                text, done, created_ms\
             ) VALUES ('r:1', 'reconcile', 'n_a', NULL, 'recon done', 1, 200)",
            [],
        )
        .unwrap();
        // Note-origin row on n_a, open.
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_note_id, origin_line, \
                text, done, created_ms\
             ) VALUES ('n_a:1', 'note', 'n_a', 1, 'hand task', 0, 100)",
            [],
        )
        .unwrap();
        // Note-origin row on a DIFFERENT note — must be excluded.
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_note_id, origin_line, \
                text, done, created_ms\
             ) VALUES ('n_b:1', 'note', 'n_b', 1, 'unrelated', 0, 100)",
            [],
        )
        .unwrap();

        let got = list_actions_for_note(&conn, "n_a").unwrap();
        let ids: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "expected 2 rows for n_a, got {ids:?}");
        assert!(ids.contains(&"r:1"));
        assert!(ids.contains(&"n_a:1"));
        // created_ms DESC → reconcile row (200) before note row (100).
        assert_eq!(ids[0], "r:1");
        assert_eq!(ids[1], "n_a:1");

        // Empty-note case returns []
        let empty = list_actions_for_note(&conn, "/path/none").unwrap();
        assert!(empty.is_empty());
    }

    // ----- #113 open questions integration ----------------------------------

    fn mk_indexable_with_questions(
        bundle_id: &str,
        body: &str,
        questions: Vec<ParsedQuestion>,
        modified_ms: i64,
    ) -> Indexable {
        Indexable {
            bundle_id: bundle_id.into(),
            title: bundle_id.into(),
            modified_ms,
            duration_ms: None,
            preview: String::new(),
            body_size: body.len() as i64,
            actions: vec![],
            open_questions: questions,
            body: body.into(),
        }
    }

    #[test]
    fn upsert_replaces_open_questions_but_keeps_resolved() {
        let mut conn = fresh_conn();
        let p = mk_indexable_with_questions(
            "a",
            "- [?] alpha\n- [?] beta\n",
            vec![
                ParsedQuestion {
                    line: 1,
                    text: "alpha".into(),
                    owner_candidate: None,
                },
                ParsedQuestion {
                    line: 2,
                    text: "beta".into(),
                    owner_candidate: None,
                },
            ],
            100,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p).unwrap();
        tx.commit().unwrap();

        // Mark `alpha` resolved out-of-band (simulating the resolve IPC).
        conn.execute(
            "UPDATE note_open_questions SET resolved = 1, resolved_ms = 200 \
               WHERE id = (SELECT id FROM note_open_questions \
                            WHERE origin_note_id = 'a' AND text = 'alpha')",
            [],
        )
        .unwrap();

        // Re-upsert with a body that drops both [?] lines.
        let p2 = mk_indexable_with_questions(
            "a",
            "- [x] alpha\n",
            vec![],
            200,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p2).unwrap();
        tx.commit().unwrap();

        // Resolved row survives; open row (beta) is gone.
        let resolved_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_open_questions \
                  WHERE origin_note_id = 'a' AND resolved = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved_n, 1);
        let open_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_open_questions \
                  WHERE origin_note_id = 'a' AND resolved = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open_n, 0);
    }

    #[test]
    fn upsert_reopens_resolved_row_when_marker_flips_back() {
        let mut conn = fresh_conn();
        // Initial save with one question.
        let p = mk_indexable_with_questions(
            "a",
            "- [?] foo\n",
            vec![ParsedQuestion {
                line: 1,
                text: "foo".into(),
                owner_candidate: None,
            }],
            100,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p).unwrap();
        tx.commit().unwrap();
        // Mark it resolved.
        conn.execute(
            "UPDATE note_open_questions SET resolved = 1, \
                                              resolved_ms = 150, \
                                              resolved_note = 'yes' \
              WHERE origin_note_id = 'a'",
            [],
        )
        .unwrap();
        // User manually edits `[x]` back to `[?]` in the body — the
        // parser surfaces the question again. ON CONFLICT branch
        // reopens the row.
        let p2 = mk_indexable_with_questions(
            "a",
            "- [?] foo\n",
            vec![ParsedQuestion {
                line: 1,
                text: "foo".into(),
                owner_candidate: None,
            }],
            200,
        );
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "a", &p2).unwrap();
        tx.commit().unwrap();

        let (resolved, resolved_ms, resolved_note): (i64, Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT resolved, resolved_ms, resolved_note \
                   FROM note_open_questions WHERE origin_note_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(resolved, 0);
        assert_eq!(resolved_ms, None);
        assert_eq!(resolved_note, None);
    }
}
