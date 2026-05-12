//! Embedding pipeline (#104). Polls every 15s; on each tick, collects
//! every row whose source has changed since its last embedding (or
//! that's never been embedded), batches into Voyage's 128-input cap,
//! upserts results into `embeddings` + `embeddings_vec`.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::sources::{self, WorkItem};
use super::voyage::{self, Embedder, InputType, VoyageClient};

/// Polling cadence. Cheap idle path: when no rows are stale the
/// eligibility SELECTs return empty and the tick is a no-op.
const TICK_INTERVAL_SECS: u64 = 15;

/// Voyage's per-request batch cap. The HTTP client sequentially fans
/// out at this size; for the initial backfill on a fresh install the
/// total wall-clock is `total_items / 128 * round_trip_ms`.
const BATCH_SIZE: usize = 128;

/// Process-wide guard against overlapping ticks. An AtomicBool flag
/// rather than a Mutex because a std::sync::MutexGuard is !Send and
/// would break Tauri's Send requirement on async commands. Semantics
/// stay identical: if a pass is in flight, others bail without
/// blocking. `RunGuard` resets the flag on drop, so a panic inside
/// the pass doesn't permanently wedge the worker.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Unix-ms timestamp until which the worker stays idle after a 429
/// from Voyage. Set when a batch fails with "rate limited", then
/// checked at the top of each tick. Without this, the 15s polling
/// loop would slam the API with the same backlog endlessly when the
/// user's account is on Voyage's free-tier limits (3 RPM / 10K TPM
/// without a payment method on file). Cleared back to 0 by a manual
/// `force_reindex_embeddings` IPC.
static RATE_LIMIT_BACKOFF_UNTIL_MS: AtomicI64 = AtomicI64::new(0);

const RATE_LIMIT_BACKOFF_MS: i64 = 5 * 60 * 1000; // 5 min

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
    pub done: u32,
    pub remaining: u32,
    pub errored: u32,
    pub message: Option<String>,
}

fn emit(app: &AppHandle, ev: StatusEvent) {
    let _ = app.emit("embed-status", ev);
}

/// Start the polling worker. Fire-and-forget; the worker runs for the
/// lifetime of the app.
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&app).await {
                eprintln!("[embeddings] tick failed: {e}");
            }
        }
    });
}

/// Clear any active rate-limit backoff. Called by the manual reindex
/// IPC so the user can retry immediately after adding a payment method
/// without waiting for the backoff window to elapse.
pub fn clear_backoff() {
    RATE_LIMIT_BACKOFF_UNTIL_MS.store(0, Ordering::Release);
}

/// One synchronous pass: enumerate stale items, embed them in batches,
/// upsert. Public so the `force_reindex` IPC can drive it on-demand.
pub async fn run_once(app: &AppHandle) -> Result<(), String> {
    let _guard = match try_acquire() {
        Some(g) => g,
        None => return Ok(()), // another tick in flight; skip
    };

    let api_key = match crate::keychain::read_voyage_api_key() {
        Ok(k) => k,
        Err(_) => {
            emit(
                app,
                StatusEvent {
                    state: "needs_key".into(),
                    done: 0,
                    remaining: 0,
                    errored: 0,
                    message: Some("Voyage API key not configured".into()),
                },
            );
            return Ok(());
        }
    };

    let client = VoyageClient::new(api_key);
    run_pass(app, &client).await
}

