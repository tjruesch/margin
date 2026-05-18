//! Phase 1.4 (#146) — one-time backfill of pre-#144 reconciled notes.
//!
//! Walks every note that has a sibling `transcript.json` (proxied by
//! `notes.duration_ms IS NOT NULL`) and an inline `## Action items`
//! block in its `body_md`. For each such note: parses the block into
//! reconcile-origin rows, preserves done/manual_override/assignee_id
//! across the origin flip, strips the block from `body_md` via
//! `upsert_in_tx`, and writes a per-note `.action-items-backup.md`
//! snapshot of the original body for rollback.
//!
//! Idempotent: a subsequent run finds nothing to migrate because the
//! `## Action items` heading is no longer in the body. Transactional
//! per-note: a failure on one note doesn't abort the rest.

use std::fs;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

const BACKUP_FILENAME: &str = ".action-items-backup.md";
const FLAG_KEY: &str = "actions_migration_v1_completed";

#[derive(Serialize, Default, Debug, Clone)]
pub struct MigrationReport {
    pub dry_run: bool,
    /// Notes with a transcript and a `## Action items` substring in body.
    pub candidates_scanned: u32,
    /// Notes whose body actually contained a block we extracted.
    pub notes_migrated: u32,
    /// Reconcile-origin rows touched (inserted or replaced-with-preserved).
    pub rows_created: u32,
    /// New backup files written (i.e. ones that didn't already exist).
    pub backups_written: u32,
    /// Candidates that matched on the LIKE heuristic but had no real block.
    pub notes_already_clean: u32,
    /// Per-note error messages (the migration continues on per-note failure).
    pub errors: Vec<String>,
}

struct Candidate {
    note_id: String,
    body_md: String,
}

