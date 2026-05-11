//! Adaptive sampling — speed up the stats collector when somebody is
//! actually watching, slow down when nobody is.
//!
//! Mechanism:
//! - Every `SAMPLING_TICK_MS` (10s) check `stats_tx.receiver_count()`.
//! - >0 receivers ⇒ Active (live SSE/WS subscriber present).
//! - 0 receivers ⇒ Idle (background-only mode).
//! - Hysteresis: only flip after `HYSTERESIS_TICKS` consecutive observations
//!   in the new state, so a brief subscribe/unsubscribe doesn't oscillate
//!   the collector cadence.
//! - In Idle the collector interval becomes `base × IDLE_MULTIPLIER`. In
//!   Active it returns to `base`.
//!
//! Only the *stats* collector is governed here. Process/Docker collectors
//! follow their static configured intervals because they don't have
//! persistent SSE/WS subscribers in the current routing.
//!
//! Adaptive sampling never touches the configured base. PATCH /config
//! changes the base in `EffectiveConfig`; this task reads the live base
//! every tick and recomputes the effective interval from current state,
//! so a config change shows up within one sampling tick.

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
        tokio::time::sleep(Duration::from_millis(SAMPLING_TICK_MS)).await;

        let subscribers = state.stats_tx.receiver_count();
        let observed = if subscribers > 0 { Mode::Active } else { Mode::Idle };

        if observed != current {
            if observed == Mode::Active {
                // Subscriber appeared: activate immediately so the first
                // real-time viewer doesn't wait up to HYSTERESIS_TICKS×SAMPLING_TICK_MS
                // before the fast interval kicks in.
                current = Mode::Active;
                consecutive_opposite = 0;
                info!("Adaptive sampling → Active (stats subscribers={})", subscribers);
            } else {
                // No subscribers: require HYSTERESIS_TICKS consecutive idle
                // observations before slowing down, to avoid flapping on
                // brief disconnects.
                consecutive_opposite = consecutive_opposite.saturating_add(1);
                if consecutive_opposite >= HYSTERESIS_TICKS {
                    current = Mode::Idle;
                    consecutive_opposite = 0;
                    info!("Adaptive sampling → Idle (stats subscribers={})", subscribers);
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
    }
}
