//! Deterministic edge synthesizer (#103). Walks events + entity tables
//! and (re)derives the `edges` graph layer without any LLM call.
//!
//! Ships behind the same shape as the workstream synthesizer:
//! - boot-tick run on startup (after the workstream synth pass)
//! - chained after every successful workstream synth (fresh signals →
//!   new INCLUDES, new attendees → new CO_ATTENDED, etc.)
//! - on-demand via `synthesize_edges(force)` IPC
//!
//! Seven edge kinds in v1: AUTHORED, REPLIED_TO, MENTIONED, CO_ATTENDED,
//! ATTENDED, INCLUDES, OWNS. See the module-level constants for the
//! tunables (CO_ATTENDED window, mention confidence, TTL).

pub mod commands;
pub mod synthesizer;

pub use synthesizer::{maybe_run, EdgeSynthReport};
