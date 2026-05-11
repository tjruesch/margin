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
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection, OptionalExtension, Result, Transaction};
use serde::Serialize;

use crate::notes::{
    action_id, bundle_dir_for_in, extract_preview, parse_actions, parse_frontmatter,
    read_archived, read_favorite, read_tags, split_frontmatter, ActionListItem, ActionScope,
    NoteListItem, NoteScope, ParsedAction, NOTE_FILENAME, TRANSCRIPT_FILENAME,
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
const SCHEMA_VERSION: i64 = 22;

/// Open the index DB at `db_path` (creating it if absent) and apply any
/// pending migrations.
pub fn open_or_init(db_path: &Path) -> Result<Connection> {
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
    if version != SCHEMA_VERSION {
        // Future: bump SCHEMA_VERSION and add another step above.
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

/// Re-read `note_path` from disk and refresh its row + tag rows + FTS row.
pub fn upsert(conn: &mut Connection, note_path: &Path) -> Result<()> {
    upsert_in(conn, note_path, &paths::notes_dir())
}

fn upsert_in(conn: &mut Connection, note_path: &Path, notes_dir: &Path) -> Result<()> {
    let parsed = match read_indexable(note_path, notes_dir) {
        Some(p) => p,
        None => return Ok(()), // missing or not an owned note — nothing to index
    };
    let path_str = note_path.to_string_lossy().into_owned();
    let tx = conn.transaction()?;
    upsert_in_tx(&tx, &path_str, &parsed)?;
    tx.commit()
}

/// Drop a note (and its tags + FTS rows) from the index. No-op if absent.
pub fn remove(conn: &mut Connection, note_path: &Path) -> Result<()> {
    let path_str = note_path.to_string_lossy().into_owned();
    let tx = conn.transaction()?;
    remove_in_tx(&tx, &path_str)?;
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
        "SELECT n.note_path, n.title, n.modified_ms, n.duration_ms, n.preview, n.favorite \
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

/// Action items unified across two sources (#100):
///  - `actions` (note-backed markdown checkboxes on non-archived notes)
///  - `workstream_actions` on active workstreams
///
/// Each row carries a `source` discriminator so the UI can route click-
/// through, delete, and assignee writes through the right IPC path.
/// Archived notes / non-active workstreams are excluded from `Open`
/// view since their actions are out of sight.
pub fn list_actions(
    conn: &Connection,
    scope: ActionScope,
    assignee_id: Option<&str>,
) -> Result<Vec<ActionListItem>> {
    let where_done_note = match scope {
        ActionScope::Open => "AND a.done = 0",
        ActionScope::Done => "AND a.done = 1",
        ActionScope::All => "",
    };
    let where_done_ws = match scope {
        ActionScope::Open => "AND wa.done = 0",
        ActionScope::Done => "AND wa.done = 1",
        ActionScope::All => "",
    };
    // Always bind ?1 (assignee_id, NULL when no filter); the SQL
    // `(?1 IS NULL OR <col> = ?1)` short-circuits to "no filter" when
    // ?1 is NULL. Avoids the lifetime gymnastics of building a dynamic
    // params vec.
    let sql = format!(
        "SELECT * FROM ( \
            SELECT 'note' AS source, a.id AS id, a.note_path AS note_path, n.title AS title, \
                   NULL AS workstream_id, a.text AS text, a.done AS done, a.line AS line, \
                   a.created_ms AS created_ms, a.due_ms AS due_ms, a.assignee_id AS assignee_id, \
                   t.display_name AS display_name, n.modified_ms AS order_ms \
              FROM actions a \
              JOIN notes n ON n.note_path = a.note_path \
              LEFT JOIN team_members t ON t.id = a.assignee_id \
             WHERE n.archived = 0 {where_done_note} \
               AND (?1 IS NULL OR a.assignee_id = ?1) \
            UNION ALL \
            SELECT 'workstream' AS source, wa.id AS id, '' AS note_path, w.title AS title, \
                   wa.workstream_id AS workstream_id, wa.text AS text, wa.done AS done, \
                   0 AS line, wa.created_ms AS created_ms, wa.due_ms AS due_ms, \
                   wa.assignee_id AS assignee_id, t.display_name AS display_name, \
                   w.last_activity_ms AS order_ms \
              FROM workstream_actions wa \
              JOIN workstreams w ON w.id = wa.workstream_id \
              LEFT JOIN team_members t ON t.id = wa.assignee_id \
             WHERE w.status = 'active' {where_done_ws} \
               AND (?1 IS NULL OR wa.assignee_id = ?1) \
         ) \
         ORDER BY (due_ms IS NULL), due_ms ASC, order_ms DESC, line ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![assignee_id], |r| {
        Ok(ActionListItem {
            source: r.get(0)?,
            id: r.get(1)?,
            note_path: r.get(2)?,
            note_title: r.get(3)?,
            workstream_id: r.get(4)?,
            text: r.get(5)?,
            done: r.get::<_, i64>(6)? != 0,
            line: r.get(7)?,
            created_ms: r.get(8)?,
            due_ms: r.get(9)?,
            assignee_id: r.get(10)?,
            assignee_display_name: r.get(11)?,
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
        SELECT n.note_path, n.bundle_id, n.title, n.modified_ms, \
               snippet(notes_fts, 2, ?2, ?3, '…', 12) AS body_snip, \
               bm25(notes_fts) AS score \
        FROM notes_fts \
        JOIN notes n ON n.note_path = notes_fts.note_path \
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
                .prepare("SELECT note_path FROM notes WHERE archived = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = std::collections::HashSet::new();
            for r in rows {
                set.insert(r?);
            }
            set
        };
        let titles_by_path: HashMap<String, (String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT note_path, bundle_id, title, modified_ms FROM notes \
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
        "SELECT note_path, bundle_id, title, modified_ms, preview \
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
        SELECT n.note_path, n.bundle_id, n.title, n.modified_ms, \
               snippet(notes_fts, 2, ?2, ?3, '…', 16) AS body_snip, \
               bm25(notes_fts) AS score \
        FROM notes_fts \
        JOIN notes n ON n.note_path = notes_fts.note_path \
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
                .prepare("SELECT note_path FROM notes WHERE archived = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = std::collections::HashSet::new();
            for r in rows {
                set.insert(r?);
            }
            set
        };
        let titles_by_path: HashMap<String, (String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT note_path, bundle_id, title, modified_ms FROM notes \
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
        "SELECT note_path, bundle_id, title, modified_ms, preview \
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

#[derive(Default)]
pub struct ReconcileReport {
    pub upserted: usize,
    pub removed: usize,
    pub skipped: usize,
}

/// Walk `notes_dir`, compute the diff against the index, and apply only
/// the necessary changes. Cheap-checks first via `(count, max(mtime))`.
pub fn reconcile(conn: &mut Connection, notes_dir: &Path) -> Result<ReconcileReport> {
    let disk = scan_disk(notes_dir);
    let (db_count, db_max_mtime): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(modified_ms), 0) FROM notes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    // Migrations set `body_size = -1` on rows that need a forced re-read
    // (e.g. when a parser change means the cached `text` is stale). Skip
    // the global count+max-mtime shortcut whenever any such sentinel
    // exists, otherwise the migration's intent gets bypassed and the new
    // parser never runs against unchanged files.
    let pending_resync: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE body_size < 0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let disk_max_mtime = disk.iter().map(|d| d.modified_ms).max().unwrap_or(0);
    if pending_resync == 0
        && db_count as usize == disk.len()
        && db_max_mtime == disk_max_mtime
    {
        return Ok(ReconcileReport {
            skipped: disk.len(),
            ..Default::default()
        });
    }

    // Index existing rows by path for diff.
    let mut existing: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT note_path, modified_ms, body_size FROM notes")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (p, m, s) = row?;
            existing.insert(p, (m, s));
        }
    }

    let mut report = ReconcileReport::default();
    let tx = conn.transaction()?;

    let disk_paths: Vec<String> = disk
        .iter()
        .map(|d| d.note_path.to_string_lossy().into_owned())
        .collect();
    let disk_set: std::collections::HashSet<&str> =
        disk_paths.iter().map(|s| s.as_str()).collect();

    for (path, (_, _)) in existing.iter() {
        if !disk_set.contains(path.as_str()) {
            remove_in_tx(&tx, path)?;
            report.removed += 1;
        }
    }

    for (i, entry) in disk.iter().enumerate() {
        let path_str = &disk_paths[i];
        let needs_upsert = match existing.get(path_str) {
            None => true,
            Some((m, s)) => *m != entry.modified_ms || *s != entry.body_size,
        };
        if !needs_upsert {
            report.skipped += 1;
            continue;
        }
        let parsed = match read_indexable(&entry.note_path, notes_dir) {
            Some(p) => p,
            None => continue,
        };
        upsert_in_tx(&tx, path_str, &parsed)?;
        report.upserted += 1;
    }

    tx.commit()?;
    Ok(report)
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

struct DiskEntry {
    note_path: PathBuf,
    modified_ms: i64,
    body_size: i64,
}

struct Indexable {
    bundle_id: String,
    title: String,
    modified_ms: i64,
    duration_ms: Option<u64>,
    preview: String,
    body_size: i64,
    archived: bool,
    favorite: bool,
    tags: Vec<String>,
    actions: Vec<ParsedAction>,
    body: String,
}

fn scan_disk(notes_dir: &Path) -> Vec<DiskEntry> {
    let mut out = Vec::new();
    let read_dir = match fs::read_dir(notes_dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read_dir.flatten() {
        let bundle = entry.path();
        if !bundle.is_dir() {
            continue;
        }
        let note_path = bundle.join(NOTE_FILENAME);
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
        let body_size = meta.len() as i64;
        out.push(DiskEntry {
            note_path,
            modified_ms,
            body_size,
        });
    }
    out
}

fn read_indexable(note_path: &Path, notes_dir: &Path) -> Option<Indexable> {
    let bundle_dir = bundle_dir_for_in(note_path, notes_dir)?;
    let bundle_id = bundle_dir.file_name()?.to_string_lossy().into_owned();
    let meta = fs::metadata(note_path).ok()?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let body_size = meta.len() as i64;

    let raw = fs::read_to_string(note_path).ok()?;
    let (yaml, body) = split_frontmatter(&raw);
    let frontmatter = yaml.map(parse_frontmatter).unwrap_or_default();
    let tags = read_tags(&frontmatter);
    let archived = read_archived(&frontmatter);
    let favorite = read_favorite(&frontmatter);
    let actions = parse_actions(body);
    let title = body
        .lines()
        .find_map(|l| {
            l.trim_start()
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

    let preview = extract_preview(body);

    Some(Indexable {
        bundle_id,
        title,
        modified_ms,
        duration_ms,
        preview,
        body_size,
        archived,
        favorite,
        tags,
        actions,
        body: body.to_string(),
    })
}

fn upsert_in_tx(tx: &Transaction<'_>, note_path: &str, p: &Indexable) -> Result<()> {
    tx.execute(
        "INSERT INTO notes(note_path, bundle_id, title, modified_ms, duration_ms, preview, body_size, archived, favorite) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(note_path) DO UPDATE SET \
            bundle_id = excluded.bundle_id, \
            title = excluded.title, \
            modified_ms = excluded.modified_ms, \
            duration_ms = excluded.duration_ms, \
            preview = excluded.preview, \
            body_size = excluded.body_size, \
            archived = excluded.archived, \
            favorite = excluded.favorite",
        params![
            note_path,
            p.bundle_id,
            p.title,
            p.modified_ms,
            p.duration_ms.map(|v| v as i64),
            p.preview,
            p.body_size,
            p.archived as i64,
            p.favorite as i64,
        ],
    )?;

    tx.execute("DELETE FROM tags WHERE note_path = ?1", params![note_path])?;
    {
        let mut stmt =
            tx.prepare_cached("INSERT INTO tags(note_path, tag) VALUES (?1, ?2)")?;
        for tag in &p.tags {
            stmt.execute(params![note_path, tag])?;
        }
    }

    tx.execute(
        "DELETE FROM notes_fts WHERE note_path = ?1",
        params![note_path],
    )?;
    tx.execute(
        "INSERT INTO notes_fts(note_path, title, body) VALUES (?1, ?2, ?3)",
        params![note_path, p.title, p.body],
    )?;

    // Actions: replace wholesale. Two open checkboxes with identical
    // text in one note collapse to one row via the PRIMARY KEY (id is
    // <bundle>:<hash(text)>). Documented as the v1 trade-off.
    //
    // Owner resolution (#49) runs in this same pass: build the
    // `OwnerResolver` once from the current team_members snapshot, then
    // resolve each action's `owner_candidate` to a member id when
    // unambiguous. Ambiguous and unmatched candidates leave assignee_id
    // NULL.
    let team_members = crate::team::list_team_members_raw(tx).unwrap_or_else(|e| {
        eprintln!("[index] list_team_members_raw failed: {e}");
        Vec::new()
    });
    let resolver = crate::team::OwnerResolver::from_members(&team_members);

    tx.execute("DELETE FROM actions WHERE note_path = ?1", params![note_path])?;
    {
        let now_ms = current_unix_ms();
        let mut stmt = tx.prepare_cached(
            "INSERT INTO actions(id, note_path, line, text, done, created_ms, due_ms, assignee_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(id) DO NOTHING",
        )?;
        for a in &p.actions {
            let id = action_id(&p.bundle_id, &a.text);
            let assignee_id = a
                .owner_candidate
                .as_deref()
                .and_then(|c| resolver.resolve(c));
            stmt.execute(params![
                id,
                note_path,
                a.line as i64,
                a.text,
                a.done as i64,
                now_ms,
                a.due_ms,
                assignee_id,
            ])?;
        }
    }
    Ok(())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn remove_in_tx(tx: &Transaction<'_>, note_path: &str) -> Result<()> {
    // FK ON DELETE CASCADE handles `tags`; FTS is a virtual table so we
    // delete its row explicitly.
    tx.execute(
        "DELETE FROM notes_fts WHERE note_path = ?1",
        params![note_path],
    )?;
    tx.execute("DELETE FROM notes WHERE note_path = ?1", params![note_path])?;
    Ok(())
}

fn load_tags_grouped(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare("SELECT note_path, tag FROM tags ORDER BY note_path, tag")?;
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

        // archived + favorite columns exist and default to 0.
        conn.execute(
            "INSERT INTO notes(note_path, bundle_id, title, modified_ms, body_size) \
             VALUES ('/x/abc/note.md', 'abc', 't', 1, 0)",
            [],
        )
        .unwrap();
        let (archived, favorite): (i64, i64) = conn
            .query_row(
                "SELECT archived, favorite FROM notes WHERE note_path='/x/abc/note.md'",
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
            "INSERT INTO notes(note_path, bundle_id, title, modified_ms, body_size) \
             VALUES ('/x/zzz/note.md', 'zzz', 't', 1, 0)",
            [],
        )
        .unwrap();
        let favorite: i64 = conn
            .query_row(
                "SELECT favorite FROM notes WHERE note_path='/x/zzz/note.md'",
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
        // Insert a pre-v5 actions row to confirm ALTERs don't disturb it.
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
        let bs: i64 = conn
            .query_row(
                "SELECT body_size FROM notes WHERE note_path='/x/dd/note.md'",
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

    #[test]
    fn upsert_indexes_a_note() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(
            &notes,
            "abc",
            "---\ntags:\n  - work\n  - urgent\n---\n# Hello\n\nSome body text.\n",
        );
        let mut conn = fresh_conn();
        upsert_in(&mut conn, &note, &notes).unwrap();

        let items = list_all(&conn, NoteScope::Active).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.title, "Hello");
        assert_eq!(item.preview, "Some body text.");
        let mut got = item.tags.clone();
        got.sort();
        assert_eq!(got, vec!["urgent".to_string(), "work".to_string()]);

        let fts_count: i64 = conn
            .query_row("SELECT count(*) FROM notes_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1);
    }

    #[test]
    fn reconcile_indexes_fresh_disk() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        write_bundle(&notes, "aaa", "# A\n\nFirst note.\n");
        write_bundle(&notes, "bbb", "---\ntags: [todo]\n---\n# B\n\nSecond.\n");
        let mut conn = fresh_conn();
        let report = reconcile(&mut conn, &notes).unwrap();
        assert_eq!(report.upserted, 2);
        assert_eq!(report.removed, 0);
        let items = list_all(&conn, NoteScope::Active).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn reconcile_noop_when_consistent() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        write_bundle(&notes, "aaa", "# A\n\nbody\n");
        let mut conn = fresh_conn();
        reconcile(&mut conn, &notes).unwrap();
        let report = reconcile(&mut conn, &notes).unwrap();
        assert_eq!(report.upserted, 0);
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn reconcile_removes_orphans() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(&notes, "aaa", "# A\n\nbody\n");
        let mut conn = fresh_conn();
        reconcile(&mut conn, &notes).unwrap();
        assert_eq!(list_all(&conn, NoteScope::Active).unwrap().len(), 1);

        // Remove the bundle directory and reconcile.
        fs::remove_dir_all(note.parent().unwrap()).unwrap();
        let report = reconcile(&mut conn, &notes).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(list_all(&conn, NoteScope::Active).unwrap().len(), 0);
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut conn = fresh_conn();
        let path = "/fake/notes/xyz/note.md".to_string();
        let mut p = Indexable {
            bundle_id: "xyz".into(),
            title: "First".into(),
            modified_ms: 1,
            duration_ms: None,
            preview: "v1".into(),
            body_size: 1,
            archived: false,
            favorite: false,
            actions: vec![],
            tags: vec!["a".into()],
            body: "v1".into(),
        };
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, &path, &p).unwrap();
        tx.commit().unwrap();

        p.title = "Second".into();
        p.tags = vec!["b".into(), "c".into()];
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, &path, &p).unwrap();
        tx.commit().unwrap();

        let items = list_all(&conn, NoteScope::Active).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Second");
        assert_eq!(items[0].tags, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn remove_deletes_cascade() {
        let mut conn = fresh_conn();
        let path = "/fake/notes/xyz/note.md".to_string();
        let p = Indexable {
            bundle_id: "xyz".into(),
            title: "T".into(),
            modified_ms: 1,
            duration_ms: None,
            preview: "p".into(),
            body_size: 1,
            archived: false,
            favorite: false,
            actions: vec![],
            tags: vec!["a".into(), "b".into()],
            body: "body".into(),
        };
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, &path, &p).unwrap();
        tx.commit().unwrap();

        let tx = conn.transaction().unwrap();
        remove_in_tx(&tx, &path).unwrap();
        tx.commit().unwrap();

        let n: i64 = conn
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        let t: i64 = conn
            .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(t, 0);
        let f: i64 = conn
            .query_row("SELECT count(*) FROM notes_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(f, 0);
    }

    #[test]
    fn list_all_filters_by_scope() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        write_bundle(&notes, "act1", "# A\n\nactive one\n");
        write_bundle(
            &notes,
            "arc1",
            "---\narchived: true\n---\n# Z\n\narchived one\n",
        );
        write_bundle(&notes, "act2", "# B\n\nanother active\n");
        let mut conn = fresh_conn();
        reconcile(&mut conn, &notes).unwrap();

        let active = list_all(&conn, NoteScope::Active).unwrap();
        let archived = list_all(&conn, NoteScope::Archived).unwrap();
        let all = list_all(&conn, NoteScope::All).unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].title, "Z");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn upsert_indexes_favorite_flag() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(
            &notes,
            "abc",
            "---\nfavorite: true\n---\n# Hi\n\nbody\n",
        );
        let mut conn = fresh_conn();
        upsert_in(&mut conn, &note, &notes).unwrap();
        let favorite: i64 = conn
            .query_row(
                "SELECT favorite FROM notes WHERE bundle_id='abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(favorite, 1);
        let items = list_all(&conn, NoteScope::Favorites).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].favorite);
    }

    #[test]
    fn list_all_filters_by_favorites_scope() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        write_bundle(&notes, "plain", "# Plain\n");
        write_bundle(
            &notes,
            "fav1",
            "---\nfavorite: true\n---\n# Fav One\n",
        );
        write_bundle(
            &notes,
            "fav-arc",
            "---\nfavorite: true\narchived: true\n---\n# Hidden\n",
        );
        let mut conn = fresh_conn();
        reconcile(&mut conn, &notes).unwrap();

        let active = list_all(&conn, NoteScope::Active).unwrap();
        let favorites = list_all(&conn, NoteScope::Favorites).unwrap();
        let archived = list_all(&conn, NoteScope::Archived).unwrap();
        assert_eq!(active.len(), 2, "plain + fav1 (fav-arc archived out)");
        assert_eq!(favorites.len(), 1, "fav1 only — archived favorites hidden");
        assert_eq!(favorites[0].title, "Fav One");
        assert_eq!(archived.len(), 1);
    }

    #[test]
    fn upsert_indexes_actions() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(
            &notes,
            "actbundle",
            "# Plan\n\n- [ ] open one\n- [x] done one\n",
        );
        let mut conn = fresh_conn();
        upsert_in(&mut conn, &note, &notes).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let opens: Vec<ActionListItem> = list_actions(&conn, ActionScope::Open, None).unwrap();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].text, "open one");
        let done: Vec<ActionListItem> = list_actions(&conn, ActionScope::Done, None).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].text, "done one");
    }

    #[test]
    fn upsert_indexes_actions_with_due_ms() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(
            &notes,
            "due-bundle",
            "# Plan\n\n- [ ] Pay invoice @2026-06-01\n- [ ] No date here\n",
        );
        let mut conn = fresh_conn();
        upsert_in(&mut conn, &note, &notes).unwrap();
        let opens = list_actions(&conn, ActionScope::Open, None).unwrap();
        assert_eq!(opens.len(), 2);
        // Sort: dated row leads (ORDER BY due_ms IS NULL), then by due_ms ASC.
        assert_eq!(opens[0].text, "Pay invoice");
        assert!(opens[0].due_ms.is_some());
        assert_eq!(opens[1].text, "No date here");
        assert!(opens[1].due_ms.is_none());
    }

    #[test]
    fn upsert_replaces_actions_on_rewrite() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(&notes, "rewrite", "# T\n\n- [ ] alpha\n");
        let mut conn = fresh_conn();
        upsert_in(&mut conn, &note, &notes).unwrap();
        // Rewrite with a different action text.
        std::fs::write(&note, "# T\n\n- [ ] beta\n").unwrap();
        upsert_in(&mut conn, &note, &notes).unwrap();
        let opens = list_actions(&conn, ActionScope::Open, None).unwrap();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].text, "beta");
    }

    #[test]
    fn list_actions_excludes_archived_note() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        write_bundle(&notes, "active", "# A\n\n- [ ] visible\n");
        write_bundle(
            &notes,
            "arc",
            "---\narchived: true\n---\n# Z\n\n- [ ] hidden\n",
        );
        let mut conn = fresh_conn();
        reconcile(&mut conn, &notes).unwrap();
        let opens = list_actions(&conn, ActionScope::Open, None).unwrap();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].text, "visible");
    }

    #[test]
    fn upsert_indexes_archived_flag() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().to_path_buf();
        let note = write_bundle(
            &notes,
            "abc",
            "---\narchived: true\n---\n# Hi\n\nbody\n",
        );
        let mut conn = fresh_conn();
        upsert_in(&mut conn, &note, &notes).unwrap();
        let archived: i64 = conn
            .query_row(
                "SELECT archived FROM notes WHERE bundle_id='abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived, 1);
    }

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
            archived: false,
            favorite: false,
            actions: vec![],
            tags: vec![],
            body: String::new(),
        };
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "/n/old/note.md", &mk("old", 100)).unwrap();
        upsert_in_tx(&tx, "/n/mid/note.md", &mk("mid", 500)).unwrap();
        upsert_in_tx(&tx, "/n/new/note.md", &mk("new", 900)).unwrap();
        tx.commit().unwrap();

        let items = list_all(&conn, NoteScope::Active).unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["new", "mid", "old"]);
    }

    // ----- events + edges backfill (#102) -----------------------------------

    /// Seed two team members: a self row (id 'tm_self', alias 'me@x.io')
    /// and a teammate ('tm_bob', alias 'bob@x.io'). Required setup for
    /// every backfill test below.
    fn seed_self_and_teammate(conn: &Connection) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES ('tm_self', 'Me', '', '/x/self.md', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES ('tm_bob', 'Bob', '', '/x/bob.md', 0, 0, 0)",
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

    fn seed_note_row(conn: &Connection, path: &str, modified: i64) {
        conn.execute(
            "INSERT INTO notes(note_path, bundle_id, title, modified_ms, body_size) \
             VALUES (?1, 'b', 'Title', ?2, 0)",
            rusqlite::params![path, modified],
        )
        .unwrap();
    }

    fn seed_action(conn: &Connection, id: &str, note_path: &str, assignee: Option<&str>) {
        conn.execute(
            "INSERT INTO actions(id, note_path, line, text, done, created_ms, assignee_id) \
             VALUES (?1, ?2, 1, 'task', 0, 100, ?3)",
            rusqlite::params![id, note_path, assignee],
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
        conn.execute(
            "INSERT INTO workstream_actions(\
                id, workstream_id, text, due_ms, source_kind, source_id, done, created_ms, assignee_id\
             ) VALUES (?1, ?2, 'task', NULL, 'email', 'src', 0, 100, ?3)",
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
        seed_note_row(&conn, "/n/x/note.md", 5_000);
        // One note-backed action with an assignee.
        seed_action(&conn, "a-1", "/n/x/note.md", Some("tm_bob"));
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
              'note', n.note_path,
              json_object('title', n.title, 'bundle_id', n.bundle_id),
              n.modified_ms
            FROM notes n;

            INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
            SELECT
              a.created_ms, 'action_created',
              COALESCE(a.assignee_id, (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1)),
              'action', a.id,
              json_object('text', a.text, 'note_path', a.note_path),
              a.created_ms
            FROM actions a;

            INSERT INTO events (ts_ms, kind, actor_id, ref_kind, ref_id, payload, created_ms)
            SELECT
              wa.created_ms, 'action_created',
              COALESCE(wa.assignee_id, (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1)),
              'action', wa.id,
              json_object('text', wa.text, 'workstream_id', wa.workstream_id),
              wa.created_ms
            FROM workstream_actions wa;

            INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
            SELECT 'workstream', s.workstream_id, s.kind, s.item_id, 'INCLUDES',
                   s.added_ms, s.added_ms
            FROM workstream_signals s
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

            INSERT OR IGNORE INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, first_seen_ms, last_seen_ms)
            SELECT 'person', wa.assignee_id, 'action', wa.id, 'OWNS',
                   wa.created_ms, wa.created_ms
            FROM workstream_actions wa
            WHERE wa.assignee_id IS NOT NULL
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
}
