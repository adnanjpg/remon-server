//! Sliding-window phase-timing aggregator for collector loops.
//!
//! Each collector measures wall time per phase (`refresh`, `compute`, …)
//! using `Instant::elapsed()` and pushes the microsecond sample into a
//! bounded `VecDeque`. Every N ticks we sort a copy of the buffer to
//! compute p50/p95/p99/max and emit one `info!` line per phase set.
//!
//! Why a sliding window instead of resetting after each flush:
//! a 10-minute distribution is much more useful than a 1-minute mean for
//! decisions like "did this code change reduce p99 latency?". The window
//! is bounded so memory stays small (a few KB even at 600 samples).
//!
//! Why percentiles instead of just the mean: tick timings on Windows have
//! extreme variance (sysinfo's WMI bridge competes with AV / scheduled
//! tasks), so a single mean over 30 samples can swing 10× across cycles.
//! p50 + p95 + max are stable enough to spot real regressions and ignore
//! one-off spikes.

use std::collections::VecDeque;
use std::time::Duration;

use log::debug;

pub struct TickStats {
    label: &'static str,
    phases: Vec<PhaseSamples>,
    window: usize,
    flush_every: u32,
    count_since_flush: u32,
}

struct PhaseSamples {
    name: &'static str,
    samples: VecDeque<u64>,
}

impl TickStats {
    /// `label` — human-readable name printed in log lines (e.g. "stats").
    /// `phases` — fixed phase order matching what `record()` will pass.
    /// `window` — max raw samples to retain (sliding window).
    /// `flush_every` — emit a summary line after this many records since
    /// the last flush. Decoupled from `window` so the printed window can
    /// span many flushes (stable distribution) while flushes are frequent
    /// (live visibility).
    pub fn new(
        label: &'static str,
        phases: &[&'static str],
        window: usize,
        flush_every: u32,
    ) -> Self {
        Self {
            label,
            phases: phases
                .iter()
                .map(|n| PhaseSamples {
                    name: n,
                    samples: VecDeque::with_capacity(window),
                })
                .collect(),
            window,
            flush_every,
            count_since_flush: 0,
        }
    }

    /// Record one tick's per-phase durations. `durations.len()` must
    /// match the phase count handed to `new()`; mismatch is a programmer
    /// error and will panic in debug builds (silent in release for
    /// resilience — collector hot path).
    pub fn record(&mut self, durations: &[Duration]) {
        debug_assert_eq!(
            durations.len(),
            self.phases.len(),
            "tick_timer phase-count mismatch for label='{}'",
            self.label
        );
        let n = durations.len().min(self.phases.len());
        for (phase, dur) in self.phases.iter_mut().zip(durations[..n].iter()) {
            if phase.samples.len() >= self.window {
                phase.samples.pop_front();
            }
            phase.samples.push_back(dur.as_micros() as u64);
        }
        self.count_since_flush += 1;
    }

    /// If `flush_every` ticks have elapsed since the last flush, emit one
    /// `debug!` line and reset the counter (samples persist via the
    /// sliding window). Cheap path when not flushing — a single integer
    /// compare. Emitted at `debug` so production at `info` doesn't pay
    /// per-collector percentile spam every minute.
    pub fn flush_if_needed(&mut self) {
        if self.count_since_flush < self.flush_every {
            return;
        }
        self.count_since_flush = 0;

        let window_n = self
            .phases
            .first()
            .map(|p| p.samples.len())
            .unwrap_or(0);
        if window_n == 0 {
            return;
        }

        let mut line = String::with_capacity(64 + 64 * self.phases.len());
        for (i, phase) in self.phases.iter().enumerate() {
            let mut sorted: Vec<u64> = phase.samples.iter().copied().collect();
            sorted.sort_unstable();
            let p50 = percentile(&sorted, 50);
            let p95 = percentile(&sorted, 95);
            let p99 = percentile(&sorted, 99);
            let max = sorted.last().copied().unwrap_or(0);
            if i > 0 {
                line.push(' ');
            }
            line.push_str(&format!(
                "{}={{p50={} p95={} p99={} max={}}}",
                phase.name, p50, p95, p99, max
            ));
        }
        debug!(
            "{} tick μs (window={} samples): {}",
            self.label, window_n, line
        );
    }
}

fn percentile(sorted: &[u64], p: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank, linearly interpolated index. For p=100 we'd be off
    // the end without the .min() clamp.
    let idx = ((p as f64 / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_percentile_returns_zero() {
        assert_eq!(percentile(&[], 50), 0);
    }

    #[test]
    fn single_sample_percentiles() {
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 99), 42);
    }

    #[test]
    fn percentile_indexing_stable() {
        // 10 evenly spaced values; nearest-rank rounds 0.95 * 9 = 8.55 → 9
        let s: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile(&s, 50), 6); // round(0.5 * 9) = 5 → s[5] = 6
        assert_eq!(percentile(&s, 95), 10); // round(0.95 * 9) = 9 → s[9] = 10
        assert_eq!(percentile(&s, 100), 10);
    }

    #[test]
    fn sliding_window_evicts_oldest() {
        let mut t = TickStats::new("test", &["a"], 3, 10);
        for i in 0..5u64 {
            t.record(&[Duration::from_micros(i)]);
        }
        // Window is 3, we recorded 5; should retain [2, 3, 4]
        assert_eq!(t.phases[0].samples.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    }
}