pub fn run(conn: &mut Connection, dry_run: bool) -> Result<MigrationReport, String> {
    let candidates = load_candidates(conn)?;
    let mut report = MigrationReport {
        dry_run,
        ..Default::default()
    };

    let members = crate::team::list_team_members_raw(conn)?;
    let resolver = crate::team::OwnerResolver::from_members(&members);
    let self_id: Option<String> = conn
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;

    for cand in candidates {
        report.candidates_scanned += 1;
        match migrate_one(
            conn,
            &cand.note_id,
            &cand.body_md,
            &resolver,
            self_id.as_deref(),
            dry_run,
        ) {
            Ok(one) => {
                if one.had_block {
                    report.notes_migrated += 1;
                    report.rows_created += one.rows_created;
                    if one.backup_written {
                        report.backups_written += 1;
                    }
                } else {
                    report.notes_already_clean += 1;
                }
            }
            Err(e) => report.errors.push(format!("{}: {}", cand.note_id, e)),
        }
    }

    if !dry_run && report.errors.is_empty() {
        conn.execute(
            "UPDATE meta SET value = '1' WHERE key = ?1",
            params![FLAG_KEY],
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(report)
}

/// Boot-time wrapper. Reads the migration flag; if `'0'`, runs the
/// migration with `dry_run=false` and (on success) flips the flag.
/// Silent — failures are logged to stderr, never thrown. Mirrors the
/// shape of `team::purge_profile_md_if_pending` at `team.rs:186`.
pub fn run_if_pending(conn: &mut Connection) {
    let pending = match read_flag(conn) {
        Ok(v) => v == "0",
        Err(e) => {
            eprintln!("[actions_migration] read flag failed: {e}");
            return;
        }
    };
    if !pending {
        return;
    }
    match run(conn, false) {
        Ok(report) => {
            if !report.errors.is_empty() {
                eprintln!(
                    "[actions_migration] completed with {} error(s):",
                    report.errors.len()
                );
                for err in &report.errors {
                    eprintln!("  - {err}");
                }
            } else if report.notes_migrated > 0 {
                eprintln!(
                    "[actions_migration] migrated {} note(s), {} row(s), {} backup(s)",
                    report.notes_migrated, report.rows_created, report.backups_written,
                );
            } else {
                eprintln!("[actions_migration] no candidates — flag set");
            }
        }
        Err(e) => eprintln!("[actions_migration] failed: {e}"),
    }
}

fn read_flag(conn: &Connection) -> Result<String, String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![FLAG_KEY],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map(|v| v.unwrap_or_else(|| "1".to_string()))
    .map_err(|e| e.to_string())
}

fn load_candidates(conn: &Connection) -> Result<Vec<Candidate>, String> {
    // `duration_ms IS NOT NULL` is the cheap proxy for "has a sibling
    // transcript.json" — `parse_indexable_from_body` only populates
    // that column when the file exists. `body_md LIKE '%## Action
    // items%'` is the cheap inclusion filter; false positives are
    // harmless (the splitter returns empty raw_lines and we count the
    // note as already-clean).
    let mut stmt = conn
        .prepare(
            "SELECT id, body_md FROM notes \
              WHERE duration_ms IS NOT NULL \
                AND body_md LIKE '%## Action items%' \
              ORDER BY id",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Candidate {
                note_id: r.get(0)?,
                body_md: r.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

struct MigrationOne {
    rows_created: u32,
    backup_written: bool,
    had_block: bool,
}

fn migrate_one(
    conn: &mut Connection,
    note_id: &str,
    body: &str,
    resolver: &crate::team::OwnerResolver,
    self_id: Option<&str>,
    dry_run: bool,
) -> Result<MigrationOne, String> {
    let (stripped, raw_lines) = crate::reconcile::split_action_items_block(body);
    if raw_lines.is_empty() {
        return Ok(MigrationOne {
            rows_created: 0,
            backup_written: false,
            had_block: false,
        });
    }

    // Backup BEFORE any state change. Skipped on dry_run. Don't
    // overwrite an existing backup — that one captures the
    // first-migration body and is the user's true rollback target.
    let mut backup_written = false;
    if !dry_run {
        let backup_path = std::path::Path::new(note_id)
            .parent()
            .ok_or_else(|| format!("no parent dir for note path: {note_id}"))?
            .join(BACKUP_FILENAME);
        if !backup_path.exists() {
            // Make sure the parent dir exists. Normally it does (the
            // note bundle), but in tempdir-based tests we may seed a
            // note_id whose dir isn't on disk.
            if let Some(parent) = backup_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create bundle dir: {e}"))?;
            }
            let header = "<!-- Margin action-items backup (#146).\n\
                          Original body before reconcile-origin migration.\n\
                          To roll back this note: copy the body below back into\n\
                          note.md and delete this note's reconcile-origin actions\n\
                          rows from the DB.\n\
                          -->\n\n";
            fs::write(&backup_path, format!("{header}{body}"))
                .map_err(|e| format!("write backup: {e}"))?;
            backup_written = true;
        }
    }

    if dry_run {
        let n = raw_lines
            .iter()
            .filter_map(|l| crate::notes::parse_action_line(l.trim_start()))
            .count() as u32;
        return Ok(MigrationOne {
            rows_created: n,
            backup_written: false,
            had_block: true,
        });
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let now_ms = crate::events::current_unix_ms();
    let mut rows_created = 0u32;

    for raw in &raw_lines {
        let trimmed = raw.trim_start();
        let Some((text, body_done, due_ms)) = crate::notes::parse_action_line(trimmed) else {
            continue;
        };
        let id = crate::notes::action_id(note_id, &text);

        // Preserve done / manual_override / assignee_id / created_ms
        // from any existing row with this id. The pre-existing row is
        // typically the note-origin row produced by the inline parser;
        // on a re-run it's the reconcile-origin row from a prior pass.
        let existing: Option<(i64, i64, Option<String>, i64)> = tx
            .query_row(
                "SELECT done, manual_override, assignee_id, created_ms \
                   FROM actions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        let had_existing = existing.is_some();
        let (done, manual_override, assignee_id, created_ms) = match existing {
            Some((d, m, a, c)) => (d != 0, m != 0, a, c),
            None => {
                let assignee = crate::notes::extract_owner_candidate(&text)
                    .and_then(|c| resolver.resolve(&c));
                (body_done, false, assignee, now_ms)
            }
        };

        tx.execute("DELETE FROM actions WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO actions \
                (id, origin_kind, origin_note_id, origin_line, text, done, \
                 manual_override, created_ms, due_ms, assignee_id) \
             VALUES (?1, 'reconcile', ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                note_id,
                text,
                done as i64,
                manual_override as i64,
                created_ms,
                due_ms,
                assignee_id,
            ],
        )
        .map_err(|e| e.to_string())?;

        if !had_existing {
            let actor = assignee_id.as_deref().or(self_id);
            let payload = serde_json::json!({"text": text, "note_id": note_id});
            crate::events::emit(
                &tx,
                now_ms,
                "action_created",
                actor,
                "action",
                &id,
                &payload,
            )
            .map_err(|e| e.to_string())?;
        }

        rows_created += 1;
    }

    // Refresh derived state against the stripped body. `upsert_in_tx`
    // updates `notes.body_md`, refreshes FTS, and wipes-and-replaces
    // note-origin rows for this note (scoped to origin_kind='note', so
    // our just-inserted reconcile-origin rows are untouched).
    let parsed = crate::index::parse_indexable_from_body(note_id, &stripped, now_ms);
    crate::index::upsert_in_tx(&tx, note_id, &parsed).map_err(|e| e.to_string())?;

    tx.commit().map_err(|e| e.to_string())?;

    Ok(MigrationOne {
        rows_created,
        backup_written,
        had_block: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    /// Seed a note row whose `id` is an absolute path under `dir/bundle/note.md`.
    /// Creates the bundle directory on disk so backup writes land in the tempdir.
    fn seed_note(
        conn: &Connection,
        dir: &TempDir,
        bundle: &str,
        body: &str,
        has_transcript: bool,
    ) -> String {
        let bundle_dir = dir.path().join(bundle);
        fs::create_dir_all(&bundle_dir).unwrap();
        let note_id = bundle_dir.join("note.md").to_string_lossy().into_owned();
        let duration: Option<i64> = if has_transcript { Some(60_000) } else { None };
        conn.execute(
            "INSERT INTO notes(id, bundle_id, title, modified_ms, body_size, \
                                body_md, duration_ms) \
             VALUES (?1, ?2, 'Title', 100, ?3, ?4, ?5)",
            params![note_id, bundle, body.len() as i64, body, duration],
        )
        .unwrap();
        note_id
    }

    fn body_with_block() -> String {
        "# Meeting\n\
         \n\
         ## Summary\n\
         \n\
         We discussed plans.\n\
         \n\
         ## Action items\n\
         \n\
         - [ ] task one\n\
         - [x] task two\n\
         \n\
         ## Open questions\n\
         \n\
         - [?] who owns deploy?\n"
            .to_string()
    }

    #[test]
    fn migrate_creates_reconcile_rows_and_strips_section() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        let note_id = seed_note(&conn, &dir, "b1", &body_with_block(), true);

        let report = run(&mut conn, false).unwrap();
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(report.candidates_scanned, 1);
        assert_eq!(report.notes_migrated, 1);
        assert_eq!(report.rows_created, 2);

        // Both rows are reconcile-origin, origin_line is NULL.
        let mut stmt = conn
            .prepare(
                "SELECT origin_kind, origin_line, text, done FROM actions \
                  WHERE origin_note_id = ?1 ORDER BY text",
            )
            .unwrap();
        let rows: Vec<(String, Option<i64>, String, i64)> = stmt
            .query_map(params![note_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for (kind, line, _text, _done) in &rows {
            assert_eq!(kind, "reconcile");
            assert!(line.is_none());
        }
        // body_md is stripped.
        let body: String = conn
            .query_row(
                "SELECT body_md FROM notes WHERE id = ?1",
                params![note_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!body.contains("## Action items"));
        assert!(body.contains("## Summary"));
        assert!(body.contains("## Open questions"));
    }

    #[test]
    fn migrate_preserves_done_state_on_already_completed_items() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        let body = "## Action items\n\n- [x] task done\n";
        let note_id = seed_note(&conn, &dir, "b1", body, true);

        // Pre-seed the note-origin row that the inline parser would have
        // created. done=1 because the body has [x].
        let id = crate::notes::action_id(&note_id, "task done");
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_note_id, origin_line, \
                text, done, created_ms\
             ) VALUES (?1, 'note', ?2, 1, 'task done', 1, 50)",
            params![id, note_id],
        )
        .unwrap();

        let report = run(&mut conn, false).unwrap();
        assert!(report.errors.is_empty());

        let (kind, done, created_ms): (String, i64, i64) = conn
            .query_row(
                "SELECT origin_kind, done, created_ms FROM actions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "reconcile");
        assert_eq!(done, 1, "done flipped during migration");
        assert_eq!(created_ms, 50, "created_ms preserved");
    }

    #[test]
    fn migrate_preserves_manual_override_and_assignee() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        // Seed a team member so a synthetic assignee_id passes the FK.
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES ('tm_x', 'X', '', 0, 100, 100)",
            [],
        )
        .unwrap();
        let body = "## Action items\n\n- [ ] task one\n";
        let note_id = seed_note(&conn, &dir, "b1", body, true);
        let id = crate::notes::action_id(&note_id, "task one");
        conn.execute(
            "INSERT INTO actions(\
                id, origin_kind, origin_note_id, origin_line, \
                text, done, manual_override, assignee_id, created_ms\
             ) VALUES (?1, 'note', ?2, 1, 'task one', 0, 1, 'tm_x', 75)",
            params![id, note_id],
        )
        .unwrap();

        run(&mut conn, false).unwrap();

        let (mo, assignee): (i64, Option<String>) = conn
            .query_row(
                "SELECT manual_override, assignee_id FROM actions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mo, 1, "manual_override preserved");
        assert_eq!(assignee.as_deref(), Some("tm_x"));
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        seed_note(&conn, &dir, "b1", &body_with_block(), true);

        let r1 = run(&mut conn, false).unwrap();
        assert_eq!(r1.notes_migrated, 1);

        // body_md no longer contains the heading, so the candidate
        // filter returns nothing.
        let r2 = run(&mut conn, false).unwrap();
        assert_eq!(r2.candidates_scanned, 0);
        assert_eq!(r2.notes_migrated, 0);
        assert_eq!(r2.rows_created, 0);
    }

    #[test]
    fn migrate_skips_notes_without_transcript() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        seed_note(&conn, &dir, "b1", &body_with_block(), false /* no transcript */);

        let report = run(&mut conn, false).unwrap();
        assert_eq!(report.candidates_scanned, 0);
        assert_eq!(report.notes_migrated, 0);
    }

    #[test]
    fn migrate_writes_backup_file_with_original_body() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        let original = body_with_block();
        let note_id = seed_note(&conn, &dir, "b1", &original, true);

        run(&mut conn, false).unwrap();

        let backup_path = std::path::Path::new(&note_id)
            .parent()
            .unwrap()
            .join(BACKUP_FILENAME);
        assert!(backup_path.exists(), "backup file missing: {:?}", backup_path);
        let content = fs::read_to_string(&backup_path).unwrap();
        assert!(content.starts_with("<!-- Margin action-items backup"));
        assert!(content.contains(&original), "backup must contain original body");
    }

    #[test]
    fn migrate_dry_run_does_not_mutate() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        let note_id = seed_note(&conn, &dir, "b1", &body_with_block(), true);

        let report = run(&mut conn, true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.notes_migrated, 1);
        assert_eq!(report.rows_created, 2);
        assert_eq!(report.backups_written, 0);

        // Body unchanged.
        let body: String = conn
            .query_row(
                "SELECT body_md FROM notes WHERE id = ?1",
                params![note_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.contains("## Action items"));

        // No reconcile-origin rows.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions WHERE origin_kind = 'reconcile'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);

        // No backup file.
        let backup_path = std::path::Path::new(&note_id)
            .parent()
            .unwrap()
            .join(BACKUP_FILENAME);
        assert!(!backup_path.exists());

        // Flag still '0'.
        assert_eq!(read_flag(&conn).unwrap(), "0");
    }

    #[test]
    fn migrate_flips_flag_on_success() {
        let mut conn = fresh_conn();
        assert_eq!(read_flag(&conn).unwrap(), "0");
        run(&mut conn, false).unwrap();
        assert_eq!(read_flag(&conn).unwrap(), "1");

        // run_if_pending is a no-op the second time.
        let mut conn2 = fresh_conn();
        run_if_pending(&mut conn2);
        assert_eq!(read_flag(&conn2).unwrap(), "1");
        // And again — definitely no-op.
        run_if_pending(&mut conn2);
        assert_eq!(read_flag(&conn2).unwrap(), "1");
    }

    #[test]
    fn migrate_preserves_existing_backup() {
        let dir = TempDir::new().unwrap();
        let mut conn = fresh_conn();
        let note_id = seed_note(&conn, &dir, "b1", &body_with_block(), true);
        let backup_path = std::path::Path::new(&note_id)
            .parent()
            .unwrap()
            .join(BACKUP_FILENAME);
        let sentinel = "DO NOT TOUCH";
        fs::write(&backup_path, sentinel).unwrap();

        let report = run(&mut conn, false).unwrap();
        assert_eq!(report.notes_migrated, 1);
        assert_eq!(report.backups_written, 0, "existing backup must not be re-written");

        let content = fs::read_to_string(&backup_path).unwrap();
        assert_eq!(content, sentinel);
    }
}
