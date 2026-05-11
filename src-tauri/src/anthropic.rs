//! Shared Anthropic API constants.
//!
//! Two callers consume the Anthropic Messages API: `ask.rs` (streaming
//! Q&A over notes) and `workstreams::synthesizer` (single-shot JSON
//! clustering pass). Centralizing the endpoint, version, and default
//! model here keeps them in lockstep when we bump or pin.

pub const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
/// Cheap, fast model for low-stakes single-shot classifiers — e.g. the
/// link-categorizer prompt that takes a URL and returns `{label, kind}`.
/// Don't use for anything that needs nuance or long-context reasoning.
pub const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
