//! Per-ref_kind text extraction for embeddings (#104). Shared between
//! the worker (build the corpus to embed) and the retriever (hydrate
//! one-line previews for hits).

use rusqlite::{params, Connection, OptionalExtension};

/// Hard cap per source text — well within Voyage's per-input limit and
/// keeps embedding cost predictable.
pub const SOURCE_CHAR_CAP: usize = 8000;

/// One row to (re)embed. `text` is already trimmed + capped.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub ref_kind: String,
    pub ref_id: String,
    pub text: String,
    pub source_hash: String,
}

/// Strip HTML tags + collapse whitespace. Mirrors the helper used by
/// the edges synthesizer (`strip_html_tags`) — kept local so future
/// changes there don't accidentally change embedding behavior.
pub fn strip_html(html: &str) -> String {
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

pub fn truncate_chars(s: &str, cap: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= cap {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(cap - 1).collect();
    format!("{cut}…")
}

pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Best-effort full read of a note body from disk. Returns "" on
/// failure; callers should skip empty bodies.
pub fn read_note_body(note_path: &str) -> String {
    let raw = match std::fs::read_to_string(note_path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let (_yaml, body) = crate::notes::split_frontmatter(&raw);
    truncate_chars(body, SOURCE_CHAR_CAP)
}

/// One-line preview for a `(ref_kind, ref_id)` hit. Used by
/// `search_similar` to give the model something readable per hit.
/// Returns the raw id when the source can't be resolved.
pub fn preview_for(conn: &Connection, ref_kind: &str, ref_id: &str) -> String {
    let raw = match ref_kind {
        "note" => {
            let title: Option<String> = conn
                .query_row(
                    "SELECT title FROM notes WHERE id = ?1",
                    params![ref_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            title.unwrap_or_else(|| ref_id.to_string())
        }
        "email" => conn
            .query_row(
                "SELECT subject FROM email_messages WHERE id = ?1",
                params![ref_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap_or(None)
            .unwrap_or_else(|| ref_id.to_string()),
        "event" => conn
            .query_row(
                "SELECT title FROM calendar_events WHERE id = ?1",
                params![ref_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap_or(None)
            .unwrap_or_else(|| ref_id.to_string()),
        "workstream" => conn
            .query_row(
                "SELECT title FROM workstreams WHERE id = ?1",
                params![ref_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap_or(None)
            .unwrap_or_else(|| ref_id.to_string()),
        "teams_message" => {
            // Prefer chat_topic; fall back to a body-preview snippet so
            // the AI ask result is human-readable.
            let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT chat_topic, from_name, body_preview FROM teams_messages WHERE id = ?1",
                    params![ref_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .unwrap_or(None);
            match row {
                Some((Some(topic), _, body)) if !topic.is_empty() => {
                    format!("{topic} — {}", body.unwrap_or_default())
                }
                Some((_, Some(from), Some(body))) => format!("{from}: {body}"),
                Some((_, Some(from), None)) => from,
                Some((_, None, Some(body))) => body,
                _ => ref_id.to_string(),
            }
        }
        "github" => {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT repo, title FROM github_contributions WHERE id = ?1",
                    params![ref_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap_or(None);
            match row {
                Some((repo, title)) => format!("{repo}: {title}"),
                None => ref_id.to_string(),
            }
        }
        _ => ref_id.to_string(),
    };
    truncate_chars(&raw, 100)
}

/// Gather every row across the five embeddable kinds whose source
/// has changed (or that's never been embedded). Returns the union as
/// a flat work-list ready for batching.
pub fn collect_work(conn: &Connection, model: &str) -> rusqlite::Result<Vec<WorkItem>> {
    let mut items: Vec<WorkItem> = Vec::new();

    // ---- notes (#112: body now lives in the DB) ----
    let note_rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.title, n.body_md FROM notes n \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'note' AND e.ref_id = n.id AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL OR n.modified_ms > e.indexed_ms",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, title, body) in note_rows {
        let body_capped = truncate_chars(&body, SOURCE_CHAR_CAP);
        let text = if body_capped.is_empty() {
            title.clone()
        } else {
            truncate_chars(&format!("{title}\n{body_capped}"), SOURCE_CHAR_CAP)
        };
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "note".into(),
            ref_id: id,
            source_hash: sha256_hex(&text),
            text,
        });
    }

    // ---- emails ----
    let email_rows: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT e.id, e.subject, e.body_html, e.body_preview FROM email_messages e \
             LEFT JOIN embeddings em \
               ON em.ref_kind = 'email' AND em.ref_id = e.id AND em.model = ?1 \
             WHERE em.indexed_ms IS NULL OR e.modified_ms > em.indexed_ms",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, subject, body_html, body_preview) in email_rows {
        let body = body_html
            .as_deref()
            .map(strip_html)
            .or(body_preview)
            .unwrap_or_default();
        let text = truncate_chars(&format!("{subject}\n{body}"), SOURCE_CHAR_CAP);
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "email".into(),
            ref_id: id,
            source_hash: sha256_hex(&text),
            text,
        });
    }

    // ---- events ----
    // For recurring series (#109) embed only the earliest occurrence
    // — Voyage was previously embedding 50+ near-identical rows per
    // year for a weekly 1:1 with a stable title. The subquery uses
    // `idx_events_series` (partial index) so it stays cheap.
    let event_rows: Vec<(String, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.description FROM calendar_events c \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'event' AND e.ref_id = c.id AND e.model = ?1 \
             WHERE (e.indexed_ms IS NULL OR c.modified_ms > e.indexed_ms) \
               AND ( \
                    c.series_master_id IS NULL \
                 OR c.start_ms = ( \
                        SELECT MIN(c2.start_ms) FROM calendar_events c2 \
                         WHERE c2.series_master_id = c.series_master_id \
                    ) \
               )",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, title, description) in event_rows {
        let desc = description.unwrap_or_default();
        let text = truncate_chars(&format!("{title}\n{desc}"), SOURCE_CHAR_CAP);
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "event".into(),
            ref_id: id,
            source_hash: sha256_hex(&text),
            text,
        });
    }

    // ---- teams messages (#105) ----
    let teams_rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.chat_topic, t.body_html, t.body_preview FROM teams_messages t \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'teams_message' AND e.ref_id = t.id AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL OR t.modified_ms > e.indexed_ms",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, topic, body_html, body_preview) in teams_rows {
        let body = body_html
            .as_deref()
            .map(strip_html)
            .or(body_preview)
            .unwrap_or_default();
        let header = topic.unwrap_or_else(|| "Teams message".to_string());
        let text = truncate_chars(&format!("{header}\n{body}"), SOURCE_CHAR_CAP);
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "teams_message".into(),
            ref_id: id,
            source_hash: sha256_hex(&text),
            text,
        });
    }

    // ---- workstreams ----
    let ws_rows: Vec<(String, String, String, Option<String>, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT w.id, w.title, w.summary, w.user_notes, w.updated_ms FROM workstreams w \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'workstream' AND e.ref_id = w.id AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL OR w.updated_ms > e.indexed_ms",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, title, summary, user_notes, _updated) in ws_rows {
        let notes = user_notes.unwrap_or_default();
        let text = truncate_chars(&format!("{title}\n{summary}\n{notes}"), SOURCE_CHAR_CAP);
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "workstream".into(),
            ref_id: id,
            source_hash: sha256_hex(&text),
            text,
        });
    }

    // ---- github contributions (#165) ----
    let gh_rows: Vec<(String, String, Option<String>, String)> = {
        let mut stmt = conn.prepare(
            "SELECT g.id, g.title, g.body, g.repo FROM github_contributions g \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'github' AND e.ref_id = g.id AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL OR g.modified_ms > e.indexed_ms",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, title, body, repo) in gh_rows {
        let body = body.unwrap_or_default();
        let text = truncate_chars(&format!("{repo}: {title}\n{body}"), SOURCE_CHAR_CAP);
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "github".into(),
            ref_id: id,
            source_hash: sha256_hex(&text),
            text,
        });
    }

    Ok(items)
}

