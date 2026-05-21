//! "Recently rejected action items" block for the reconcile prompt (#148).
//!
//! Reads the `action_deletions` log (#147) and assembles a small markdown
//! block that lists action-item texts the user previously rejected from
//! similar meetings. The reconcile prompt picks this up so the LLM stops
//! re-emitting boilerplate the user already deleted.
//!
//! Sources, in priority order:
//!   1. **Same series** — deletions where `source_series_master_id`
//!      matches the meeting's series. Limit 20.
//!   2. **Same attendees** — deletions where `subject_member_id` is any
//!      of this meeting's non-self attendees. Limit 10. The attendee's
//!      display name is rendered alongside the text so the LLM has
//!      enough context to judge "resembles."
//!
//! Window: 60 days. Cause filter: `user_delete` or `user_dismiss` only —
//! `auto_resolved` is excluded because worker omissions are weak signal
//! and including them would create a feedback loop (LLM hides items →
//! worker sweeps → LLM trains to hide more).
//!
//! Dedup: normalized text (lowercased + whitespace collapsed). Series
//! entries take precedence so a text rejected via both a series
//! occurrence and an attendee-only path shows up once, under series.
//!
//! Token budget: ~800 tokens (capped at `MAX_BLOCK_CHARS` chars, ~4
//! chars/token). When the assembled block exceeds the cap, older entries
//! drop first within each section.

use rusqlite::{params_from_iter, Connection};

/// Window over which deletions count. Older user signal is stale.
const WINDOW_DAYS_MS: i64 = 60 * 24 * 60 * 60 * 1000;
/// Limit on rows pulled per source. Recency desc; older rows drop first.
const SERIES_LIMIT: usize = 20;
const ATTENDEE_LIMIT: usize = 10;
/// Rough cap on block size. ~800 tokens × ~4 chars/token.
const MAX_BLOCK_CHARS: usize = 3200;

const HEADING_INSTRUCTIONS: &str = "## Recently rejected action items

The user has previously rejected these action items extracted from \
similar meetings. Do NOT emit anything that closely resembles them. \
If you think one of these still belongs in this meeting's output, \
phrase it differently or skip it — the user's deletion is strong \
signal that the original framing was wrong.";

#[derive(Debug, Clone)]
struct SeriesEntry {
    text: String,
}

#[derive(Debug, Clone)]
struct AttendeeEntry {
    text: String,
    display_name: Option<String>,
}

/// Lowercase + collapse internal whitespace runs into single spaces.
/// Used as the dedup key — the LLM doesn't care about punctuation drift
/// but we keep punctuation to keep the key narrow (a real text change
/// like "send recap" vs "send the recap" should still be distinct).
fn normalize_for_dedup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.extend(ch.to_lowercase());
            last_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Pull series-matched deletion rows, recency desc, capped at SERIES_LIMIT.
fn fetch_series_rows(
    conn: &Connection,
    series_master_id: &str,
    window_start_ms: i64,
) -> rusqlite::Result<Vec<SeriesEntry>> {
    let mut stmt = conn.prepare(
        "SELECT text \
           FROM action_deletions \
          WHERE source_series_master_id = ?1 \
            AND cause IN ('user_delete', 'user_dismiss') \
            AND deleted_ms > ?2 \
          ORDER BY deleted_ms DESC \
          LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![series_master_id, window_start_ms, SERIES_LIMIT as i64],
        |r| {
            Ok(SeriesEntry {
                text: r.get::<_, String>(0)?,
            })
        },
    )?;
    rows.collect()
}

