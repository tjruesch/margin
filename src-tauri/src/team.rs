//! Team-member CRUD, the `meeting_attendees` join, and `actions.assignee_id`
//! writes.
//!
//! Profile bodies live on disk as `~/.margin/team/<member_id>/profile.md`,
//! outside the notes index. The `team_members.profile_md_path` column
//! stores the absolute path so the frontend can read/write the body via
//! the generic `read_file` / `write_file` commands.
//!
//! Self bootstrap runs once at app start (see `lib.rs::setup`), inserting
//! a single `is_self = 1` row if none exists. The partial unique index
//! `idx_team_self` guarantees there can never be more than one Self.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::paths;

#[derive(Serialize, Deserialize, Clone)]
pub struct TeamMember {
    pub id: String,
    pub display_name: String,
    pub role: String,
    pub aliases: Vec<TypedAlias>,
    pub profile_md_path: String,
    pub is_self: bool,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// One typed identity (#87). `kind` is a soft enum; the canonical
/// values live in [`kinds`]. Adding a new alias kind is a pure-add: a
/// new constant + a new resolver method, no schema change.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypedAlias {
    pub kind: String,
    pub value: String,
}

/// Canonical string values for `TypedAlias.kind`. Soft enum so adding a
/// new kind doesn't touch the schema or this module's surface.
pub mod kinds {
    pub const EMAIL: &str = "email";
    pub const NAME: &str = "name";
    pub const GITHUB_LOGIN: &str = "github_login";
    pub const SLACK_ID: &str = "slack_id";
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

fn member_dir(id: &str) -> PathBuf {
    paths::team_dir().join(id)
}

fn profile_path_for(id: &str) -> PathBuf {
    member_dir(id).join("profile.md")
}

fn write_stub_profile(profile_path: &Path, display_name: &str) -> Result<(), String> {
    if let Some(parent) = profile_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if !profile_path.exists() {
        fs::write(profile_path, format!("# {}\n", display_name)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn row_to_member(
    id: String,
    display_name: String,
    role: String,
    profile_md_path: String,
    is_self: i64,
    created_ms: i64,
    updated_ms: i64,
) -> TeamMember {
    TeamMember {
        id,
        display_name,
        role,
        // Aliases are joined separately to avoid a JSON column. Callers
        // populate this Vec via `attach_aliases` after the main query.
        aliases: Vec::new(),
        profile_md_path,
        is_self: is_self != 0,
        created_ms,
        updated_ms,
    }
}

const SELECT_MEMBER_COLS: &str = "id, display_name, role, profile_md_path, is_self, \
                                  created_ms, updated_ms";

/// Read all alias rows for the given member ids in a single query, then
/// attach them to each member. One extra round-trip regardless of input
/// size — no per-row N+1.
fn attach_aliases(conn: &Connection, members: &mut [TeamMember]) -> Result<(), String> {
    if members.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?")
        .take(members.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT member_id, kind, value FROM team_member_aliases \
         WHERE member_id IN ({placeholders}) \
         ORDER BY member_id, kind, value"
    );
    let id_refs: Vec<&dyn rusqlite::ToSql> = members
        .iter()
        .map(|m| &m.id as &dyn rusqlite::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(id_refs), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .map_err(|e| e.to_string())?;
    let mut by_member: HashMap<String, Vec<TypedAlias>> = HashMap::new();
    for row in rows {
        let (member_id, kind, value) = row.map_err(|e| e.to_string())?;
        by_member
            .entry(member_id)
            .or_default()
            .push(TypedAlias { kind, value });
    }
    for m in members.iter_mut() {
        if let Some(v) = by_member.remove(&m.id) {
            m.aliases = v;
        }
    }
    Ok(())
}

fn fetch_one(conn: &Connection, id: &str) -> Result<TeamMember, String> {
    let sql = format!(
        "SELECT {SELECT_MEMBER_COLS} FROM team_members WHERE id = ?1"
    );
    let member = conn
        .query_row(&sql, params![id], |r| {
            Ok(row_to_member(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("team member not found: {id}"))?;
    let mut members = vec![member];
    attach_aliases(conn, &mut members)?;
    Ok(members.into_iter().next().unwrap())
}

fn default_self_display_name() -> String {
    let real = whoami::realname();
    if !real.trim().is_empty() {
        return real;
    }
    if let Ok(user) = std::env::var("USER") {
        if !user.trim().is_empty() {
            return user;
        }
    }
    "You".to_string()
}

/// Insert the Self row if it does not already exist. Idempotent — safe
/// to call on every app start. Called from `lib.rs::setup` after
/// migrations apply, before the connection is moved into Tauri state.
pub fn bootstrap_self_if_missing(conn: &mut Connection) -> Result<(), String> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if existing.is_some() {
        return Ok(());
    }

    let id = uuid::Uuid::new_v4().to_string();
    let display_name = default_self_display_name();
    let profile_md_path = profile_path_for(&id);
    write_stub_profile(&profile_md_path, &display_name)?;
    let now = now_ms();
    conn.execute(
        "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, \
         created_ms, updated_ms) VALUES (?1, ?2, '', ?3, 1, ?4, ?4)",
        params![id, display_name, profile_md_path.to_string_lossy().to_string(), now],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Internal helper used by both the Tauri command and the indexer
/// (#49). Same query as the command — kept here so callers with direct
/// `&Connection` access (e.g. inside a transaction) don't need to go
/// through Tauri's invoke_handler.
pub(crate) fn list_team_members_raw(conn: &Connection) -> Result<Vec<TeamMember>, String> {
    let sql = format!(
        "SELECT {SELECT_MEMBER_COLS} FROM team_members \
         ORDER BY is_self DESC, display_name COLLATE NOCASE ASC"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(row_to_member(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    attach_aliases(conn, &mut out)?;
    Ok(out)
}

#[tauri::command]
pub fn list_team_members(
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<TeamMember>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    list_team_members_raw(&c)
}

/// NFD-decompose, drop combining marks (the diacritics), then lowercase.
/// Used by `OwnerResolver` to match action-item owner candidates against
/// `display_name ∪ aliases` regardless of case or accent (#49).
fn fold_for_match(s: &str) -> String {
    s.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Case- and accent-insensitive lookup from a candidate name to a team
/// member id. Built once per indexer pass from the current team_members
/// snapshot (#49). `resolve` returns `Some(id)` only when the normalized
/// candidate maps to exactly one member; ambiguous and unmatched
/// candidates both return `None`.
pub(crate) struct OwnerResolver {
    by_key: HashMap<String, Vec<String>>, // normalized → member ids
}

impl OwnerResolver {
    pub(crate) fn from_members(members: &[TeamMember]) -> Self {
        let mut by_key: HashMap<String, Vec<String>> = HashMap::new();
        for m in members {
            let mut keys: Vec<String> = Vec::with_capacity(1 + m.aliases.len());
            keys.push(fold_for_match(&m.display_name));
            // Only `name`-kind aliases participate in name resolution
            // (#87). Email / GitHub / Slack handles aren't names, even
            // when their string shape happens to look like one.
            for a in &m.aliases {
                if a.kind != kinds::NAME {
                    continue;
                }
                let k = fold_for_match(&a.value);
                if !k.is_empty() {
                    keys.push(k);
                }
            }
            for k in keys {
                if k.is_empty() {
                    continue;
                }
                let entry = by_key.entry(k).or_default();
                if !entry.contains(&m.id) {
                    entry.push(m.id.clone());
                }
            }
        }
        Self { by_key }
    }

    pub(crate) fn resolve(&self, candidate: &str) -> Option<String> {
        let key = fold_for_match(candidate.trim());
        if key.is_empty() {
            return None;
        }
        match self.by_key.get(&key) {
            Some(ids) if ids.len() == 1 => Some(ids[0].clone()),
            _ => None,
        }
    }
}

#[tauri::command]
pub fn get_team_member(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<TeamMember, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    fetch_one(&c, &id)
}

#[tauri::command]
pub fn create_team_member(
    display_name: String,
    role: String,
    aliases: Vec<TypedAlias>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<TeamMember, String> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        return Err("display_name is required".to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let profile_md_path = profile_path_for(&id);
    write_stub_profile(&profile_md_path, trimmed)?;
    let now = now_ms();
    {
        let mut c = conn.lock().map_err(|e| e.to_string())?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, \
             is_self, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
            params![
                id,
                trimmed,
                role,
                profile_md_path.to_string_lossy().to_string(),
                now,
            ],
        )
        .map_err(|e| e.to_string())?;
        write_aliases(&tx, &id, &aliases).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    let c = conn.lock().map_err(|e| e.to_string())?;
    fetch_one(&c, &id)
}

/// Replace all alias rows for `member_id` with `aliases`. Caller is
/// responsible for the transaction. Empty values are filtered; the PK
/// `(member_id, kind, value)` enforces dedup at the SQL layer so even
/// a sloppy caller can't double-insert.
fn write_aliases(
    tx: &rusqlite::Transaction<'_>,
    member_id: &str,
    aliases: &[TypedAlias],
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM team_member_aliases WHERE member_id = ?1",
        params![member_id],
    )?;
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO team_member_aliases(member_id, kind, value) \
         VALUES (?1, ?2, ?3)",
    )?;
    for a in aliases {
        let kind = a.kind.trim();
        let value = a.value.trim();
        if kind.is_empty() || value.is_empty() {
            continue;
        }
        stmt.execute(params![member_id, kind, value])?;
    }
    Ok(())
}

#[tauri::command]
pub fn update_team_member(
    id: String,
    display_name: Option<String>,
    role: Option<String>,
    aliases: Option<Vec<TypedAlias>>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<TeamMember, String> {
    if display_name.is_none() && role.is_none() && aliases.is_none() {
        let c = conn.lock().map_err(|e| e.to_string())?;
        return fetch_one(&c, &id);
    }
    let now = now_ms();
    {
        let mut c = conn.lock().map_err(|e| e.to_string())?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        if let Some(name) = display_name.as_deref() {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err("display_name cannot be empty".to_string());
            }
            tx.execute(
                "UPDATE team_members SET display_name = ?1, updated_ms = ?2 WHERE id = ?3",
                params![trimmed, now, id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(role) = role.as_deref() {
            tx.execute(
                "UPDATE team_members SET role = ?1, updated_ms = ?2 WHERE id = ?3",
                params![role, now, id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(aliases) = aliases.as_deref() {
            write_aliases(&tx, &id, aliases).map_err(|e| e.to_string())?;
            // Stamp `updated_ms` so the workstreams list / detail caches
            // re-render even when only aliases changed.
            tx.execute(
                "UPDATE team_members SET updated_ms = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
    }
    let c = conn.lock().map_err(|e| e.to_string())?;
    fetch_one(&c, &id)
}

#[tauri::command]
pub fn delete_team_member(
    id: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    {
        let c = conn.lock().map_err(|e| e.to_string())?;
        let is_self: Option<i64> = c
            .query_row(
                "SELECT is_self FROM team_members WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match is_self {
            None => return Err(format!("team member not found: {id}")),
            Some(1) => return Err("cannot delete the Self profile".to_string()),
            _ => {}
        }
        c.execute("DELETE FROM team_members WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
    }
    let dir = member_dir(&id);
    if dir.exists() {
        if let Err(e) = fs::remove_dir_all(&dir) {
            // DB row is already gone; surface a soft error so the user
            // knows the bundle leaked but the operation otherwise
            // succeeded.
            eprintln!("team: failed to remove {}: {e}", dir.display());
        }
    }
    Ok(())
}

#[tauri::command]
pub fn set_meeting_attendees(
    note_path: String,
    member_ids: Vec<String>,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<(), String> {
    let mut c = conn.lock().map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM meeting_attendees WHERE note_path = ?1",
        params![note_path],
    )
    .map_err(|e| e.to_string())?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO meeting_attendees(note_path, member_id) VALUES (?1, ?2) \
                 ON CONFLICT(note_path, member_id) DO NOTHING",
            )
            .map_err(|e| e.to_string())?;
        for member_id in &member_ids {
            stmt.execute(params![note_path, member_id])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Internal helper used by both the Tauri command and `reconcile.rs`'s
/// in-process attendee fetch (#48). Same query as the command — kept here
/// so reconcile_notes doesn't have to duplicate the SQL or go through
/// Tauri's invoke_handler when it already holds the AppHandle.
pub(crate) fn list_meeting_attendees(
    conn: &Connection,
    note_path: &str,
) -> Result<Vec<TeamMember>, String> {
    let sql = format!(
        "SELECT {} FROM team_members t \
         JOIN meeting_attendees a ON a.member_id = t.id \
         WHERE a.note_path = ?1 \
         ORDER BY t.is_self DESC, t.display_name COLLATE NOCASE ASC",
        SELECT_MEMBER_COLS
            .split(", ")
            .map(|c| format!("t.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![note_path], |r| {
            Ok(row_to_member(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    attach_aliases(conn, &mut out)?;
    Ok(out)
}

#[tauri::command]
pub fn get_meeting_attendees(
    note_path: String,
    conn: tauri::State<'_, std::sync::Mutex<rusqlite::Connection>>,
) -> Result<Vec<TeamMember>, String> {
    let c = conn.lock().map_err(|e| e.to_string())?;
    list_meeting_attendees(&c, &note_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_member(id: &str, name: &str, aliases: &[(&str, &str)]) -> TeamMember {
        TeamMember {
            id: id.into(),
            display_name: name.into(),
            role: String::new(),
            aliases: aliases
                .iter()
                .map(|(k, v)| TypedAlias {
                    kind: (*k).to_string(),
                    value: (*v).to_string(),
                })
                .collect(),
            profile_md_path: String::new(),
            is_self: false,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn fold_for_match_strips_diacritics_and_lowers_case() {
        assert_eq!(fold_for_match("José"), "jose");
        assert_eq!(fold_for_match("Müller"), "muller");
        assert_eq!(fold_for_match("Tom Ruesch"), "tom ruesch");
        assert_eq!(fold_for_match(""), "");
    }

    #[test]
    fn owner_resolver_matches_display_name_and_aliases() {
        let members = vec![make_member(
            "tom-id",
            "Tom Ruesch",
            &[("name", "TJ"), ("name", "Tom")],
        )];
        let r = OwnerResolver::from_members(&members);
        assert_eq!(r.resolve("Tom"), Some("tom-id".into()));
        assert_eq!(r.resolve("tom"), Some("tom-id".into()));
        assert_eq!(r.resolve("TJ"), Some("tom-id".into()));
        assert_eq!(r.resolve("Tom Ruesch"), Some("tom-id".into()));
    }

    #[test]
    fn owner_resolver_returns_none_when_ambiguous() {
        let members = vec![
            make_member("tom-id", "Tom Ruesch", &[("name", "TR")]),
            make_member("tina-id", "Tina Romero", &[("name", "TR")]),
        ];
        let r = OwnerResolver::from_members(&members);
        assert_eq!(r.resolve("TR"), None);
        // Unambiguous full names still resolve.
        assert_eq!(r.resolve("Tom Ruesch"), Some("tom-id".into()));
    }

    #[test]
    fn owner_resolver_returns_none_when_unknown() {
        let members = vec![make_member("tom-id", "Tom Ruesch", &[("name", "TJ")])];
        let r = OwnerResolver::from_members(&members);
        assert_eq!(r.resolve("Sarah"), None);
        assert_eq!(r.resolve(""), None);
        assert_eq!(r.resolve("   "), None);
    }

    #[test]
    fn owner_resolver_folds_accents_in_lookups() {
        let members = vec![make_member("jose-id", "José", &[])];
        let r = OwnerResolver::from_members(&members);
        assert_eq!(r.resolve("Jose"), Some("jose-id".into()));
        assert_eq!(r.resolve("JOSÉ"), Some("jose-id".into()));
    }

    /// Run all migrations 1..=16 against a fresh in-memory DB, stopping
    /// short of 17 so the test can manually seed pre-#87 data.
    fn open_db_at_version_16() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Apply migrations 1..=16 verbatim. We can't call
        // `index::apply_migrations` because that would jump to 17. Instead
        // run each include_str! batch in order.
        for sql in [
            include_str!("migrations/001_init.sql"),
            include_str!("migrations/002_archived.sql"),
            include_str!("migrations/003_favorite.sql"),
            include_str!("migrations/004_actions.sql"),
            include_str!("migrations/005_due_dates.sql"),
            include_str!("migrations/006_team_members.sql"),
            include_str!("migrations/007_action_owners.sql"),
            include_str!("migrations/008_connectors.sql"),
            include_str!("migrations/009_calendar.sql"),
            include_str!("migrations/010_event_note_link.sql"),
            include_str!("migrations/011_email.sql"),
            include_str!("migrations/012_workstreams.sql"),
            include_str!("migrations/013_workstream_user_notes.sql"),
            include_str!("migrations/014_workstream_archive_resurface.sql"),
            include_str!("migrations/015_workstream_owner.sql"),
            include_str!("migrations/016_workstream_signals.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn
    }

    #[test]
    fn migration_017_backfills_typed_aliases() {
        let conn = open_db_at_version_16();
        // Seed the legacy JSON-aliases shape: one email-shaped, one name.
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, aliases, profile_md_path, \
             is_self, created_ms, updated_ms) \
             VALUES ('m1', 'Heike Müller', '', \
                     '[\"heike@example.com\",\"Heike\"]', '', 0, 0, 0)",
            [],
        )
        .unwrap();

        // Apply migration 17.
        conn.execute_batch(include_str!("migrations/017_typed_aliases.sql"))
            .unwrap();

        // Pivot rows split by `@` sniff.
        let rows: Vec<(String, String, String)> = conn
            .prepare(
                "SELECT member_id, kind, value FROM team_member_aliases \
                 WHERE member_id = 'm1' ORDER BY kind, value",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                ("m1".into(), "email".into(), "heike@example.com".into()),
                ("m1".into(), "name".into(), "Heike".into()),
            ]
        );

        // The legacy column is gone.
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(team_members)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            !columns.iter().any(|c| c == "aliases"),
            "team_members.aliases should be dropped, found columns: {:?}",
            columns
        );

        // schema_version stamped.
        let v: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 17);
    }

    #[test]
    fn migration_017_filters_empty_aliases() {
        let conn = open_db_at_version_16();
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, aliases, profile_md_path, \
             is_self, created_ms, updated_ms) \
             VALUES ('m1', 'Heike', '', '[\"\",\"heike@example.com\",\"\"]', '', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute_batch(include_str!("migrations/017_typed_aliases.sql"))
            .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM team_member_aliases WHERE member_id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "empty strings filtered");
    }

    #[test]
    fn owner_resolver_ignores_non_name_aliases() {
        // An email-kind alias's local part used to fold into the
        // name-match dictionary because all aliases were untyped (#87).
        // Now name resolution only considers `kind == "name"` aliases —
        // display_name still always counts.
        let members = vec![make_member(
            "heike-id",
            "Heike Müller",
            &[
                ("email", "heike@example.com"),
                ("github_login", "heike-mueller"),
            ],
        )];
        let r = OwnerResolver::from_members(&members);
        assert_eq!(
            r.resolve("Heike Müller"),
            Some("heike-id".into()),
            "display_name still resolves"
        );
        assert_eq!(
            r.resolve("heike"),
            None,
            "email local part no longer resolves through aliases"
        );
        assert_eq!(
            r.resolve("heike-mueller"),
            None,
            "github_login does not resolve as a name"
        );
    }
}
