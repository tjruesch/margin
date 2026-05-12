//! Embeddings + sqlite-vec semantic retrieval (#104).
//!
//! Every note / email / event / action / workstream gets a 1024-dim
//! Voyage embedding stored in a `vec0` virtual table. The AI ask
//! palette's `search_similar` tool queries it for meaning-based
//! retrieval (complements the structural `read_edges` tool from #103).
//!
//! Architecture:
//! - `voyage` — HTTP client for the Voyage AI embeddings endpoint.
//! - `sources` — per-ref_kind text extraction (note body, email html,
//!   etc.). Shared between the worker and the retrieve hydrator.
//! - `worker` — polling loop (15s tick) that drains rows whose source
//!   has changed since the last embedding pass.
//! - `retrieve` — `retrieve(query, opts)` helper used by the
//!   `search_similar` AI tool.
//! - `commands` — Tauri IPCs (status query + force re-index).

pub mod commands;
pub mod retrieve;
pub mod sources;
pub mod voyage;
pub mod worker;

pub use retrieve::{retrieve, RetrieveHit, RetrieveOpts};
pub use worker::start as start_worker;