/// Pull attendee-matched deletion rows, joining team_members for the
/// counterparty's display name. Recency desc, capped at ATTENDEE_LIMIT.
fn fetch_attendee_rows(
    conn: &Connection,
    attendee_member_ids: &[String],
    window_start_ms: i64,
) -> rusqlite::Result<Vec<AttendeeEntry>> {
    if attendee_member_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; attendee_member_ids.len()].join(",");
    let sql = format!(
        "SELECT d.text, tm.display_name \
           FROM action_deletions d \
           LEFT JOIN team_members tm ON tm.id = d.subject_member_id \
          WHERE d.subject_member_id IN ({placeholders}) \
            AND d.cause IN ('user_delete', 'user_dismiss') \
            AND d.deleted_ms > ? \
          ORDER BY d.deleted_ms DESC \
          LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = attendee_member_ids
        .iter()
        .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    params.push(Box::new(window_start_ms));
    params.push(Box::new(ATTENDEE_LIMIT as i64));
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok(AttendeeEntry {
            text: r.get::<_, String>(0)?,
            display_name: r.get::<_, Option<String>>(1)?,
        })
    })?;
    rows.collect()
}

/// Render one line of the series section. Quotes the text so multi-line
/// LLM action items render as a single bullet (transcripts can produce
/// trailing newlines we don't want bleeding into the list).
fn render_series_line(text: &str) -> String {
    format!("- \"{}\"", text.trim().replace('\n', " "))
}

fn render_attendee_line(text: &str, display_name: Option<&str>) -> String {
    let cleaned = text.trim().replace('\n', " ");
    match display_name {
        Some(name) if !name.trim().is_empty() => format!("- \"{cleaned}\" ({})", name.trim()),
        _ => format!("- \"{cleaned}\""),
    }
}

