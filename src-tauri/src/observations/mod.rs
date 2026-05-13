//! AI-suggested profile observations (#52).
//!
//! Reconcile emits a side-channel structured block alongside the
//! markdown body; each item lands here as a `pending` row. The user
//! reviews them from the Team detail page. Accepting an observation
//! emits an `observation_accepted` event — the #107 profile worker's
//! dirty-detection picks the person up on the next tick.

pub mod commands;
pub mod persist;
