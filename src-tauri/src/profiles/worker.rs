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

use rusqlite::Connection;
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
    let body = match super::prompt::call_anthropic(&api_key, &inputs).await {
        Ok(b) => b,
        Err(super::prompt::CallError::RateLimited) => {
            return Ok(RecomputeOutcome::RateLimited);
        }
        Err(super::prompt::CallError::Other(msg)) => return Err(msg),
    };

    {
        let conn_state = app.state::<std::sync::Mutex<Connection>>();
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        persist::insert_snapshot(&c, person_id, now, &body, &source_hash)
            .map_err(|e| e.to_string())?;
    }
    Ok(RecomputeOutcome::Wrote)
}