/// Assemble the "Recently rejected action items" block for the meeting
/// identified by `series_master_id` (when recurring) + `attendee_member_ids`
/// (non-self attendees from the `## Attendees` block).
///
/// Returns `None` when no matching deletions exist — caller skips the
/// system block entirely rather than emitting a "_None._" placeholder.
pub fn build_rejected_block(
    conn: &Connection,
    series_master_id: Option<&str>,
    attendee_member_ids: &[String],
    now_ms: i64,
) -> Option<String> {
    let window_start = now_ms - WINDOW_DAYS_MS;

    let series_rows = match series_master_id {
        Some(sid) => fetch_series_rows(conn, sid, window_start).unwrap_or_else(|e| {
            eprintln!("[reconcile/rejected] series fetch failed: {e}");
            Vec::new()
        }),
        None => Vec::new(),
    };
    let attendee_rows =
        fetch_attendee_rows(conn, attendee_member_ids, window_start).unwrap_or_else(|e| {
            eprintln!("[reconcile/rejected] attendee fetch failed: {e}");
            Vec::new()
        });

    if series_rows.is_empty() && attendee_rows.is_empty() {
        return None;
    }

    // Dedup across both sources by normalized text. Series wins (a text
    // rejected in both contexts shows only under series). Within each
    // section, rows already arrived recency-desc from the queries.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let series_kept: Vec<&SeriesEntry> = series_rows
        .iter()
        .filter(|e| seen.insert(normalize_for_dedup(&e.text)))
        .collect();
    let attendee_kept: Vec<&AttendeeEntry> = attendee_rows
        .iter()
        .filter(|e| seen.insert(normalize_for_dedup(&e.text)))
        .collect();

    // Greedy assembly with a token budget. Series rows are higher
    // priority (same-series signal > attendee-only signal), so they fill
    // first; attendee rows take whatever's left. Within each section,
    // older rows drop first when the cap kicks in.
    let mut block = String::from(HEADING_INSTRUCTIONS);
    let mut included_any = false;

    if !series_kept.is_empty() {
        let mut section = String::from("\n\nRejected from this series (last 60 days):\n");
        let mut wrote_any = false;
        for entry in &series_kept {
            let line = render_series_line(&entry.text);
            if block.len() + section.len() + line.len() + 1 > MAX_BLOCK_CHARS {
                break;
            }
            section.push_str(&line);
            section.push('\n');
            wrote_any = true;
        }
        if wrote_any {
            block.push_str(section.trim_end_matches('\n'));
            included_any = true;
        }
    }

    if !attendee_kept.is_empty() {
        let mut section = String::from("\n\nRejected involving these attendees:\n");
        let mut wrote_any = false;
        for entry in &attendee_kept {
            let line = render_attendee_line(&entry.text, entry.display_name.as_deref());
            if block.len() + section.len() + line.len() + 1 > MAX_BLOCK_CHARS {
                break;
            }
            section.push_str(&line);
            section.push('\n');
            wrote_any = true;
        }
        if wrote_any {
            block.push_str(section.trim_end_matches('\n'));
            included_any = true;
        }
    }

    if !included_any {
        // Both sections got starved by the cap — emit nothing rather
        // than a header without bullets.
        return None;
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE team_members (
                 id           TEXT PRIMARY KEY,
                 display_name TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE action_deletions (
                 id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                 deleted_ms              INTEGER NOT NULL,
                 origin_kind             TEXT NOT NULL,
                 origin_synth_kind       TEXT,
                 origin_synth_id         TEXT,
                 origin_note_id          TEXT,
                 subject_member_id       TEXT,
                 assignee_id             TEXT,
                 text                    TEXT NOT NULL,
                 source_series_master_id TEXT,
                 cause                   TEXT NOT NULL DEFAULT 'user_delete'
             );",
        )
        .unwrap();
        conn
    }

    fn seed_member(conn: &Connection, id: &str, display_name: &str) {
        conn.execute(
            "INSERT INTO team_members (id, display_name) VALUES (?1, ?2)",
            params![id, display_name],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_deletion(
        conn: &Connection,
        deleted_ms: i64,
        origin_kind: &str,
        text: &str,
        subject_member_id: Option<&str>,
        source_series_master_id: Option<&str>,
        cause: &str,
    ) {
        conn.execute(
            "INSERT INTO action_deletions \
                (deleted_ms, origin_kind, subject_member_id, text, \
                 source_series_master_id, cause) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                deleted_ms,
                origin_kind,
                subject_member_id,
                text,
                source_series_master_id,
                cause
            ],
        )
        .unwrap();
    }

    #[test]
    fn build_rejected_block_returns_empty_when_no_matches() {
        let conn = open_test_db();
        let now = 1_000_000;
        assert!(build_rejected_block(&conn, None, &[], now).is_none());

        // Series id provided but no rows match it.
        assert!(
            build_rejected_block(&conn, Some("series:absent"), &[], now).is_none(),
            "non-matching series id must produce no block"
        );

        // Attendee id provided but no rows match.
        let attendees = vec!["tm:nobody".to_string()];
        assert!(
            build_rejected_block(&conn, None, &attendees, now).is_none(),
            "non-matching attendees must produce no block"
        );
    }

    #[test]
    fn build_rejected_block_groups_by_series_then_attendees() {
        let conn = open_test_db();
        seed_member(&conn, "tm:heike", "Heike Epple");
        let now = 1_000_000;
        // Series row.
        seed_deletion(
            &conn,
            now - 1_000,
            "reconcile",
            "Confirm next week's agenda",
            None,
            Some("series:weekly"),
            "user_delete",
        );
        // Attendee row.
        seed_deletion(
            &conn,
            now - 2_000,
            "synth",
            "Follow up with Heike on budget",
            Some("tm:heike"),
            None,
            "user_delete",
        );

        let attendees = vec!["tm:heike".to_string()];
        let block = build_rejected_block(&conn, Some("series:weekly"), &attendees, now).unwrap();

        // Section order matters: series first, attendees second.
        let series_idx = block.find("Rejected from this series").unwrap();
        let attendees_idx = block.find("Rejected involving these attendees").unwrap();
        assert!(series_idx < attendees_idx);

        assert!(block.contains("Confirm next week's agenda"));
        assert!(block.contains("Follow up with Heike on budget"));
        assert!(
            block.contains("(Heike Epple)"),
            "attendee row must carry the display name"
        );
    }

    #[test]
    fn build_rejected_block_dedupes_by_normalized_text() {
        let conn = open_test_db();
        seed_member(&conn, "tm:heike", "Heike Epple");
        let now = 1_000_000;
        // Same normalized text appears under series AND attendees.
        seed_deletion(
            &conn,
            now - 1_000,
            "reconcile",
            "Send recap",
            Some("tm:heike"),
            Some("series:weekly"),
            "user_delete",
        );
        seed_deletion(
            &conn,
            now - 2_000,
            "synth",
            "  Send   recap  ",
            Some("tm:heike"),
            None,
            "user_delete",
        );

        let attendees = vec!["tm:heike".to_string()];
        let block = build_rejected_block(&conn, Some("series:weekly"), &attendees, now).unwrap();

        // Series wins. The attendee section either drops the row or is
        // absent because its only row was deduped away.
        let series_section_start = block.find("Rejected from this series").unwrap();
        let series_section = &block[series_section_start..];
        // The dedup-keyed text appears exactly once in the whole block.
        assert_eq!(
            block.matches("Send recap").count() + block.matches("Send   recap").count(),
            1,
            "deduped text must appear exactly once across sections"
        );
        // And it appears in the series section, not the attendees one.
        assert!(series_section.contains("Send recap") || series_section.contains("Send   recap"));
    }

    #[test]
    fn build_rejected_block_excludes_auto_resolved_cause() {
        let conn = open_test_db();
        seed_member(&conn, "tm:heike", "Heike Epple");
        let now = 1_000_000;
        seed_deletion(
            &conn,
            now - 1_000,
            "reconcile",
            "Worker swept this",
            Some("tm:heike"),
            Some("series:weekly"),
            "auto_resolved",
        );

        let attendees = vec!["tm:heike".to_string()];
        let block = build_rejected_block(&conn, Some("series:weekly"), &attendees, now);
        assert!(
            block.is_none(),
            "auto_resolved rows must not feed the prompt"
        );
    }

    #[test]
    fn build_rejected_block_excludes_rows_outside_window() {
        let conn = open_test_db();
        let now = 100 * 24 * 60 * 60 * 1000; // 100 days
        seed_deletion(
            &conn,
            now - 90 * 24 * 60 * 60 * 1000, // 90 days ago, outside 60-day window
            "reconcile",
            "Stale item",
            None,
            Some("series:weekly"),
            "user_delete",
        );
        let block = build_rejected_block(&conn, Some("series:weekly"), &[], now);
        assert!(block.is_none(), "rows older than 60 days must be dropped");
    }

    #[test]
    fn build_rejected_block_truncates_to_token_budget() {
        let conn = open_test_db();
        let now = 1_000_000;
        // Seed SERIES_LIMIT rows with text long enough that the cap
        // forces a tail trim. ~300 chars per line × 20 rows ≈ 6000
        // chars — well over MAX_BLOCK_CHARS (3200). Most recent rows
        // survive; older ones drop.
        let long_text: String = "Confirm the alignment on the very long boilerplate action \
                                  item that the user keeps deleting because it adds no value \
                                  and we are padding this text to push past the per-line \
                                  threshold so the budget caps before all rows make it in"
            .into();
        for i in 0..SERIES_LIMIT {
            seed_deletion(
                &conn,
                now - (i as i64) * 1_000,
                "reconcile",
                &format!("{long_text} #{i:02}"),
                None,
                Some("series:weekly"),
                "user_delete",
            );
        }

        let block = build_rejected_block(&conn, Some("series:weekly"), &[], now).unwrap();
        assert!(
            block.len() <= MAX_BLOCK_CHARS,
            "assembled block {} chars must respect cap of {}",
            block.len(),
            MAX_BLOCK_CHARS
        );
        // The most recent row (#00) must be present; the oldest (#19)
        // must not, since the cap dropped tail entries first.
        assert!(block.contains("#00"));
        assert!(
            !block.contains("#19"),
            "older rows must be dropped first when capped"
        );
    }
}
