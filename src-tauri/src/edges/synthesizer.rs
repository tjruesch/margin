//! Deterministic edge synthesizer (#103). Walks events + entity tables
//! and re-derives the `edges` graph layer. No LLM calls.
//!
//! Seven edge kinds in v1: AUTHORED, REPLIED_TO, MENTIONED, CO_ATTENDED,
//! ATTENDED, INCLUDES, OWNS. Each kind runs as its own UPSERT pass
//! against `edges` keyed on the natural PK (src, src_id, tgt, tgt_id,
//! edge_kind). Re-running the synth is idempotent: `first_seen_ms` is
//! preserved, `last_seen_ms` and `confidence` get monotonically updated.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::team::{self, fold_for_match, kinds, TeamMember};

/// Skip a run when the last successful pass was within this window
/// (unless `force=true`). 5 minutes is a sanity guard against rapid-
/// fire calls from the boot tick + post-workstream-synth callback;
/// not a policy. Manual IPC + the workstream-synth chained call both
/// pass `force=false`.
const EDGE_SYNTH_TTL_MS: i64 = 5 * 60 * 1000;

/// Lookback window for CO_ATTENDED inference. Meetings older than
/// this don't contribute. Sliding window keeps the edge fresh —
/// people who stopped meeting drop below the threshold.
const CO_ATTENDED_WINDOW_MS: i64 = 60 * 24 * 3600 * 1000;

/// Minimum shared meetings (within the window) to emit CO_ATTENDED.
/// 2 is the smallest signal that's more than coincidence.
const CO_ATTENDED_MIN_MEETINGS: i64 = 2;

const META_LAST_EDGE_SYNTH_MS: &str = "last_edge_synth_ms";

/// Process-wide guard. Boot, post-workstream-synth, and manual IPC
/// all serialize on this lock. `try_lock` non-blocking; if held, the
/// caller bails with `state="skipped"`.
pub fn synth_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct EdgeSynthReport {
    pub state: String, // "skipped" | "synced" | "errored"
    /// Per-edge-kind count of rows touched this pass (INSERT + UPDATE
    /// combined, via SQLite `changes()`).
    pub by_kind: HashMap<String, u32>,
    /// Sum of by_kind.
    pub total_touched: u32,
    pub last_run_ms: i64,
}

#[derive(Serialize, Clone)]
struct StatusEvent<'a> {
    state: &'a str,
    message: Option<String>,
}

fn emit_status(app: &AppHandle, state: &str, message: Option<String>) {
    let _ = app.emit(
        "edge-synth-status",
        StatusEvent { state, message },
    );
}

pub async fn maybe_run(app: &AppHandle, force: bool) -> Result<EdgeSynthReport, String> {
    let lock = synth_lock();
    let _guard = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("[edges] another synth pass in flight; skipping");
            return Ok(EdgeSynthReport {
                state: "skipped".into(),
                ..Default::default()
            });
        }
    };

    let conn_state = app.state::<Mutex<Connection>>();
    let now_ms = current_unix_ms();

    let last = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        read_last_run_ms(&c)
    };

    if !force && now_ms.saturating_sub(last) < EDGE_SYNTH_TTL_MS {
        return Ok(EdgeSynthReport {
            state: "skipped".into(),
            last_run_ms: last,
            ..Default::default()
        });
    }

    emit_status(app, "running", None);

    let mut report = EdgeSynthReport {
        state: "synced".into(),
        ..Default::default()
    };

    let team_snapshot = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        team::list_team_members_raw(&c).unwrap_or_default()
    };
    let matcher = MentionMatcher::from_members(&team_snapshot);

    {
        let mut c = conn_state.lock().map_err(|e| e.to_string())?;
        if let Err(e) = run_passes(&mut c, &matcher, now_ms, &mut report) {
            emit_status(app, "errored", Some(e.clone()));
            return Err(e);
        }
        // Stamp last-run only on success.
        write_last_run_ms(&c, now_ms).map_err(|e| e.to_string())?;
    }

    report.total_touched = report.by_kind.values().sum();
    report.last_run_ms = now_ms;
    emit_status(app, "synced", Some(format_report(&report)));
    Ok(report)
}

