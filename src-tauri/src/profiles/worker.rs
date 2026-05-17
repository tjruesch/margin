//! Profile snapshot worker (#107).
//!
//! Polling loop (60s tick). On each tick:
//!   1. Bail if another tick is in flight (RUNNING atomic).
//!   2. Bail if the Anthropic key isn't configured (emit needs_key).
//!   3. Bail if we're in a rate-limit backoff window.
//!   4. Pick up to BATCH_PER_TICK eligible members via
//!      `persist::ttl_eligible_members(now, 24h, force=false)`.
//!   5. For each, build PromptInputs, compare source_hash to the
//!      last snapshot. If equal — structural cache hit, skip. If
//!      different — call Anthropic, parse JSON, insert snapshot.
//!
//! `force_recompute_profile(member_id)` runs a single-person
//! recompute with `force=true`. Same code path; bypasses the TTL
//! filter and the BATCH_PER_TICK cap.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::persist::{self, ProfileSnapshot};

/// Polling cadence. Recomputes are expensive (one Anthropic call
/// per person), so 60s is generous — the TTL guard (24h per
/// person) is what actually paces things.
const TICK_INTERVAL_SECS: u64 = 60;

/// Maximum age of the latest snapshot before we'll recompute.
const PER_PERSON_TTL_MS: i64 = 24 * 3600 * 1000;

/// Worst-case spend per tick = `BATCH_PER_TICK * Anthropic call`.
/// Three keeps the bursty cost bounded; the TTL guard keeps the
/// long-run cadence honest.
const BATCH_PER_TICK: usize = 3;

const RATE_LIMIT_BACKOFF_MS: i64 = 5 * 60 * 1000;

/// Consecutive recomputes a waiting action must be absent from the
/// LLM's live output before `auto_resolve_missing` flips it `done=1`
/// (#124). A single bad LLM pass (truncated context, transient
/// hallucination) shouldn't silently drop a real ask; the counter
/// gates the flip until ~two ticks corroborate.
const AUTO_RESOLVE_THRESHOLD: i64 = 2;

static RUNNING: AtomicBool = AtomicBool::new(false);
static RATE_LIMIT_BACKOFF_UNTIL_MS: AtomicI64 = AtomicI64::new(0);

struct RunGuard;
impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}
fn try_acquire() -> Option<RunGuard> {
    if RUNNING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        Some(RunGuard)
    } else {
        None
    }
}

#[derive(Serialize, Clone)]
pub struct StatusEvent {
    pub state: String,
    pub recomputed: u32,
    pub skipped_cached: u32,
    pub remaining: u32,
    pub message: Option<String>,
}

fn emit(app: &AppHandle, ev: StatusEvent) {
    let _ = app.emit("profile-status", ev);
}

/// Outcome of one tick. Test-friendly — `run_once` returns this so
/// unit tests can assert "no key → skipped" / "one recompute" /
/// "structural cache hit" without inspecting events.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunOutcome {
    pub state: String, // "ran" | "needs_key" | "skipped" | "backoff"
    pub recomputed: u32,
    pub skipped_cached: u32,
    pub eligible: u32,
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&app, false).await {
                eprintln!("[profiles] tick failed: {e}");
            }
        }
    });
}

/// Clear any active rate-limit backoff. Called by
/// `force_recompute_profile` so the user can retry immediately
/// after correcting a key/billing issue.
pub fn clear_backoff() {
    RATE_LIMIT_BACKOFF_UNTIL_MS.store(0, Ordering::Release);
}

