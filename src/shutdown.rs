use log::{error, info};
use tokio::sync::watch;

use crate::state::ExitIntent;

/// Resolve when it is time to stop serving.
///
/// Three ways in: the two signals a supervisor or a terminal sends, and an
/// in-process request from `/system/restart` or `/system/shutdown`. The last
/// one exists because a handler cannot end the process on its own — it has to
/// let the response go out first, and let the graceful drain run.
pub async fn signal(mut exit_intent: watch::Receiver<Option<ExitIntent>>) {
    tokio::select! {
        _ = ctrl_c() => info!("received ctrl+c, shutting down"),
        _ = terminate() => info!("received SIGTERM, shutting down"),
        intent = requested(&mut exit_intent) => {
            info!("shutdown requested via API ({intent:?})");
        }
    }
}

/// Wait for the next tick, unless the server is stopping.
///
/// The shape every periodic background loop wants: `while tick_or_stop(&mut
/// ticker, &mut shutdown).await { … }`. Returning `false` ends the loop, so a
/// collector, a rollup pass or a retention sweep does not start fresh work
/// while the process is on its way out.
///
/// These loops have nothing queued to flush — unlike the ledger and the
/// notification queue, which are awaited by `main` because they do. Here it is
/// enough that they stop promptly on their own.
pub async fn tick_or_stop(
    ticker: &mut tokio::time::Interval,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    // A receiver subscribed after the flip only ever sees *future* changes, so
    // the current value has to be checked before waiting on the next one.
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => false,
        _ = ticker.tick() => true,
    }
}

/// Resolve once a `watch<bool>` is set to true, and never on the sender going
/// away.
///
/// The owned workers (ledger, notification queue) select their flush signal
/// against their inbound queue. Letting a dropped sender resolve that branch
/// would make them close mid-run — `changed()` reports a gone sender as an
/// error, not as a value — and the rows or pages still arriving would be
/// dropped against a closed channel. Parking instead leaves the queue's own
/// disconnect as the way those tasks end.
pub(crate) async fn flagged(flag: &mut watch::Receiver<bool>) {
    if *flag.borrow() {
        return;
    }
    loop {
        if flag.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *flag.borrow() {
            return;
        }
    }
}

/// Wait for a handler to record why we are ending.
async fn requested(exit_intent: &mut watch::Receiver<Option<ExitIntent>>) -> Option<ExitIntent> {
    // `changed()` only errors when every sender is gone, which cannot happen
    // while AppState is alive — park rather than resolve, so a bug here never
    // looks like a shutdown request.
    loop {
        if exit_intent.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        let current = *exit_intent.borrow();
        if current.is_some() {
            return current;
        }
    }
}

async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("ctrl+c handler error: {}", e);
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(e) => {
            error!("failed to install SIGTERM handler: {}", e);
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn hour_ticker() -> tokio::time::Interval {
        let mut t = tokio::time::interval(Duration::from_secs(3600));
        // An interval's first tick is immediate; drop it so the tests below
        // are actually waiting on something.
        t.reset();
        t
    }

    /// The case the up-front check exists for: a loop that subscribes *after*
    /// the flag was already flipped. `changed()` only reports changes made
    /// after subscribing, so without it a late starter waits for a signal that
    /// has already been and gone — and keeps working through shutdown.
    /// Bounded so a regression fails the run instead of hanging it: the
    /// ticker these wait on is an hour out, which is the point.
    async fn stops_promptly(shutdown: &mut watch::Receiver<bool>) -> bool {
        !tokio::time::timeout(
            Duration::from_millis(500),
            tick_or_stop(&mut hour_ticker(), shutdown),
        )
        .await
        .expect("tick_or_stop should have returned without waiting for the tick")
    }

    #[tokio::test]
    async fn stops_when_shutdown_already_flipped() {
        let (tx, _rx) = watch::channel(false);
        tx.send(true).expect("flip");
        let mut late = tx.subscribe();

        assert!(stops_promptly(&mut late).await);
    }

    #[tokio::test]
    async fn stops_when_shutdown_flips_while_waiting() {
        let (tx, mut rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(true);
        });

        assert!(stops_promptly(&mut rx).await);
    }

    #[tokio::test]
    async fn ticks_while_the_server_is_running() {
        let (_tx, mut rx) = watch::channel(false);
        let mut ticker = tokio::time::interval(Duration::from_millis(1));

        assert!(tick_or_stop(&mut ticker, &mut rx).await);
    }
}
