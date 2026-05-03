//! Alert evaluator entry point.
//!
//! Thin shim that delegates to `services::alerting::evaluator`. Kept
//! here so `main.rs`'s wiring (`services::alerts::spawn(state)`) reads
//! the same as before; the actual engine lives next door under
//! `services::alerting/`.

use std::sync::Arc;

use crate::state::AppState;

pub fn spawn(state: Arc<AppState>) {
    crate::services::alerting::spawn(state);
}