/// Pass logic factored out from `run_once` so tests can inject a fake
/// `Embedder` without touching the keychain.
pub async fn run_pass(app: &AppHandle, embedder: &dyn Embedder) -> Result<(), String> {
    // Backoff guard: when Voyage 429s, we set a "don't try until T"
    // timestamp. Bail early so the worker doesn't burn ticks (or the
    // API's IP-level rate limit) hammering a known-failing endpoint.
    let now = current_unix_ms();
    let backoff_until = RATE_LIMIT_BACKOFF_UNTIL_MS.load(Ordering::Acquire);
    if now < backoff_until {
        let remaining = (backoff_until - now) / 1000;
        emit(
            app,
            StatusEvent {
                state: "rate_limited".into(),
                done: 0,
                remaining: 0,
                errored: 0,
                message: Some(format!(
                    "Voyage rate-limited; backing off for ~{remaining}s. \
                     If this persists, add a payment method at https://dashboard.voyageai.com/."
                )),
            },
        );
        return Ok(());
    }

    let conn_state = app.state::<Mutex<Connection>>();

    // Phase 1: collect work + skip unchanged-hash rows (under the lock).
    let work: Vec<WorkItem> = {
        let c = conn_state.lock().map_err(|e| e.to_string())?;
        let raw = sources::collect_work(&c, voyage::MODEL).map_err(|e| e.to_string())?;
        sources::drop_unchanged(&c, voyage::MODEL, raw).map_err(|e| e.to_string())?
    };

    if work.is_empty() {
        emit(
            app,
            StatusEvent {
                state: "idle".into(),
                done: 0,
                remaining: 0,
                errored: 0,
                message: None,
            },
        );
        return Ok(());
    }

    let total = work.len() as u32;
    emit(
        app,
        StatusEvent {
            state: "syncing".into(),
            done: 0,
            remaining: total,
            errored: 0,
            message: None,
        },
    );

    let mut done = 0u32;
    let mut errored = 0u32;
    let mut last_error: Option<String> = None;

    // Phase 2: batched embedding + upsert. Sequential batches; lock is
    // re-acquired per batch so other writers aren't blocked for the
    // whole pass.
    for chunk in work.chunks(BATCH_SIZE) {
        let texts: Vec<String> = chunk.iter().map(|w| w.text.clone()).collect();
        let vectors = match embedder.embed_batch(&texts, InputType::Document).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[embeddings] batch failed: {e}");
                let is_rate_limit = e.contains("rate limit");
                errored += chunk.len() as u32;
                last_error = Some(e.clone());
                emit(
                    app,
                    StatusEvent {
                        state: if is_rate_limit { "rate_limited" } else { "syncing" }
                            .into(),
                        done,
                        remaining: total - done - errored,
                        errored,
                        message: Some(e),
                    },
                );
                if is_rate_limit {
                    // No point continuing the pass — subsequent batches
                    // will hit the same limit. Park future ticks for a
                    // few minutes so we stop hammering.
                    let until = current_unix_ms() + RATE_LIMIT_BACKOFF_MS;
                    RATE_LIMIT_BACKOFF_UNTIL_MS.store(until, Ordering::Release);
                    break;
                }
                continue;
            }
        };

        // Explicit scope drops the MutexGuard before the next `.await`
        // in the loop — Tokio requires futures to be Send across await
        // points, and std::sync::MutexGuard is !Send.
        let now_ms = current_unix_ms();
        {
            let mut c = conn_state.lock().map_err(|e| e.to_string())?;
            let tx = c.transaction().map_err(|e| e.to_string())?;
            for (item, vec) in chunk.iter().zip(vectors.iter()) {
                upsert_one(&tx, item, vec, now_ms).map_err(|e| e.to_string())?;
                done += 1;
            }
            tx.commit().map_err(|e| e.to_string())?;
        }

        emit(
            app,
            StatusEvent {
                state: "syncing".into(),
                done,
                remaining: total - done - errored,
                errored,
                message: None,
            },
        );
    }

    emit(
        app,
        StatusEvent {
            state: "idle".into(),
            done,
            remaining: 0,
            errored,
            // Surface the last batch error so the Settings pill can
            // show it — otherwise users see "527 errored" with no clue
            // what went wrong. Cleared when the next pass succeeds.
            message: last_error,
        },
    );
    Ok(())
}

