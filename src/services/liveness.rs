//! Outbound dead-man's switch.
//!
//! Every other alarm here is evaluated inside this daemon, so when the box
//! dies the evaluator dies with it and the outage is silent. This inverts the
//! direction: we ping an external service on a schedule and *absence* becomes
//! the signal. A ping means only that this loop is turning — no health verdict.
//!
//! Not to be confused with `heartbeat_checks`, the inbound mirror, where our
//! own tick is the clock and therefore stops when we do.

use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};

use crate::config::LivenessConfig;
use crate::state::AppState;

pub fn spawn(state: Arc<AppState>, config: LivenessConfig) {
    if !config.enabled() {
        debug!("liveness pinger disabled (no liveness.url configured)");
        return;
    }
    crate::supervision::supervise(
        "liveness pinger",
        tokio::spawn(async move { run(state, config).await }),
    );
}

async fn run(state: Arc<AppState>, config: LivenessConfig) {
    let url = config.url.trim().to_string();

    // No redirects, matching the notification channels.
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("liveness pinger could not build its HTTP client, disabled: {e}");
            return;
        }
    };

    info!(
        "liveness pinger started: every {}s to {}",
        config.interval_secs,
        redacted(&url)
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs));
    // A missed slot must not queue catch-up pings that all land at once.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shutdown = state.shutdown.subscribe();

    let mut failures = 0u32;
    while crate::shutdown::tick_or_stop(&mut ticker, &mut shutdown).await {
        match ping(&http, &url).await {
            Ok(()) => {
                if failures > 0 {
                    info!("liveness ping recovered after {failures} failure(s)");
                    failures = 0;
                }
            }
            // Loud once, then quiet: a dead endpoint must not add a row a
            // minute to the log an operator is reading to diagnose it.
            Err(e) => {
                failures += 1;
                if failures == 1 {
                    warn!("liveness ping to {} failed: {e}", redacted(&url));
                } else {
                    debug!("liveness ping failed ({failures} consecutive): {e}");
                }
            }
        }
    }

    // No farewell ping on the way out: a clean stop and a crash must look
    // identical from outside, or the crash becomes the worst-handled case.
    info!("liveness pinger stopped");
}

async fn ping(http: &reqwest::Client, url: &str) -> Result<(), String> {
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    Err(format!("endpoint answered {status}"))
}

/// The path is the credential on these endpoints, and logs are readable over
/// the API, so only the origin is ever written out.
fn redacted(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match u.host_str() {
            Some(host) => format!("{}://{}/…", u.scheme(), host),
            None => "<url>".to_string(),
        },
        Err(_) => "<url>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_keeps_the_origin_and_drops_the_capability_path() {
        assert_eq!(
            redacted("https://hc-ping.com/8f3a-secret-uuid"),
            "https://hc-ping.com/…"
        );
        assert_eq!(
            redacted("http://192.168.1.10:3001/api/push/abc123"),
            "http://192.168.1.10/…"
        );
        assert_eq!(redacted("not a url"), "<url>");
    }

    #[test]
    fn a_blank_url_is_disabled_and_valid() {
        for url in ["", "   "] {
            let c = LivenessConfig {
                url: url.to_string(),
                ..Default::default()
            };
            assert!(!c.enabled());
            assert!(c.validate().is_ok());
        }
    }

    /// A non-empty url is an operator asking for coverage, so every way of
    /// getting it wrong fails at boot rather than running without a pinger.
    #[test]
    fn a_configured_but_unusable_pinger_fails_validation() {
        let ok = LivenessConfig {
            url: "https://hc-ping.com/uuid".to_string(),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());

        let cases = [
            LivenessConfig {
                url: "hc-ping.com/uuid".to_string(),
                ..Default::default()
            },
            LivenessConfig {
                url: "ftp://hc-ping.com/uuid".to_string(),
                ..Default::default()
            },
            LivenessConfig {
                interval_secs: 0,
                ..ok.clone()
            },
            LivenessConfig {
                timeout_secs: 0,
                ..ok.clone()
            },
            // One hung ping would eat the next slot.
            LivenessConfig {
                interval_secs: 30,
                timeout_secs: 30,
                ..ok.clone()
            },
        ];
        for c in cases {
            assert!(c.validate().is_err(), "expected rejection for {:?}", c.url);
        }
    }
}
