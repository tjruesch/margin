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
//! Out of scope here: `notes_fts` is populated but no search command
//! exists yet; that lands with #31.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection, OptionalExtension, Result, Transaction};

use crate::notes::{
    bundle_dir_for_in, extract_preview, parse_frontmatter, read_tags, split_frontmatter,
    NoteListItem, NOTE_FILENAME, TRANSCRIPT_FILENAME,
};
use crate::paths;

const SCHEMA_V1: &str = include_str!("migrations/001_init.sql");
const SCHEMA_VERSION: i64 = 1;

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

fn apply_migrations(conn: &Connection) -> Result<()> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);

    if current.is_none() {
        conn.execute_batch(SCHEMA_V1)?;
    } else if current == Some(SCHEMA_VERSION) {
        // Up to date.
    } else {
        // Future: forward-only migrations from N+1.
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

/// All indexed notes, newest-first by `modified_ms`. Same shape as the
/// pre-DB `notes::list_notes` so the frontend doesn't need to change.
pub fn list_all(conn: &Connection) -> Result<Vec<NoteListItem>> {
    let mut stmt = conn.prepare(
        "SELECT n.note_path, n.title, n.modified_ms, n.duration_ms, n.preview \
         FROM notes n ORDER BY n.modified_ms DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(NoteRow {
            note_path: r.get(0)?,
            title: r.get(1)?,
            modified_ms: r.get(2)?,
            duration_ms: r.get(3)?,
            preview: r.get(4)?,
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
        })
        .collect())
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

    let disk_max_mtime = disk.iter().map(|d| d.modified_ms).max().unwrap_or(0);
    if db_count as usize == disk.len() && db_max_mtime == disk_max_mtime {
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
    tags: Vec<String>,
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
        tags,
        body: body.to_string(),
    })
}

fn upsert_in_tx(tx: &Transaction<'_>, note_path: &str, p: &Indexable) -> Result<()> {
    tx.execute(
        "INSERT INTO notes(note_path, bundle_id, title, modified_ms, duration_ms, preview, body_size) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(note_path) DO UPDATE SET \
            bundle_id = excluded.bundle_id, \
            title = excluded.title, \
            modified_ms = excluded.modified_ms, \
            duration_ms = excluded.duration_ms, \
            preview = excluded.preview, \
            body_size = excluded.body_size",
        params![
            note_path,
            p.bundle_id,
            p.title,
            p.modified_ms,
            p.duration_ms.map(|v| v as i64),
            p.preview,
            p.body_size,
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
    Ok(())
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
        conn.execute_batch(SCHEMA_V1).unwrap();
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
        assert_eq!(v, 1);
        // FTS table reachable.
        conn.query_row("SELECT count(*) FROM notes_fts", [], |r| r.get::<_, i64>(0))
            .unwrap();
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
        assert_eq!(v, 1);
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

        let items = list_all(&conn).unwrap();
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
        let items = list_all(&conn).unwrap();
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
        assert_eq!(list_all(&conn).unwrap().len(), 1);

        // Remove the bundle directory and reconcile.
        fs::remove_dir_all(note.parent().unwrap()).unwrap();
        let report = reconcile(&mut conn, &notes).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(list_all(&conn).unwrap().len(), 0);
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

        let items = list_all(&conn).unwrap();
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
    fn list_all_returns_newest_first() {
        let mut conn = fresh_conn();
        let mk = |id: &str, mtime: i64| Indexable {
            bundle_id: id.into(),
            title: id.into(),
            modified_ms: mtime,
            duration_ms: None,
            preview: String::new(),
            body_size: 0,
            tags: vec![],
            body: String::new(),
        };
        let tx = conn.transaction().unwrap();
        upsert_in_tx(&tx, "/n/old/note.md", &mk("old", 100)).unwrap();
        upsert_in_tx(&tx, "/n/mid/note.md", &mk("mid", 500)).unwrap();
        upsert_in_tx(&tx, "/n/new/note.md", &mk("new", 900)).unwrap();
        tx.commit().unwrap();

        let items = list_all(&conn).unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["new", "mid", "old"]);
    }
}
