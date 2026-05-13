//! Adaptive sampling — speed up the stats collector when someone is
//! watching, slow down when nobody is.
//!
//! - Every `SAMPLING_TICK_MS`, or on `sampling_wake`, check
//!   `stats_tx.receiver_count()`.
//! - Activation is immediate; Idle requires `HYSTERESIS_TICKS` to flip,
//!   so brief subscribe/unsubscribe doesn't oscillate the cadence.
//! - In Idle the interval is `base × IDLE_MULTIPLIER`; in Active it
//!   returns to `base`. Active transitions poke `collector_wake`.
//!
//! Only the stats collector is governed here. The configured base is
//! read live each tick, so `PATCH /config` takes effect within one tick.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use log::info;

use crate::state::AppState;

/// How often the sampling task re-evaluates subscriber count.
const SAMPLING_TICK_MS: u64 = 10_000;
/// Number of consecutive ticks in the opposite state required before the
/// task transitions. Two ticks (=20s) prevents flapping on momentary
/// subscribe/unsubscribe.
const HYSTERESIS_TICKS: u8 = 2;
/// Idle slowdown factor on top of the base interval.
const IDLE_MULTIPLIER: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Active,
    Idle,
}

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        run(state).await;
    });
}

async fn run(state: Arc<AppState>) {
    let mut current = Mode::Active;
    let mut consecutive_opposite: u8 = 0;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(SAMPLING_TICK_MS)) => {}
            _ = state.sampling_wake.notified() => {}
        }

        let subscribers = state.stats_tx.receiver_count();
        let observed = if subscribers > 0 {
            Mode::Active
        } else {
            Mode::Idle
        };

        let mut activated_now = false;
        if observed != current {
            if observed == Mode::Active {
                // Subscriber appeared: activate immediately so the first
                // real-time viewer doesn't wait up to HYSTERESIS_TICKS×SAMPLING_TICK_MS
                // before the fast interval kicks in.
                current = Mode::Active;
                consecutive_opposite = 0;
                activated_now = true;
                info!(
                    "Adaptive sampling → Active (stats subscribers={})",
                    subscribers
                );
            } else {
                // No subscribers: require HYSTERESIS_TICKS consecutive idle
                // observations before slowing down, to avoid flapping on
                // brief disconnects.
                consecutive_opposite = consecutive_opposite.saturating_add(1);
                if consecutive_opposite >= HYSTERESIS_TICKS {
                    current = Mode::Idle;
                    consecutive_opposite = 0;
                    info!(
                        "Adaptive sampling → Idle (stats subscribers={})",
                        subscribers
                    );
                }
            }
        } else {
            consecutive_opposite = 0;
        }

        // Always recompute the effective interval from the current base —
        // this picks up PATCH /config changes within one tick, regardless
        // of mode transitions.
        let base = state
            .effective_config
            .read()
            .await
            .collector_stats_base_interval_ms;

        let effective = match current {
            Mode::Active => base,
            Mode::Idle => base.saturating_mul(IDLE_MULTIPLIER),
        };

        state
            .collector_stats_interval_ms
            .store(effective, Ordering::Relaxed);

        if activated_now {
            state.collector_wake.notify_one();
        }
    }
}
