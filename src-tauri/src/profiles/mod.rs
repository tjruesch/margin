//! Profile snapshots: derived per-person profiles (#107).
//!
//! A worker recomputes a structured per-person profile from edges +
//! events + (optionally) Voyage embedding hits every 24h, or
//! on-demand via `force_recompute_profile`. The resulting
//! `body_json` lands in the `profile_snapshots` table (created by
//! migration 028); the latest row per person is "the current
//! profile". Older rows are kept for history.
//!
//! Reconcile (#48) and AI ask both read attendee/team context from
//! the latest snapshot per person instead of the obsolete
//! `team_members.profile_md_path` files (post-#112).
//!
//! Architecture:
//! - `persist` — schema row shapes, get_latest/get_latest_map,
//!   compute_dirty, insert. No I/O outside SQLite.
//! - `prompt` — PromptInputs builder, system-prompt template, JSON
//!   output schema, `source_hash` over inputs, and the
//!   `render_snapshot_excerpt` helper consumed by reconcile/ask.
//! - `worker` — 60s tick loop. Picks dirty + TTL-eligible members,
//!   builds inputs, short-circuits on stable `source_hash`, calls
//!   Anthropic, parses the JSON response, INSERTs a new row.
//! - `commands` — Tauri IPCs (`get_profile_snapshot`,
//!   `force_recompute_profile`).
//!
//! Pairs with #52 (AI-suggested observations). v1 leaves the
//! `evidence_observation_ids` array empty; v2 populates it once
//! `profile_observations` rows flow into the prompt.

pub mod commands;
pub mod persist;
pub mod prompt;
pub mod signals;
pub mod worker;

pub use persist::{
    CollaboratorScore, FocusItem, ProfileSnapshot, ProfileSnapshotBody, WorkingHours,
};
pub use worker::start as start_worker;
