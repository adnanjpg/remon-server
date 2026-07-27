//! Owned delivery task for notifications.
//!
//! Producers — the alert evaluator, the host-event ledger — hand a
//! [`Notification`] to [`NotifyQueue`] and return immediately. One task owns
//! the actual fan-out, so a channel that takes its full budget to fail (a Web
//! Push relay can burn 30 s) costs the producer nothing.
//!
//! Why a queue and not a `tokio::spawn` per notification: spawning gives no
//! ordering, no bound, and nothing to drain at shutdown. Fire and resolve for
//! the same rule would race, and an operator could be told a rule recovered
//! before being told it fired. A single consumer over an `mpsc` keeps
//! delivery in the order transitions happened, caps how much can pile up, and
//! gives shutdown one place to flush.
//!
//! `fanout` itself is already concurrent across channels (see
//! [`NotificationManager::fanout`]); this is about not blocking the producer.

use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::notify::{Notification, NotificationManager};
use crate::storage::repositories::AlertRepository;

/// Queue depth. Deep enough that a rule set going critical all at once still
/// fits (every label_set of every rule crossing in one tick); shallow enough
/// that a wedged relay cannot grow the backlog without bound.
const CAPACITY: usize = 256;

/// How long shutdown waits for queued notifications to go out. Bounded on
/// purpose: the process is stopping, and systemd's stop timeout has to cover
/// this plus the SSE drain plus the clean-shutdown marker.
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// A notification plus, optionally, the `alert_events` row that should be
/// marked delivered once a channel accepts it.
pub struct NotifyRequest {
    pub notification: Notification,
    /// `alert_events.id` to stamp `notified = 1` on success. `None` for
    /// notifications with no audit row of their own (host events).
    pub receipt: Option<i64>,
}

/// Producer handle. Cheap to clone; lives on `AppState`.
#[derive(Clone)]
pub struct NotifyQueue {
    tx: mpsc::Sender<NotifyRequest>,
}

impl NotifyQueue {
    /// Hand a notification to the delivery task. Never blocks and never fails
    /// the caller: a monitoring tick must not stall or abort because a relay
    /// is slow.
    ///
    /// A full queue drops the *newest* — which is what a bounded `mpsc` does
    /// naturally, and the only choice that keeps the ordering the queue exists
    /// to provide. The drop is logged and the event's `notified` stays false,
    /// so the timeline shows honestly that nobody was paged.
    pub fn dispatch(&self, notification: Notification, receipt: Option<i64>) {
        use mpsc::error::TrySendError;
        match self.tx.try_send(NotifyRequest {
            notification,
            receipt,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(req)) => warn!(
                "notification queue full ({} deep), dropped: {}",
                CAPACITY, req.notification.title
            ),
            // Shutdown closed the receiver — expected, not a fault.
            Err(TrySendError::Closed(req)) => debug!(
                "notification queue closed, dropped: {}",
                req.notification.title
            ),
        }
    }
}

/// Create the queue and its receiving end.
///
/// Split from [`spawn`] because the producer handle belongs to `AppState`
/// while the task needs the shutdown channel that `AppState` owns: build the
/// queue, construct the state with it, then start the task from the state.
pub fn channel() -> (NotifyQueue, mpsc::Receiver<NotifyRequest>) {
    let (tx, rx) = mpsc::channel(CAPACITY);
    (NotifyQueue { tx }, rx)
}

/// Start the delivery task.
///
/// The returned [`JoinHandle`] must be awaited after serving stops so queued
/// notifications get their flush; dropping it reverts to the fire-and-forget
/// behaviour this exists to replace.
pub fn spawn(
    rx: mpsc::Receiver<NotifyRequest>,
    notify: Arc<NotificationManager>,
    pool: SqlitePool,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(run(rx, notify, pool, shutdown))
}

async fn run(
    mut rx: mpsc::Receiver<NotifyRequest>,
    notify: Arc<NotificationManager>,
    pool: SqlitePool,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let req = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            req = rx.recv() => match req {
                Some(req) => req,
                // Every producer is gone; nothing more can arrive.
                None => return,
            },
        };
        deliver(&notify, &pool, req).await;
    }

    // Shutdown: refuse new work, then flush what is already queued under a
    // budget. `close()` lets in-flight `try_send`s fail fast rather than
    // queueing behind a drain that will not reach them.
    rx.close();
    let flush = async {
        let mut sent = 0usize;
        while let Some(req) = rx.recv().await {
            deliver(&notify, &pool, req).await;
            sent += 1;
        }
        sent
    };
    match tokio::time::timeout(DRAIN_BUDGET, flush).await {
        Ok(0) => {}
        Ok(sent) => info!("notification queue flushed {sent} pending on shutdown"),
        Err(_) => warn!("notification queue still draining after {DRAIN_BUDGET:?}, giving up"),
    }
}

async fn deliver(notify: &NotificationManager, pool: &SqlitePool, req: NotifyRequest) {
    let delivered = notify.fanout(&req.notification).await;

    // `notified` is only ever raised on a confirmed delivery, so every failure
    // mode — a drop, a dead channel, this process dying mid-flight — leaves it
    // false. The field is read by an operator asking "was anyone told about
    // this?", and under-claiming is the only safe direction for that question.
    if delivered > 0
        && let Some(id) = req.receipt
        && let Err(e) = AlertRepository::new(pool.clone())
            .mark_event_notified(id)
            .await
    {
        warn!("marking alert_event {id} as notified failed: {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NotificationEvent, Severity};

    fn notif(title: &str) -> Notification {
        Notification {
            title: title.to_string(),
            body: String::new(),
            severity: Severity::Warn,
            event: NotificationEvent::Fired,
        }
    }

    /// The whole reason this is a queue and not a task per notification: an
    /// operator must never be told a rule recovered before being told it
    /// fired, and independent tasks give no such guarantee.
    #[tokio::test]
    async fn delivers_in_the_order_produced() {
        let (queue, mut rx) = channel();

        queue.dispatch(notif("fired"), Some(1));
        queue.dispatch(notif("resolved"), Some(2));

        assert_eq!(rx.recv().await.expect("first").notification.title, "fired");
        assert_eq!(
            rx.recv().await.expect("second").notification.title,
            "resolved"
        );
    }

    /// A producer here is a monitoring tick, so dispatch must never block or
    /// fail it — past capacity the newest is dropped instead, which is the
    /// only policy that leaves the ordering above intact. This test also pins
    /// the signature: an `async` send would not compile in this shape.
    #[tokio::test]
    async fn a_full_queue_drops_instead_of_blocking() {
        let (queue, rx) = channel();

        for i in 0..CAPACITY + 10 {
            queue.dispatch(notif(&format!("n{i}")), None);
        }
        assert_eq!(rx.len(), CAPACITY, "the queue must stop at its bound");

        // And with no receiver at all, which is what shutdown looks like.
        drop(rx);
        queue.dispatch(notif("after close"), None);
    }
}