/// Insert/update one (embeddings, embeddings_vec) row pair. Uses
/// SQLite UPSERT semantics so the rowid is preserved across re-embeds.
fn upsert_one(
    tx: &rusqlite::Transaction<'_>,
    item: &WorkItem,
    vector: &[f32],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let bytes = voyage::vec_to_bytes(vector);

    // Resolve rowid via SELECT then INSERT OR REPLACE. We can't use
    // RETURNING after INSERT because of the vec0 sidecar table — we
    // need the rowid to know which vec0 row to write.
    let existing_rowid: Option<i64> = tx
        .query_row(
            "SELECT rowid FROM embeddings WHERE ref_kind = ?1 AND ref_id = ?2 AND model = ?3",
            params![&item.ref_kind, &item.ref_id, voyage::MODEL],
            |r| r.get(0),
        )
        .ok();

    let rowid = if let Some(rid) = existing_rowid {
        tx.execute(
            "UPDATE embeddings SET source_hash = ?2, indexed_ms = ?3 WHERE rowid = ?1",
            params![rid, &item.source_hash, now_ms],
        )?;
        rid
    } else {
        tx.execute(
            "INSERT INTO embeddings (ref_kind, ref_id, model, source_hash, indexed_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &item.ref_kind,
                &item.ref_id,
                voyage::MODEL,
                &item.source_hash,
                now_ms,
            ],
        )?;
        tx.last_insert_rowid()
    };

    // vec0 doesn't support UPSERT; delete+insert is the idiomatic
    // "replace" path for this virtual table.
    tx.execute(
        "DELETE FROM embeddings_vec WHERE rowid = ?1",
        params![rowid],
    )?;
    tx.execute(
        "INSERT INTO embeddings_vec(rowid, embedding) VALUES (?1, ?2)",
        params![rowid, &bytes],
    )?;
    Ok(())
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
    use async_trait::async_trait;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Deterministic test embedder: returns a 1024-dim one-hot vector
    /// where the hot index is `hash(text) % 1024`. Distinct texts
    /// produce orthogonal vectors → distance ranking is predictable.
    struct FakeEmbedder {
        log: Mutex<Vec<String>>,
    }
    impl FakeEmbedder {
        fn new() -> Self {
            Self { log: Mutex::new(Vec::new()) }
        }
        fn log_count(&self) -> usize {
            self.log.lock().unwrap().len()
        }
    }
    #[async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed_batch(
            &self,
            texts: &[String],
            _input_type: InputType,
        ) -> Result<Vec<Vec<f32>>, String> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                self.log.lock().unwrap().push(t.clone());
                let mut v = vec![0.0f32; voyage::VEC_DIM];
                let h = sha2_hash_u64(t);
                v[(h as usize) % voyage::VEC_DIM] = 1.0;
                out.push(v);
            }
            Ok(out)
        }
    }

    fn sha2_hash_u64(s: &str) -> u64 {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        let d = h.finalize();
        u64::from_be_bytes(d[0..8].try_into().unwrap())
    }

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_note(conn: &Connection, path: &str, title: &str, modified_ms: i64) {
        conn.execute(
            "INSERT INTO notes(note_path, bundle_id, title, modified_ms, body_size) \
             VALUES (?1, 'b', ?2, ?3, 0)",
            params![path, title, modified_ms],
        )
        .unwrap();
    }

    #[test]
    fn collect_work_picks_up_unindexed_note() {
        let conn = open_db();
        seed_note(&conn, "/n/a/note.md", "Plan Q3", 100);
        let work = sources::collect_work(&conn, voyage::MODEL).unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].ref_kind, "note");
        assert_eq!(work[0].ref_id, "/n/a/note.md");
    }

    #[test]
    fn drop_unchanged_skips_matching_hash() {
        let conn = open_db();
        seed_note(&conn, "/n/a/note.md", "Plan Q3", 100);
        let work = sources::collect_work(&conn, voyage::MODEL).unwrap();
        assert_eq!(work.len(), 1);
        let hash = work[0].source_hash.clone();

        // Pretend we already embedded with the SAME hash.
        conn.execute(
            "INSERT INTO embeddings(ref_kind, ref_id, model, source_hash, indexed_ms) \
             VALUES ('note', '/n/a/note.md', ?1, ?2, 50)",
            params![voyage::MODEL, &hash],
        )
        .unwrap();

        // collect_work won't even see it (LEFT JOIN gated on
        // modified_ms > indexed_ms, and we set indexed_ms < modified_ms
        // so it WILL appear) — drop_unchanged catches the duplicate.
        let raw = sources::collect_work(&conn, voyage::MODEL).unwrap();
        let filtered = sources::drop_unchanged(&conn, voyage::MODEL, raw).unwrap();
        assert_eq!(filtered.len(), 0, "unchanged-hash rows skipped");
    }

    #[test]
    fn vec0_round_trip() {
        // Direct SQL test that vec0 is actually loaded and accepts
        // 1024-float vectors. Critical sanity check.
        let conn = open_db();
        let v: Vec<f32> = (0..voyage::VEC_DIM).map(|i| i as f32 / 1024.0).collect();
        let bytes = voyage::vec_to_bytes(&v);
        conn.execute(
            "INSERT INTO embeddings_vec(rowid, embedding) VALUES (1, ?1)",
            params![&bytes],
        )
        .unwrap();
        // Query back with MATCH against the same vector → distance ~0.
        let dist: f32 = conn
            .query_row(
                "SELECT distance FROM embeddings_vec WHERE embedding MATCH ?1 AND k = 1",
                params![&bytes],
                |r| r.get(0),
            )
            .unwrap();
        assert!(dist.abs() < 1e-4, "self-match distance should be ~0, got {dist}");
    }

    #[test]
    fn fake_embedder_round_trip() {
        // Manually run the worker's transaction path with a fake
        // embedder; verify rows land in both tables.
        let mut conn = open_db();
        seed_note(&conn, "/n/a/note.md", "Hyundai POC", 100);

        let work = sources::collect_work(&conn, voyage::MODEL).unwrap();
        assert_eq!(work.len(), 1);

        let fake = FakeEmbedder::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let texts: Vec<String> = work.iter().map(|w| w.text.clone()).collect();
        let vectors = runtime
            .block_on(fake.embed_batch(&texts, InputType::Document))
            .unwrap();
        assert_eq!(fake.log_count(), 1);

        let tx = conn.transaction().unwrap();
        upsert_one(&tx, &work[0], &vectors[0], 12345).unwrap();
        tx.commit().unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let vec_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 1);
    }

    #[test]
    fn re_embed_updates_indexed_ms_and_preserves_rowid() {
        let mut conn = open_db();
        seed_note(&conn, "/n/a/note.md", "First", 100);

        let work = sources::collect_work(&conn, voyage::MODEL).unwrap();
        let fake = FakeEmbedder::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let texts: Vec<String> = work.iter().map(|w| w.text.clone()).collect();
        let vectors = runtime
            .block_on(fake.embed_batch(&texts, InputType::Document))
            .unwrap();
        let tx = conn.transaction().unwrap();
        upsert_one(&tx, &work[0], &vectors[0], 1000).unwrap();
        tx.commit().unwrap();
        let first_rowid: i64 = conn
            .query_row("SELECT rowid FROM embeddings", [], |r| r.get(0))
            .unwrap();

        // Pretend the note got modified — same path, new title content.
        conn.execute(
            "UPDATE notes SET title = 'Second', modified_ms = 2000 WHERE note_path = '/n/a/note.md'",
            [],
        )
        .unwrap();
        let work2 = sources::collect_work(&conn, voyage::MODEL).unwrap();
        assert_eq!(work2.len(), 1);
        let v2 = runtime
            .block_on(fake.embed_batch(&[work2[0].text.clone()], InputType::Document))
            .unwrap();
        let tx = conn.transaction().unwrap();
        upsert_one(&tx, &work2[0], &v2[0], 2500).unwrap();
        tx.commit().unwrap();

        // Same row updated; rowid preserved.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let updated_rowid: i64 = conn
            .query_row("SELECT rowid FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(updated_rowid, first_rowid);
        let indexed_ms: i64 = conn
            .query_row("SELECT indexed_ms FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexed_ms, 2500);
    }
}
