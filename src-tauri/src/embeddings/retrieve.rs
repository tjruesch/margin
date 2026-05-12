//! Semantic retrieval helper (#104). Embeds the query (input_type =
//! `query`) and runs a kNN against the `embeddings_vec` virtual table,
//! optionally filtering by ref_kind. Hits are hydrated with a one-line
//! preview via `sources::preview_for`.

use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::sources;
use super::voyage::{self, Embedder, InputType, VoyageClient};

#[derive(Debug, Clone, Default)]
pub struct RetrieveOpts {
    pub kinds: Option<Vec<String>>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrieveHit {
    pub ref_kind: String,
    pub ref_id: String,
    pub distance: f32,
    pub preview: String,
}

/// Production entry point — uses `VoyageClient` against the keychain
/// API key. Returns at most `opts.limit` hits ordered by ascending
/// cosine distance (closer first).
pub async fn retrieve(
    app: &AppHandle,
    query: &str,
    opts: RetrieveOpts,
) -> Result<Vec<RetrieveHit>, String> {
    let api_key = crate::keychain::read_voyage_api_key()
        .map_err(|e| format!("voyage key not configured: {e}"))?;
    let client = VoyageClient::new(api_key);
    retrieve_with(app, &client, query, opts).await
}

/// Testable variant — caller supplies the embedder.
pub async fn retrieve_with(
    app: &AppHandle,
    embedder: &dyn Embedder,
    query: &str,
    opts: RetrieveOpts,
) -> Result<Vec<RetrieveHit>, String> {
    let limit = if opts.limit == 0 { 10 } else { opts.limit };

    let vectors = embedder
        .embed_batch(&[query.to_string()], InputType::Query)
        .await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or_else(|| "voyage returned no query embedding".to_string())?;
    let bytes = voyage::vec_to_bytes(&query_vec);

    let conn_state = app.state::<Mutex<Connection>>();
    let c = conn_state.lock().map_err(|e| e.to_string())?;

    // vec0 returns the candidate set first; the outer SELECT then
    // filters by ref_kind and joins for ordering. The `k` parameter
    // controls how many vec0 returns from its ANN walk — we ask for
    // `limit * 3` so the post-filter has room (over-fetch in case
    // the kind filter knocks several out).
    let kinds_filter: Vec<String> = opts.kinds.unwrap_or_default();
    let knn_k = (limit as i64).saturating_mul(3).max(10);

    let raw_hits: Vec<(String, String, f32)> = if kinds_filter.is_empty() {
        let mut stmt = c
            .prepare(
                "SELECT e.ref_kind, e.ref_id, v.distance \
                 FROM embeddings_vec v \
                 JOIN embeddings e ON e.rowid = v.rowid \
                 WHERE v.embedding MATCH ?1 AND k = ?2 \
                 ORDER BY v.distance ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![&bytes, knn_k], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f32>(2)?))
            })
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    } else {
        // Build a parameterized IN(...) clause. vec0 + parameterized
        // IN doesn't compose cleanly in one prepared statement, so we
        // over-fetch then filter in Rust.
        let mut stmt = c
            .prepare(
                "SELECT e.ref_kind, e.ref_id, v.distance \
                 FROM embeddings_vec v \
                 JOIN embeddings e ON e.rowid = v.rowid \
                 WHERE v.embedding MATCH ?1 AND k = ?2 \
                 ORDER BY v.distance ASC",
            )
            .map_err(|e| e.to_string())?;
        let allowed: std::collections::HashSet<&str> =
            kinds_filter.iter().map(String::as_str).collect();
        let rows = stmt
            .query_map(params![&bytes, knn_k], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f32>(2)?))
            })
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok())
            .filter(|(k, _, _)| allowed.contains(k.as_str()))
            .collect()
    };

    let trimmed = raw_hits.into_iter().take(limit);
    let mut out: Vec<RetrieveHit> = Vec::with_capacity(limit);
    for (ref_kind, ref_id, distance) in trimmed {
        let preview = sources::preview_for(&c, &ref_kind, &ref_id);
        out.push(RetrieveHit {
            ref_kind,
            ref_id,
            distance,
            preview,
        });
    }
    Ok(out)
}