/// One pass over eligible members. `force=true` bypasses the per-
/// person TTL guard but still respects the concurrency + key
/// checks. Called from the polling tick and from
/// `force_recompute_profile`.
pub async fn run_once(app: &AppHandle, force: bool) -> Result<RunOutcome, String> {
    let _guard = match try_acquire() {
        Some(g) => g,
        None => {
            return Ok(RunOutcome {
                state: "skipped".into(),
                ..Default::default()
            });
        }
    };

    if crate::keychain::read_anthropic_api_key().is_err() {
        emit(
            app,
            StatusEvent {
                state: "needs_key".into(),
                recomputed: 0,
                skipped_cached: 0,
                remaining: 0,
                message: Some("Anthropic API key not configured".into()),
            },
        );
        return Ok(RunOutcome {
            state: "needs_key".into(),
            ..Default::default()
        });
    }

    let now = current_unix_ms();
    let backoff_until = RATE_LIMIT_BACKOFF_UNTIL_MS.load(Ordering::Acquire);
    if now < backoff_until && !force {
        return Ok(RunOutcome {
            state: "backoff".into(),
            ..Default::default()
        });
    }

    let candidates: Vec<String> = {
        let conn_state = app.state::<std::sync::Mutex<Connection>>();
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        persist::ttl_eligible_members(&c, now, PER_PERSON_TTL_MS, force)
            .map_err(|e| e.to_string())?
    };
    let total_eligible = candidates.len();
    let batch: Vec<String> = if force {
        candidates
    } else {
        candidates.into_iter().take(BATCH_PER_TICK).collect()
    };

    let mut outcome = RunOutcome {
        state: "ran".into(),
        eligible: total_eligible as u32,
        ..Default::default()
    };

    for person_id in &batch {
        match recompute_one(app, person_id, now).await {
            Ok(RecomputeOutcome::Wrote) => outcome.recomputed += 1,
            Ok(RecomputeOutcome::Cached) => outcome.skipped_cached += 1,
            Ok(RecomputeOutcome::RateLimited) => {
                RATE_LIMIT_BACKOFF_UNTIL_MS
                    .store(now + RATE_LIMIT_BACKOFF_MS, Ordering::Release);
                emit(
                    app,
                    StatusEvent {
                        state: "backoff".into(),
                        recomputed: outcome.recomputed,
                        skipped_cached: outcome.skipped_cached,
                        remaining: (batch.len() - outcome.recomputed as usize) as u32,
                        message: Some("rate limited; backing off".into()),
                    },
                );
                outcome.state = "backoff".into();
                return Ok(outcome);
            }
            Err(e) => {
                eprintln!("[profiles] recompute_one({person_id}) failed: {e}");
            }
        }
    }

    emit(
        app,
        StatusEvent {
            state: outcome.state.clone(),
            recomputed: outcome.recomputed,
            skipped_cached: outcome.skipped_cached,
            remaining: (total_eligible.saturating_sub(batch.len())) as u32,
            message: None,
        },
    );
    Ok(outcome)
}

/// Force a single-person recompute. Used by the IPC; clears the
/// backoff so the user can retry after fixing a key/billing issue.
pub async fn recompute_one_for_ipc(
    app: &AppHandle,
    person_id: &str,
) -> Result<ProfileSnapshot, String> {
    clear_backoff();
    let now = current_unix_ms();
    match recompute_one(app, person_id, now).await {
        Ok(RecomputeOutcome::Wrote) | Ok(RecomputeOutcome::Cached) => {
            let conn_state = app.state::<std::sync::Mutex<Connection>>();
            let c = conn_state.lock().map_err(|e| e.to_string())?;
            persist::get_latest_for_person(&c, person_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "snapshot missing after recompute".into())
        }
        Ok(RecomputeOutcome::RateLimited) => Err("Anthropic rate-limited".into()),
        Err(e) => Err(e),
    }
}

enum RecomputeOutcome {
    Wrote,
    Cached,
    RateLimited,
}

