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
                    "SELECT title FROM notes WHERE note_path = ?1",
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
        "action" => conn
            .query_row(
                "SELECT text FROM actions WHERE id = ?1",
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
        _ => ref_id.to_string(),
    };
    truncate_chars(&raw, 100)
}

/// Gather every row across the five embeddable kinds whose source
/// has changed (or that's never been embedded). Returns the union as
/// a flat work-list ready for batching.
pub fn collect_work(conn: &Connection, model: &str) -> rusqlite::Result<Vec<WorkItem>> {
    let mut items: Vec<WorkItem> = Vec::new();

    // ---- notes ----
    let note_rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT n.note_path, n.title FROM notes n \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'note' AND e.ref_id = n.note_path AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL OR n.modified_ms > e.indexed_ms",
        )?;
        let rows = stmt.query_map(params![model], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (path, title) in note_rows {
        let body = read_note_body(&path);
        let text = if body.is_empty() {
            title.clone()
        } else {
            truncate_chars(&format!("{title}\n{body}"), SOURCE_CHAR_CAP)
        };
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "note".into(),
            ref_id: path,
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
    let event_rows: Vec<(String, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.description FROM calendar_events c \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'event' AND e.ref_id = c.id AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL OR c.modified_ms > e.indexed_ms",
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

    // ---- actions (unified note + synth origins; #111) ----
    let action_rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT a.id, a.text FROM actions a \
             LEFT JOIN embeddings e \
               ON e.ref_kind = 'action' AND e.ref_id = a.id AND e.model = ?1 \
             WHERE e.indexed_ms IS NULL",
        )?;
        let rows = stmt.query_map(params![model], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, text) in action_rows {
        if text.trim().is_empty() {
            continue;
        }
        items.push(WorkItem {
            ref_kind: "action".into(),
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