/// Skip work items whose `source_hash` matches what's already stored —
/// avoids re-embedding text that hasn't changed even if `modified_ms`
/// bumped (e.g., a note was touched but its body is identical).
pub fn drop_unchanged(conn: &Connection, model: &str, items: Vec<WorkItem>) -> rusqlite::Result<Vec<WorkItem>> {
    if items.is_empty() {
        return Ok(items);
    }
    let mut stmt = conn.prepare(
        "SELECT source_hash FROM embeddings \
         WHERE ref_kind = ?1 AND ref_id = ?2 AND model = ?3",
    )?;
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let existing: Option<String> = stmt
            .query_row(
                params![&item.ref_kind, &item.ref_id, model],
                |r| r.get(0),
            )
            .optional()?;
        if existing.as_deref() == Some(item.source_hash.as_str()) {
            continue;
        }
        kept.push(item);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES ('mg:test', 'microsoft_graph', 'Test', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn seed_event(
        conn: &Connection,
        id: &str,
        start: i64,
        title: &str,
        series: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO calendar_events(\
                id, connector_id, external_id, title, start_ms, end_ms, all_day, modified_ms, \
                series_master_id\
             ) VALUES (?1, 'mg:test', ?1, ?2, ?3, ?3, 0, ?3, ?4)",
            params![id, title, start, series],
        )
        .unwrap();
    }

    /// Three occurrences of one series → only the earliest is in the
    /// work set (#109). The other two are deduped by the
    /// series_master_id subquery in `collect_work`.
    #[test]
    fn embeddings_collect_work_skips_recurring_occurrences() {
        let conn = open_db();
        seed_event(&conn, "occ1", 1_000, "Weekly standup", Some("mg:test::master-1"));
        seed_event(&conn, "occ2", 2_000, "Weekly standup", Some("mg:test::master-1"));
        seed_event(&conn, "occ3", 3_000, "Weekly standup", Some("mg:test::master-1"));
        // A one-off meeting comes through unchanged.
        seed_event(&conn, "oneoff", 4_000, "Hyundai sync", None);

        let work = collect_work(&conn, "voyage-3").unwrap();
        let event_ids: Vec<&str> = work
            .iter()
            .filter(|w| w.ref_kind == "event")
            .map(|w| w.ref_id.as_str())
            .collect();
        let mut sorted = event_ids;
        sorted.sort();
        assert_eq!(sorted, vec!["occ1", "oneoff"]);
    }
}