async fn recompute_one(
    app: &AppHandle,
    person_id: &str,
    now: i64,
) -> Result<RecomputeOutcome, String> {
    // Build inputs + hash. If the hash matches the last snapshot's
    // hash, skip the Anthropic call entirely.
    let inputs = super::prompt::build_prompt_inputs(app, person_id)
        .await
        .map_err(|e| format!("build_prompt_inputs: {e}"))?;
    let source_hash = super::prompt::source_hash(&inputs);

    {
        let conn_state = app.state::<std::sync::Mutex<Connection>>();
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        if let Some(prev) = persist::get_latest_for_person(&c, person_id)
            .map_err(|e| e.to_string())?
        {
            if prev.source_hash == source_hash {
                return Ok(RecomputeOutcome::Cached);
            }
        }
    }

    let api_key = crate::keychain::read_anthropic_api_key()
        .map_err(|e| format!("key: {e}"))?;
    let mut body = match super::prompt::call_anthropic(&api_key, &inputs).await {
        Ok(b) => b,
        Err(super::prompt::CallError::RateLimited) => {
            return Ok(RecomputeOutcome::RateLimited);
        }
        Err(super::prompt::CallError::Other(msg)) => return Err(msg),
    };

    // Strip hallucinated obs_ids before persist (#114). The model may
    // emit ids it wasn't given; only retain citations to observations
    // we actually fed it. Dedup-preserving-order so the JSON is stable.
    let allowed_ids: std::collections::HashSet<String> = inputs
        .get("accepted_observations")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("obs_id").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let raw = std::mem::take(&mut body.evidence_observation_ids);
    let before = raw.len();
    body.evidence_observation_ids =
        super::prompt::filter_evidence_ids(raw, &allowed_ids);
    let dropped = before - body.evidence_observation_ids.len();
    if dropped > 0 {
        eprintln!("[profiles] dropped {dropped} hallucinated obs_ids for {person_id}");
    }

    // Strip hallucinated waiting source_ref_ids before persist (#120).
    // Same pattern as the obs_id filter: the model may emit ids it
    // wasn't given. Build the allowed sets from the input candidates
    // and retain only matches; then dedup by source_ref_id (the model
    // sometimes echoes the same email twice across re-iterations).
    let allowed_from_me = waiting_ref_ids(&inputs, "/waiting_candidates/from_me");
    let allowed_for_them = waiting_ref_ids(&inputs, "/waiting_candidates/for_them");
    let dropped_fm = filter_and_dedup_waiting(&mut body.waiting_from_me, &allowed_from_me);
    let dropped_ft = filter_and_dedup_waiting(&mut body.waiting_for_them, &allowed_for_them);
    if dropped_fm + dropped_ft > 0 {
        eprintln!(
            "[profiles] dropped {}+{} hallucinated waiting refs for {person_id}",
            dropped_fm, dropped_ft
        );
    }

    // Deterministic override (#119). The schema doc asks the model
    // for last_seen_active_ms but the model has no events index —
    // overwrite from SQL so the value is always fresh and accurate.
    // Short-lived read lock; drops at the brace before the write tx.
    {
        let conn_state = app.state::<std::sync::Mutex<Connection>>();
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        body.last_seen_active_ms =
            persist::last_event_ms_for(&c, person_id).map_err(|e| e.to_string())?;
    }

    {
        let conn_state = app.state::<std::sync::Mutex<Connection>>();
        let mut c = conn_state.lock().map_err(|e| e.to_string())?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        persist::insert_snapshot(&tx, person_id, now, &body, &source_hash)
            .map_err(|e| e.to_string())?;
        // Side-channel event for the activity feed (#116). The snapshot
        // itself has no UI navigation target — the row click jumps to the
        // member's Team detail page, so ref_kind="person" + ref_id=person_id.
        crate::events::emit(
            &tx,
            now,
            "profile_snapshot_created",
            Some(person_id),
            "person",
            person_id,
            &serde_json::json!({}),
        )
        .map_err(|e| e.to_string())?;
        // Sync the LLM's waiting view into the unified `actions` table
        // (#120 follow-up). The body fields are kept for back-compat
        // and debuggability but the frontend reads from `actions`.
        sync_waiting_actions(&tx, person_id, &body, now)
            .map_err(|e| format!("sync_waiting_actions: {e}"))?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(RecomputeOutcome::Wrote)
}

/// Reconcile the LLM's current "waiting" view with the `actions`
/// table. For each WaitingItem the model emitted, ensure an action
/// row exists (skip if previously dismissed by the user). For each
/// existing waiting-action whose source_ref_id the LLM no longer
/// considers pending, auto-mark `done=1` — UNLESS the user has
/// touched it (`manual_override=1`), in which case leave alone.
fn sync_waiting_actions(
    tx: &rusqlite::Transaction<'_>,
    person_id: &str,
    body: &crate::profiles::persist::ProfileSnapshotBody,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let self_id: Option<String> = tx
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(self_id) = self_id else {
        // No self member configured yet — nothing to assign.
        return Ok(());
    };

    let mut live_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for w in &body.waiting_from_me {
        // assignee = self (you owe them), subject = person.
        if let Some(action_id) =
            upsert_waiting_action(tx, w, &self_id, person_id, now_ms)?
        {
            live_ids.insert(action_id);
        }
    }
    for w in &body.waiting_for_them {
        // assignee = person (they owe you), subject = self.
        if let Some(action_id) =
            upsert_waiting_action(tx, w, person_id, &self_id, now_ms)?
        {
            live_ids.insert(action_id);
        }
    }

    auto_resolve_missing(tx, person_id, &self_id, &live_ids, now_ms)?;
    Ok(())
}

/// Stable, deterministic action id for a worker-extracted waiting
/// item. Combines (source_kind, source_ref_id, assignee_id) so re-
/// runs hit the same row. Uses a sha256 prefix to keep the id short
/// and free of awkward source-id characters in keys.
fn waiting_action_id(source_kind: &str, source_ref_id: &str, assignee_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(source_kind.as_bytes());
    h.update(b":");
    h.update(source_ref_id.as_bytes());
    h.update(b":");
    h.update(assignee_id.as_bytes());
    let digest = h.finalize();
    format!("wait:{:x}", &digest[..8].iter().fold(0u64, |acc, b| (acc << 8) | (*b as u64)))
}

fn upsert_waiting_action(
    tx: &rusqlite::Transaction<'_>,
    w: &crate::profiles::persist::WaitingItem,
    assignee_id: &str,
    subject_member_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<String>> {
    let synth_kind = match w.source_kind.as_str() {
        "email" => "email_waiting",
        "teams" => "teams_waiting",
        "meeting" => "meeting_waiting",
        _ => return Ok(None),
    };

    // Skip if the user explicitly dismissed this source.
    let dismissed: bool = tx
        .query_row(
            "SELECT 1 FROM dismissed_action_sources \
              WHERE origin_synth_kind = ?1 \
                AND origin_synth_id = ?2 \
                AND (assignee_id = ?3 OR (assignee_id IS NULL AND ?3 IS NULL))",
            rusqlite::params![synth_kind, w.source_ref_id, assignee_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if dismissed {
        return Ok(None);
    }

    let action_id = waiting_action_id(synth_kind, &w.source_ref_id, assignee_id);

    // If a row exists AND manual_override=1, leave it alone entirely.
    let existing: Option<i64> = tx
        .query_row(
            "SELECT manual_override FROM actions WHERE id = ?1",
            rusqlite::params![action_id],
            |r| r.get(0),
        )
        .optional()?;
    if matches!(existing, Some(1)) {
        return Ok(Some(action_id));
    }

    // Otherwise upsert: keep `done` and `manual_override` at their
    // current values (0 by default for fresh rows), refresh text +
    // due/synth fields. We don't recompute `created_ms` on update.
    tx.execute(
        "INSERT INTO actions \
            (id, origin_kind, origin_note_id, origin_line, \
             origin_synth_kind, origin_synth_id, workstream_id, \
             text, done, due_ms, assignee_id, created_ms, \
             subject_member_id, manual_override) \
         VALUES (?1, 'synth', NULL, NULL, ?2, ?3, NULL, \
                 ?4, 0, NULL, ?5, ?6, ?7, 0) \
         ON CONFLICT(id) DO UPDATE SET \
            text = excluded.text, \
            origin_synth_kind = excluded.origin_synth_kind, \
            origin_synth_id = excluded.origin_synth_id, \
            subject_member_id = excluded.subject_member_id \
          WHERE manual_override = 0",
        rusqlite::params![
            action_id,
            synth_kind,
            w.source_ref_id,
            w.description,
            assignee_id,
            now_ms,
            subject_member_id,
        ],
    )?;

    Ok(Some(action_id))
}

/// For every `_waiting` action row touching this person, decide
/// whether the LLM's omission counts toward auto-resolution.
///
/// Hysteresis (#124): require `AUTO_RESOLVE_THRESHOLD` consecutive
/// omissions before flipping `done=1`. Each tick that re-emits the
/// id resets the counter; each tick that omits it bumps the counter
/// and, at the threshold, also stamps `auto_resolved_ms` so the
/// frontend can render the "Margin auto-resolved" pill + Undo.
///
/// Scope (#132): synth-origin rows ONLY. Note-origin rows (`- [ ]`
/// markdown checkboxes in user-authored notes) are intentionally not
/// auto-resolved — the user's notes are their source of truth, and
/// auto-resolving would require silently rewriting the `.md` file.
/// The pair (`undo_auto_resolved_action`) mirrors this guard.
fn auto_resolve_missing(
    tx: &rusqlite::Transaction<'_>,
    person_id: &str,
    self_id: &str,
    live_ids: &std::collections::HashSet<String>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let existing: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM actions \
              WHERE origin_kind = 'synth' \
                AND origin_synth_kind IN ('email_waiting', 'teams_waiting', 'meeting_waiting') \
                AND manual_override = 0 \
                AND done = 0 \
                AND ( \
                    (assignee_id = ?1 AND subject_member_id = ?2) \
                    OR (assignee_id = ?2 AND subject_member_id = ?1) \
                )",
        )?;
        let rows = stmt.query_map(rusqlite::params![self_id, person_id], |r| {
            r.get::<_, String>(0)
        })?;
        rows.filter_map(Result::ok).collect()
    };

    for id in existing {
        if live_ids.contains(&id) {
            // Re-emitted by the LLM this tick — reset hysteresis.
            tx.execute(
                "UPDATE actions SET auto_resolve_omissions = 0 \
                  WHERE id = ?1 AND auto_resolve_omissions > 0",
                rusqlite::params![id],
            )?;
        } else {
            // Omitted. Bump counter; flip `done` only at threshold.
            // SQLite reads `auto_resolve_omissions` as the OLD value in
            // both the SET expression and the CASE; threshold compare
            // uses (old + 1) for both columns to stay consistent.
            tx.execute(
                "UPDATE actions \
                    SET auto_resolve_omissions = auto_resolve_omissions + 1, \
                        done = CASE WHEN auto_resolve_omissions + 1 >= ?2 THEN 1 ELSE done END, \
                        auto_resolved_ms = CASE WHEN auto_resolve_omissions + 1 >= ?2 THEN ?3 ELSE auto_resolved_ms END \
                  WHERE id = ?1",
                rusqlite::params![id, AUTO_RESOLVE_THRESHOLD, now_ms],
            )?;
        }
    }
    Ok(())
}

/// Walk a waiting-candidate array under the given pointer in the
/// prompt-inputs JSON and collect the `source_ref_id` values. Used
/// by `recompute_one` to build the allow-set for validation.
fn waiting_ref_ids(
    inputs: &serde_json::Value,
    pointer: &str,
) -> std::collections::HashSet<String> {
    inputs
        .pointer(pointer)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("source_ref_id").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Drop items whose `source_ref_id` isn't in `allowed`, then dedup
/// the remainder by `source_ref_id` (first occurrence wins). Returns
/// the number of items removed.
fn filter_and_dedup_waiting(
    items: &mut Vec<crate::profiles::persist::WaitingItem>,
    allowed: &std::collections::HashSet<String>,
) -> usize {
    let before = items.len();
    items.retain(|w| allowed.contains(&w.source_ref_id));
    let mut seen = std::collections::HashSet::new();
    items.retain(|w| seen.insert(w.source_ref_id.clone()));
    before - items.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::persist::WaitingItem;
    use std::collections::HashSet;

    fn item(id: &str) -> WaitingItem {
        WaitingItem {
            description: format!("desc {id}"),
            source_kind: "email".into(),
            source_ref_id: id.into(),
            since_ms: 0,
        }
    }

    #[test]
    fn filter_drops_hallucinated_ids() {
        let mut items = vec![item("a"), item("ghost"), item("b")];
        let mut allowed = HashSet::new();
        allowed.insert("a".into());
        allowed.insert("b".into());
        let dropped = filter_and_dedup_waiting(&mut items, &allowed);
        assert_eq!(dropped, 1);
        assert_eq!(
            items.iter().map(|w| w.source_ref_id.clone()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn filter_dedups_repeats() {
        let mut items = vec![item("a"), item("b"), item("a")];
        let mut allowed = HashSet::new();
        allowed.insert("a".into());
        allowed.insert("b".into());
        filter_and_dedup_waiting(&mut items, &allowed);
        assert_eq!(items.len(), 2);
        // First occurrence wins.
        assert_eq!(items[0].source_ref_id, "a");
        assert_eq!(items[1].source_ref_id, "b");
    }

    #[test]
    fn filter_handles_empty_allowed() {
        let mut items = vec![item("a"), item("b")];
        let allowed = HashSet::new();
        let dropped = filter_and_dedup_waiting(&mut items, &allowed);
        assert_eq!(dropped, 2);
        assert!(items.is_empty());
    }

    #[test]
    fn waiting_ref_ids_extracts_from_pointer() {
        let inputs = serde_json::json!({
            "waiting_candidates": {
                "from_me": [
                    {"source_kind": "email", "source_ref_id": "e1", "since_ms": 1, "preview": "x"},
                    {"source_kind": "teams", "source_ref_id": "m2", "since_ms": 2, "preview": "y"},
                ],
                "for_them": [
                    {"source_kind": "email", "source_ref_id": "e3", "since_ms": 3, "preview": "z"},
                ],
            }
        });
        let fm = waiting_ref_ids(&inputs, "/waiting_candidates/from_me");
        assert_eq!(fm.len(), 2);
        assert!(fm.contains("e1") && fm.contains("m2"));
        let ft = waiting_ref_ids(&inputs, "/waiting_candidates/for_them");
        assert_eq!(ft.len(), 1);
        assert!(ft.contains("e3"));
    }

    // ---------- actions-table sync path (#122) -------------------------

    use crate::profiles::persist::ProfileSnapshotBody;
    use rusqlite::params;

    fn open_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_member(conn: &rusqlite::Connection, id: &str, is_self: bool) {
        conn.execute(
            "INSERT INTO team_members \
                (id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES (?1, ?1, '', ?2, 0, 0)",
            params![id, is_self as i64],
        )
        .unwrap();
    }

    fn waiting(kind: &str, ref_id: &str, desc: &str) -> WaitingItem {
        WaitingItem {
            description: desc.into(),
            source_kind: kind.into(),
            source_ref_id: ref_id.into(),
            since_ms: 1_000,
        }
    }

    fn count_actions(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0))
            .unwrap()
    }

    fn action_text(conn: &rusqlite::Connection, id: &str) -> Option<String> {
        conn.query_row(
            "SELECT text FROM actions WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn action_done(conn: &rusqlite::Connection, id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT done FROM actions WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn action_omissions(conn: &rusqlite::Connection, id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT auto_resolve_omissions FROM actions WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn action_auto_resolved_ms(conn: &rusqlite::Connection, id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT auto_resolved_ms FROM actions WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .unwrap()
        .flatten()
    }

    #[test]
    fn waiting_action_id_is_stable_across_inputs() {
        let a = waiting_action_id("teams_waiting", "msg1", "tm_self");
        let b = waiting_action_id("teams_waiting", "msg1", "tm_self");
        assert_eq!(a, b, "same inputs must produce same id");
        // Each component must influence the hash.
        assert_ne!(a, waiting_action_id("email_waiting", "msg1", "tm_self"));
        assert_ne!(a, waiting_action_id("teams_waiting", "msg2", "tm_self"));
        assert_ne!(a, waiting_action_id("teams_waiting", "msg1", "tm_alice"));
    }

    #[test]
    fn upsert_creates_new_row() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "send the file"),
            "tm_self",
            "tm_alice",
            5_000,
        )
        .unwrap()
        .expect("upsert returns id");
        tx.commit().unwrap();

        assert_eq!(count_actions(&conn), 1);
        let row: (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT origin_kind, origin_synth_kind, origin_synth_id, text, \
                        assignee_id, subject_member_id, done, manual_override \
                   FROM actions WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                        r.get(5)?, r.get(6)?, r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "synth");
        assert_eq!(row.1, "teams_waiting");
        assert_eq!(row.2, "msg1");
        assert_eq!(row.3, "send the file");
        assert_eq!(row.4, "tm_self");
        assert_eq!(row.5, "tm_alice");
        assert_eq!(row.6, 0);
        assert_eq!(row.7, 0);
    }

    #[test]
    fn upsert_is_idempotent() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let w = waiting("teams", "msg1", "x");
        upsert_waiting_action(&tx, &w, "tm_self", "tm_alice", 1_000).unwrap();
        upsert_waiting_action(&tx, &w, "tm_self", "tm_alice", 2_000).unwrap();
        tx.commit().unwrap();
        assert_eq!(count_actions(&conn), 1);
    }

    #[test]
    fn upsert_refreshes_text_when_not_overridden() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "first"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap()
        .unwrap();
        upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "second"),
            "tm_self",
            "tm_alice",
            2_000,
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(action_text(&conn, &id).as_deref(), Some("second"));
    }

    #[test]
    fn upsert_leaves_overridden_row_untouched() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "original"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap()
        .unwrap();
        tx.execute(
            "UPDATE actions SET manual_override = 1 WHERE id = ?1",
            params![id],
        )
        .unwrap();
        let id2 = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "rewritten"),
            "tm_self",
            "tm_alice",
            2_000,
        )
        .unwrap()
        .unwrap();
        tx.commit().unwrap();

        // Same id returned (so the caller can collect it into the live
        // set for auto_resolve_missing's check), but text is unchanged.
        assert_eq!(id, id2);
        assert_eq!(action_text(&conn, &id).as_deref(), Some("original"));
    }

    #[test]
    fn upsert_skips_dismissed_source() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO dismissed_action_sources \
                (origin_synth_kind, origin_synth_id, assignee_id, dismissed_ms) \
             VALUES ('teams_waiting', 'msg1', 'tm_self', 999)",
            [],
        )
        .unwrap();
        let res = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "x"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(res.is_none(), "dismissed source returns None");
        assert_eq!(count_actions(&conn), 0);
    }

    /// First omission bumps the hysteresis counter but does NOT flip
    /// `done`. A single bad LLM pass shouldn't silently lose a real
    /// outstanding ask.
    #[test]
    fn auto_resolve_increments_counter_on_first_omission() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "x"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap()
        .unwrap();
        let live = std::collections::HashSet::new();
        auto_resolve_missing(&tx, "tm_alice", "tm_self", &live, 2_000).unwrap();
        tx.commit().unwrap();

        assert_eq!(action_done(&conn, &id), Some(0));
        assert_eq!(action_omissions(&conn, &id), Some(1));
        assert_eq!(action_auto_resolved_ms(&conn, &id), None);
    }

    /// Two consecutive omissions cross the threshold — the row flips
    /// to `done=1` and `auto_resolved_ms` is stamped with the tick's
    /// `now_ms` so the frontend can render the audit pill.
    #[test]
    fn auto_resolve_flips_done_at_threshold() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "x"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap()
        .unwrap();
        let live = std::collections::HashSet::new();
        auto_resolve_missing(&tx, "tm_alice", "tm_self", &live, 2_000).unwrap();
        auto_resolve_missing(&tx, "tm_alice", "tm_self", &live, 3_000).unwrap();
        tx.commit().unwrap();

        assert_eq!(action_done(&conn, &id), Some(1));
        assert_eq!(action_auto_resolved_ms(&conn, &id), Some(3_000));
    }

    /// A re-emitted id resets the counter — partial-progress noise
    /// doesn't accumulate into an unintended flip later.
    #[test]
    fn auto_resolve_resets_counter_when_back_in_live() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "x"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap()
        .unwrap();
        let empty = std::collections::HashSet::new();
        auto_resolve_missing(&tx, "tm_alice", "tm_self", &empty, 2_000).unwrap();
        let after_first: i64 = tx
            .query_row(
                "SELECT auto_resolve_omissions FROM actions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after_first, 1, "counter must bump before re-emit");

        let mut live = std::collections::HashSet::new();
        live.insert(id.clone());
        auto_resolve_missing(&tx, "tm_alice", "tm_self", &live, 3_000).unwrap();
        tx.commit().unwrap();

        assert_eq!(action_done(&conn, &id), Some(0));
        assert_eq!(action_omissions(&conn, &id), Some(0));
        assert_eq!(action_auto_resolved_ms(&conn, &id), None);
    }

    #[test]
    fn auto_resolve_respects_manual_override() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let id = upsert_waiting_action(
            &tx,
            &waiting("teams", "msg1", "x"),
            "tm_self",
            "tm_alice",
            1_000,
        )
        .unwrap()
        .unwrap();
        tx.execute(
            "UPDATE actions SET manual_override = 1 WHERE id = ?1",
            params![id],
        )
        .unwrap();
        let live = std::collections::HashSet::new(); // intentionally empty
        auto_resolve_missing(&tx, "tm_alice", "tm_self", &live, 2_000).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            action_done(&conn, &id),
            Some(0),
            "manual_override blocks auto-resolve"
        );
    }

    #[test]
    fn sync_waiting_actions_round_trip() {
        let mut conn = open_db();
        seed_member(&conn, "tm_self", true);
        seed_member(&conn, "tm_alice", false);
        let tx = conn.transaction().unwrap();
        let body = ProfileSnapshotBody {
            waiting_from_me: vec![
                waiting("teams", "msg1", "send the file"),
                waiting("email", "em1", "reply to budget thread"),
            ],
            waiting_for_them: vec![waiting("teams", "msg2", "their architecture write-up")],
            ..Default::default()
        };
        sync_waiting_actions(&tx, "tm_alice", &body, 5_000).unwrap();
        tx.commit().unwrap();

        assert_eq!(count_actions(&conn), 3);
        let on_you: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions \
                  WHERE assignee_id = 'tm_self' AND subject_member_id = 'tm_alice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(on_you, 2);
        let on_them: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM actions \
                  WHERE assignee_id = 'tm_alice' AND subject_member_id = 'tm_self'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(on_them, 1);
    }

    #[test]
    fn sync_waiting_actions_noops_without_self_member() {
        let mut conn = open_db();
        seed_member(&conn, "tm_alice", false);
        // No is_self=1 row.
        let tx = conn.transaction().unwrap();
        let body = ProfileSnapshotBody {
            waiting_from_me: vec![waiting("teams", "msg1", "x")],
            ..Default::default()
        };
        sync_waiting_actions(&tx, "tm_alice", &body, 5_000).unwrap();
        tx.commit().unwrap();
        assert_eq!(count_actions(&conn), 0);
    }
}