fn run_passes(
    conn: &mut Connection,
    matcher: &MentionMatcher,
    now_ms: i64,
    report: &mut EdgeSynthReport,
) -> Result<(), String> {
    // Cheap structural mirrors. These keep #102's backfill in sync as
    // new workstream_signals / calendar_attendees / assignees show up.
    run_includes_pass(conn, report)?;
    run_attended_pass(conn, report)?;
    run_owns_pass(conn, report)?;
    run_authored_pass(conn, report)?;

    // Inference passes.
    run_replied_to_pass(conn, report)?;
    run_co_attended_pass(conn, now_ms, report)?;
    run_mentioned_pass(conn, matcher, now_ms, report)?;

    Ok(())
}

// ----- Mirror passes -------------------------------------------------------

fn run_includes_pass(conn: &mut Connection, report: &mut EdgeSynthReport) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let n = tx
        .execute(
            "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'workstream', s.workstream_id, s.kind, s.item_id, 'INCLUDES', \
                    1.0, '[]', s.added_ms, s.added_ms \
             FROM workstream_signals s \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            [],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "INCLUDES", n);
    Ok(())
}

fn run_attended_pass(conn: &mut Connection, report: &mut EdgeSynthReport) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let n = tx
        .execute(
            "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'person', ca.team_member_id, 'event', ca.event_id, 'ATTENDED', \
                    1.0, '[]', ce.start_ms, ce.start_ms \
             FROM calendar_attendees ca \
             JOIN calendar_events ce ON ce.id = ca.event_id \
             WHERE ca.team_member_id IS NOT NULL \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            [],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "ATTENDED", n);
    Ok(())
}

fn run_owns_pass(conn: &mut Connection, report: &mut EdgeSynthReport) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let n1 = tx
        .execute(
            "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'person', a.assignee_id, 'action', a.id, 'OWNS', \
                    1.0, '[]', a.created_ms, a.created_ms \
             FROM actions a WHERE a.assignee_id IS NOT NULL \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            [],
        )
        .map_err(|e| e.to_string())?;
    let n2 = tx
        .execute(
            "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'person', wa.assignee_id, 'action', wa.id, 'OWNS', \
                    1.0, '[]', wa.created_ms, wa.created_ms \
             FROM workstream_actions wa WHERE wa.assignee_id IS NOT NULL \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            [],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "OWNS", n1 + n2);
    Ok(())
}

fn run_authored_pass(conn: &mut Connection, report: &mut EdgeSynthReport) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let n = tx
        .execute(
            "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'person', (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1), \
                    'note', n.note_path, 'AUTHORED', 1.0, '[]', \
                    n.modified_ms, n.modified_ms \
             FROM notes n \
             WHERE EXISTS (SELECT 1 FROM team_members WHERE is_self = 1) \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            [],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "AUTHORED", n);
    Ok(())
}

// ----- REPLIED_TO ----------------------------------------------------------

fn run_replied_to_pass(conn: &mut Connection, report: &mut EdgeSynthReport) -> Result<(), String> {
    // Window function: within each thread_id sorted by sent_at_ms ASC,
    // emit an edge from each message to its immediate predecessor.
    // Confidence 0.7 — thread-adjacency, not header-verified. If the
    // email connector ever captures In-Reply-To, that path can write
    // the same edge at confidence 1.0 and our `max(...)` keeps it.
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let n = tx
        .execute(
            "WITH ordered AS ( \
                SELECT id, sent_at_ms, \
                       LAG(id) OVER w AS prev_id \
                FROM email_messages \
                WINDOW w AS (PARTITION BY thread_id ORDER BY sent_at_ms ASC) \
             ) \
             INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'email', id, 'email', prev_id, 'REPLIED_TO', \
                    0.7, '[]', sent_at_ms, sent_at_ms \
             FROM ordered WHERE prev_id IS NOT NULL \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) \
             DO UPDATE SET \
                confidence   = max(edges.confidence, excluded.confidence), \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            [],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "REPLIED_TO", n);
    Ok(())
}

// ----- CO_ATTENDED ---------------------------------------------------------

fn run_co_attended_pass(
    conn: &mut Connection,
    now_ms: i64,
    report: &mut EdgeSynthReport,
) -> Result<(), String> {
    // Two passes: count shared meetings via self-join, then upsert
    // both directions. Confidence scales from 0.6 (2 meetings) up to
    // 1.0 (7+). Sliding window — older co-attendance drops out
    // naturally when the window moves past it; we don't delete old
    // edges, but their confidence stops being refreshed.
    let cutoff = now_ms.saturating_sub(CO_ATTENDED_WINDOW_MS);
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let n = tx
        .execute(
            "WITH attendances AS ( \
                SELECT ca.team_member_id AS p, ce.id AS event_id, ce.start_ms \
                FROM calendar_attendees ca \
                JOIN calendar_events ce ON ce.id = ca.event_id \
                WHERE ca.team_member_id IS NOT NULL AND ce.start_ms >= ?1 \
             ), \
             pairs AS ( \
                SELECT a1.p AS a, a2.p AS b, \
                       COUNT(*) AS shared, \
                       min(a1.start_ms) AS first_ms, \
                       max(a1.start_ms) AS last_ms \
                FROM attendances a1 \
                JOIN attendances a2 ON a1.event_id = a2.event_id AND a1.p <> a2.p \
                GROUP BY a1.p, a2.p \
                HAVING shared >= ?2 \
             ) \
             INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                confidence, evidence, first_seen_ms, last_seen_ms) \
             SELECT 'person', a, 'person', b, 'CO_ATTENDED', \
                    min(1.0, 0.5 + shared * 0.1), \
                    '[]', first_ms, last_ms \
             FROM pairs \
             WHERE true \
             ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                confidence   = max(edges.confidence, excluded.confidence), \
                last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
            params![cutoff, CO_ATTENDED_MIN_MEETINGS],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "CO_ATTENDED", n);
    Ok(())
}

// ----- MENTIONED -----------------------------------------------------------

/// Pre-built mapping from a canonicalized name fragment → member ids
/// that share that fragment. Built once per synth pass.
pub(crate) struct MentionMatcher {
    /// Sorted by length DESC so longer names match first (avoids
    /// shadowing "Alice" when the body says "Alice Smith"). Each entry
    /// is (folded_name, member_id).
    needles: Vec<(String, String)>,
}

impl MentionMatcher {
    pub(crate) fn from_members(members: &[TeamMember]) -> Self {
        let mut needles: Vec<(String, String)> = Vec::new();
        for m in members {
            push_name(&mut needles, &m.display_name, &m.id);
            for a in &m.aliases {
                if a.kind == kinds::NAME {
                    push_name(&mut needles, &a.value, &m.id);
                }
            }
        }
        // Length DESC, then lex ASC for determinism. Longest match wins.
        needles.sort_by(|a, b| {
            b.0.chars()
                .count()
                .cmp(&a.0.chars().count())
                .then_with(|| a.0.cmp(&b.0))
        });
        Self { needles }
    }

    /// Run the matcher over `text` (already lowered + diacritic-folded
    /// once by caller). Returns each matched member id once, in order
    /// of first match. Skips substring matches that aren't word-bounded
    /// (e.g., "malice" doesn't match "alice").
    pub(crate) fn find_member_mentions(&self, text: &str) -> Vec<String> {
        let folded = fold_for_match(text);
        let bytes = folded.as_bytes();
        let mut seen: Vec<String> = Vec::new();
        for (needle, member_id) in &self.needles {
            if seen.contains(member_id) {
                continue;
            }
            if needle.is_empty() {
                continue;
            }
            let needle_bytes = needle.as_bytes();
            let mut start = 0;
            while start + needle_bytes.len() <= bytes.len() {
                if &bytes[start..start + needle_bytes.len()] == needle_bytes
                    && is_word_boundary(bytes, start)
                    && is_word_boundary(bytes, start + needle_bytes.len())
                {
                    seen.push(member_id.clone());
                    break;
                }
                start += 1;
            }
        }
        seen
    }
}

fn push_name(needles: &mut Vec<(String, String)>, raw: &str, member_id: &str) {
    let folded = fold_for_match(raw);
    if folded.chars().count() < 3 {
        // Short names (≤ 2 chars) generate too many false positives.
        return;
    }
    needles.push((folded, member_id.to_string()));
}

/// True if `bytes[idx]` is at a word boundary (start, end, or
/// neighbor is a non-letter/digit). Word characters are ASCII letters,
/// digits, and underscore — matches the regex \w convention. Anything
/// past that (CJK, etc.) is conservatively treated as non-word, which
/// is fine for Western-name matching in v1.
fn is_word_boundary(bytes: &[u8], idx: usize) -> bool {
    if idx == 0 || idx == bytes.len() {
        return true;
    }
    let neighbor_idx = if idx == bytes.len() { idx - 1 } else { idx };
    let prev = bytes[idx - 1];
    let next = bytes[neighbor_idx];
    !is_word_byte(prev) || !is_word_byte(next)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn run_mentioned_pass(
    conn: &mut Connection,
    matcher: &MentionMatcher,
    now_ms: i64,
    report: &mut EdgeSynthReport,
) -> Result<(), String> {
    if matcher.needles.is_empty() {
        return Ok(());
    }

    // Scan notes whose modified_ms is newer than the most recent
    // MENTIONED edge from that note, plus any note that has zero
    // MENTIONED edges so far. SQLite handles the "no edges yet" case
    // via LEFT JOIN + IS NULL.
    let note_rows: Vec<(String, String, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT n.note_path, n.title, n.modified_ms \
                 FROM notes n \
                 LEFT JOIN ( \
                    SELECT src_id, max(last_seen_ms) AS last_ms \
                    FROM edges WHERE src_kind = 'note' AND edge_kind = 'MENTIONED' \
                    GROUP BY src_id \
                 ) e ON e.src_id = n.note_path \
                 WHERE e.last_ms IS NULL OR n.modified_ms > e.last_ms",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    // Email scanning uses body_preview when body_html is absent;
    // when present, naively strip HTML tags. v1: only scan emails
    // received in the last 90 days to bound the work.
    let email_rows: Vec<(String, Option<String>, Option<String>, String, i64)> = {
        let cutoff = now_ms.saturating_sub(90 * 24 * 3600 * 1000);
        let mut stmt = conn
            .prepare(
                "SELECT e.id, e.body_html, e.body_preview, e.subject, e.sent_at_ms \
                 FROM email_messages e \
                 LEFT JOIN ( \
                    SELECT src_id, max(last_seen_ms) AS last_ms \
                    FROM edges WHERE src_kind = 'email' AND edge_kind = 'MENTIONED' \
                    GROUP BY src_id \
                 ) ed ON ed.src_id = e.id \
                 WHERE e.sent_at_ms >= ?1 AND (ed.last_ms IS NULL OR e.modified_ms > ed.last_ms)",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![cutoff], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut touched = 0u32;

    // Notes: read body from disk via std::fs (no helper exists; mirror
    // what reconcile uses).
    for (note_path, title, modified_ms) in &note_rows {
        let body = match std::fs::read_to_string(note_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // Title contributes to matches alongside the body — a person's
        // name in a note title is a strong mention signal too.
        let haystack = format!("{title}\n{body}");
        let mentions = matcher.find_member_mentions(&haystack);
        for member_id in mentions {
            let n = tx
                .execute(
                    "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                        confidence, evidence, first_seen_ms, last_seen_ms) \
                     VALUES ('note', ?1, 'person', ?2, 'MENTIONED', 0.8, '[]', ?3, ?3) \
                     ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                        last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
                    params![note_path, member_id, modified_ms],
                )
                .map_err(|e| e.to_string())?;
            touched += n as u32;
        }
    }

    for (email_id, body_html, body_preview, subject, sent_at_ms) in &email_rows {
        // Prefer body_html when present (rough HTML strip), fall back
        // to body_preview, fall back to subject only.
        let body = body_html
            .as_deref()
            .map(strip_html_tags)
            .or_else(|| body_preview.clone())
            .unwrap_or_default();
        let haystack = format!("{subject}\n{body}");
        let mentions = matcher.find_member_mentions(&haystack);
        for member_id in mentions {
            let n = tx
                .execute(
                    "INSERT INTO edges (src_kind, src_id, tgt_kind, tgt_id, edge_kind, \
                                        confidence, evidence, first_seen_ms, last_seen_ms) \
                     VALUES ('email', ?1, 'person', ?2, 'MENTIONED', 0.8, '[]', ?3, ?3) \
                     ON CONFLICT(src_kind, src_id, tgt_kind, tgt_id, edge_kind) DO UPDATE SET \
                        last_seen_ms = max(edges.last_seen_ms, excluded.last_seen_ms)",
                    params![email_id, member_id, sent_at_ms],
                )
                .map_err(|e| e.to_string())?;
            touched += n as u32;
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    bump(report, "MENTIONED", touched as usize);
    Ok(())
}

/// Quick-and-dirty HTML tag stripper. Drops anything between '<' and
/// '>'. Not a real parser; sufficient for mention scanning where
/// false-positive "matches inside a tag attribute" are very unlikely
/// for short Western names.
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// ----- Helpers -------------------------------------------------------------

fn read_last_run_ms(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![META_LAST_EDGE_SYNTH_MS],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|s| s.parse::<i64>().ok())
    .unwrap_or(0)
}

fn write_last_run_ms(conn: &Connection, ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![META_LAST_EDGE_SYNTH_MS, ms.to_string()],
    )?;
    Ok(())
}

fn bump(report: &mut EdgeSynthReport, kind: &str, n: usize) {
    *report.by_kind.entry(kind.to_string()).or_insert(0) += n as u32;
}

fn format_report(r: &EdgeSynthReport) -> String {
    if r.by_kind.is_empty() {
        return "no edges touched".to_string();
    }
    let mut parts: Vec<String> = r
        .by_kind
        .iter()
        .filter(|(_, v)| **v > 0)
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    parts.sort();
    format!("touched {} edges ({})", r.total_touched, parts.join(", "))
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_self_and_teammate(conn: &Connection) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES ('tm_self', 'Me', '', '/x/self.md', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES ('tm_alice', 'Alice Smith', '', '/x/a.md', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES ('tm_alice', 'name', 'Alice')",
            [],
        )
        .unwrap();
    }

    fn seed_connector(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES (?1, 'email', 'test', 1, '{}', 0, 0)",
            params![id],
        )
        .unwrap();
    }

    fn seed_email(conn: &Connection, id: &str, thread: &str, from: &str, sent: i64) {
        seed_connector(conn, "mg:test");
        conn.execute(
            "INSERT INTO email_messages(\
                id, connector_id, external_id, thread_id, subject, from_email, from_name, \
                sent_at_ms, body_preview, body_html, has_attachments, is_read, raw_etag, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, ?2, 'Sub', ?3, NULL, ?4, NULL, NULL, 0, 0, NULL, ?4)",
            params![id, thread, from, sent],
        )
        .unwrap();
    }

    fn seed_event_attended(conn: &Connection, id: &str, members: &[&str], start: i64) {
        seed_connector(conn, "mg:test");
        conn.execute(
            "INSERT INTO calendar_events(\
                id, connector_id, external_id, title, start_ms, end_ms, all_day, modified_ms\
             ) VALUES (?1, 'mg:test', ?1, 'Sync', ?2, ?2, 0, ?2)",
            params![id, start],
        )
        .unwrap();
        for m in members {
            conn.execute(
                "INSERT INTO calendar_attendees(event_id, email, team_member_id, is_self, is_organizer) \
                 VALUES (?1, ?2, ?3, 0, 0)",
                params![id, format!("{m}@x.io"), m],
            )
            .unwrap();
        }
    }

    fn seed_note_with_body(conn: &Connection, dir: &std::path::Path, name: &str, body: &str) -> String {
        // Write a real note file so the mention scanner can read it.
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        let path_str = path.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO notes(note_path, bundle_id, title, modified_ms, body_size) \
             VALUES (?1, 'b', ?2, 100, 0)",
            params![path_str, name],
        )
        .unwrap();
        path_str
    }

    fn count_edges(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    // SQL passes are best tested by running the same statements out-of-band
    // (no AppHandle in unit tests). The helpers below call the pure-SQL
    // pass functions directly so we exercise the actual statements.

    #[test]
    fn authored_self_to_notes() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        let tmp = tempfile::tempdir().unwrap();
        seed_note_with_body(&conn, tmp.path(), "a", "body");
        seed_note_with_body(&conn, tmp.path(), "b", "body");

        let mut report = EdgeSynthReport::default();
        run_authored_pass(&mut conn, &mut report).unwrap();

        assert_eq!(count_edges(&conn, "AUTHORED"), 2);
        let confs: Vec<f64> = conn
            .prepare("SELECT confidence FROM edges WHERE edge_kind = 'AUTHORED'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(confs.iter().all(|c| (*c - 1.0).abs() < 1e-9));
    }

    #[test]
    fn replied_to_adjacent_within_thread() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        seed_email(&conn, "msg-1", "t1", "alice@x.io", 1_000);
        seed_email(&conn, "msg-2", "t1", "me@x.io", 2_000);
        seed_email(&conn, "msg-3", "t1", "alice@x.io", 3_000);
        seed_email(&conn, "msg-4", "t1", "me@x.io", 4_000);

        let mut report = EdgeSynthReport::default();
        run_replied_to_pass(&mut conn, &mut report).unwrap();

        // 4 messages → 3 adjacent pairs.
        assert_eq!(count_edges(&conn, "REPLIED_TO"), 3);
        let (src, tgt): (String, String) = conn
            .query_row(
                "SELECT src_id, tgt_id FROM edges \
                 WHERE edge_kind = 'REPLIED_TO' ORDER BY src_id ASC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(src, "msg-2");
        assert_eq!(tgt, "msg-1");
    }

    #[test]
    fn replied_to_does_not_cross_threads() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        seed_email(&conn, "a-1", "t1", "alice@x.io", 1_000);
        seed_email(&conn, "b-1", "t2", "alice@x.io", 2_000);
        seed_email(&conn, "a-2", "t1", "me@x.io", 3_000);

        let mut report = EdgeSynthReport::default();
        run_replied_to_pass(&mut conn, &mut report).unwrap();

        // Only the t1 pair (a-1, a-2) — no cross-thread edges.
        assert_eq!(count_edges(&conn, "REPLIED_TO"), 1);
    }

    #[test]
    fn co_attended_threshold_and_directions() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        let now = current_unix_ms();

        // Two shared meetings within the window.
        seed_event_attended(&conn, "ev1", &["tm_self", "tm_alice"], now - 1_000);
        seed_event_attended(&conn, "ev2", &["tm_self", "tm_alice"], now - 2_000);

        let mut report = EdgeSynthReport::default();
        run_co_attended_pass(&mut conn, now, &mut report).unwrap();

        // Bidirectional: (self → alice) AND (alice → self).
        assert_eq!(count_edges(&conn, "CO_ATTENDED"), 2);
    }

    #[test]
    fn co_attended_under_threshold_no_edge() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        let now = current_unix_ms();
        // Only one shared meeting — below CO_ATTENDED_MIN_MEETINGS.
        seed_event_attended(&conn, "ev1", &["tm_self", "tm_alice"], now - 1_000);
        let mut report = EdgeSynthReport::default();
        run_co_attended_pass(&mut conn, now, &mut report).unwrap();
        assert_eq!(count_edges(&conn, "CO_ATTENDED"), 0);
    }

    #[test]
    fn co_attended_outside_window_ignored() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        let now = current_unix_ms();
        let old = now - CO_ATTENDED_WINDOW_MS - 1_000;
        seed_event_attended(&conn, "ev1", &["tm_self", "tm_alice"], old);
        seed_event_attended(&conn, "ev2", &["tm_self", "tm_alice"], old - 1_000);
        let mut report = EdgeSynthReport::default();
        run_co_attended_pass(&mut conn, now, &mut report).unwrap();
        assert_eq!(count_edges(&conn, "CO_ATTENDED"), 0);
    }

    #[test]
    fn mentioned_resolves_full_name_word_bounded() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        let tmp = tempfile::tempdir().unwrap();
        seed_note_with_body(
            &conn,
            tmp.path(),
            "n1",
            "Quick chat with Alice Smith about the new client. Followed up later.",
        );

        let team = team::list_team_members_raw(&conn).unwrap();
        let matcher = MentionMatcher::from_members(&team);
        let mut report = EdgeSynthReport::default();
        run_mentioned_pass(&mut conn, &matcher, current_unix_ms(), &mut report).unwrap();

        assert_eq!(count_edges(&conn, "MENTIONED"), 1);
    }

    #[test]
    fn mentioned_skips_substring_inside_word() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        let tmp = tempfile::tempdir().unwrap();
        // "malice" contains "alice" but it's NOT word-bounded.
        seed_note_with_body(&conn, tmp.path(), "n1", "no malice here");

        let team = team::list_team_members_raw(&conn).unwrap();
        let matcher = MentionMatcher::from_members(&team);
        let mut report = EdgeSynthReport::default();
        run_mentioned_pass(&mut conn, &matcher, current_unix_ms(), &mut report).unwrap();

        assert_eq!(count_edges(&conn, "MENTIONED"), 0);
    }

    #[test]
    fn mentioned_diacritic_fold() {
        let mut conn = open_db();
        // Member "Soren" should match "Sören" in the body — both
        // canonicalize to "soren" via NFD + combining-mark drop. (Note:
        // characters like "ø" / "ß" don't NFD-decompose, so they stay
        // distinct from "o" / "ss" — that's a documented limitation of
        // fold_for_match. We test the standard combining-mark case here.)
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, profile_md_path, is_self, created_ms, updated_ms) \
             VALUES ('tm_soren', 'Soren', '', '/x/s.md', 0, 0, 0)",
            [],
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        seed_note_with_body(&conn, tmp.path(), "n1", "Sören wrote back today.");

        let team = team::list_team_members_raw(&conn).unwrap();
        let matcher = MentionMatcher::from_members(&team);
        let mut report = EdgeSynthReport::default();
        run_mentioned_pass(&mut conn, &matcher, current_unix_ms(), &mut report).unwrap();

        assert_eq!(count_edges(&conn, "MENTIONED"), 1);
    }

    #[test]
    fn idempotent_rerun_preserves_first_seen() {
        let mut conn = open_db();
        seed_self_and_teammate(&conn);
        seed_email(&conn, "msg-1", "t1", "alice@x.io", 1_000);
        seed_email(&conn, "msg-2", "t1", "me@x.io", 2_000);

        let mut report = EdgeSynthReport::default();
        run_replied_to_pass(&mut conn, &mut report).unwrap();
        let first_after_1: i64 = conn
            .query_row(
                "SELECT first_seen_ms FROM edges WHERE edge_kind = 'REPLIED_TO' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Re-run; first_seen_ms must be preserved.
        let mut report2 = EdgeSynthReport::default();
        run_replied_to_pass(&mut conn, &mut report2).unwrap();
        let first_after_2: i64 = conn
            .query_row(
                "SELECT first_seen_ms FROM edges WHERE edge_kind = 'REPLIED_TO' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first_after_1, first_after_2);
        // And row count is unchanged.
        assert_eq!(count_edges(&conn, "REPLIED_TO"), 1);
    }

    #[test]
    fn strip_html_tags_basic() {
        assert_eq!(strip_html_tags("<p>hello <b>world</b></p>"), "hello world");
        assert_eq!(strip_html_tags("no tags"), "no tags");
        assert_eq!(strip_html_tags("<a href='x'>link</a>"), "link");
    }

    #[test]
    fn mention_matcher_picks_longer_first() {
        let members = vec![TeamMember {
            id: "tm".into(),
            display_name: "Alice Smith".into(),
            role: String::new(),
            aliases: vec![team::TypedAlias {
                kind: "name".into(),
                value: "Alice".into(),
            }],
            profile_md_path: String::new(),
            is_self: false,
            created_ms: 0,
            updated_ms: 0,
        }];
        let m = MentionMatcher::from_members(&members);
        // Two needles, longest ("alice smith") first.
        assert_eq!(m.needles[0].0, "alice smith");
        assert_eq!(m.needles[1].0, "alice");
    }
}
