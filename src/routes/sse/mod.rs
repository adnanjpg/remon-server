#[cfg(feature = "docker")]
pub mod docker;
pub mod services;
pub mod stats;

use crate::state::AppState;

use axum::{Router, middleware, routing::get};
use futures_util::stream::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::watch;

/// Wrap an SSE stream so it ends as soon as `shutdown` flips to `true`,
/// instead of running until the client disconnects.
///
/// These streams (live stats, container/service log follows) are infinite by
/// design. Axum's graceful shutdown stops accepting new connections but then
/// waits for every in-flight response to finish; an infinite stream never
/// finishes on its own, so without this a single open SSE client blocks
/// shutdown forever and systemd SIGKILLs the process after its stop timeout
/// (which also skips `mark_clean_shutdown`, corrupting the next boot's
/// clean/unclean classification). `Pin<Box<S>>` sidesteps having to prove
/// every wrapped stream combinator is `Unpin`.
pub(crate) fn until_shutdown<S>(
    stream: S,
    shutdown: watch::Receiver<bool>,
) -> impl Stream<Item = S::Item>
where
    S: Stream + Send + 'static,
{
    let stream: Pin<Box<S>> = Box::pin(stream);
    futures_util::stream::unfold(
        (stream, shutdown),
        |(mut stream, mut shutdown)| async move {
            // Covers the race where shutdown already flipped before this stream
            // even subscribed (a freshly subscribed receiver only observes
            // *future* changes, not one that already happened).
            if *shutdown.borrow() {
                return None;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => None,
                item = stream.next() => item.map(|i| (i, (stream, shutdown))),
            }
        },
    )
}

/// SSE routes. All require authentication; `state` is passed through so the
/// auth middleware can perform the jti revocation check.
pub fn create_routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let router = Router::new()
        .route("/stats", get(stats::stream_stats))
        .route("/stats/cpu", get(stats::stream_cpu_stats))
        .route("/stats/memory", get(stats::stream_memory_stats))
        .route("/stats/disk", get(stats::stream_disk_stats))
        .route("/stats/network", get(stats::stream_network_stats))
        .route("/services/{name}/logs", get(services::stream_service_logs));

    #[cfg(feature = "docker")]
    let router = router.route(
        "/docker/containers/{id}/logs/stream",
        get(docker::stream_logs),
    );

    router.layer(middleware::from_fn_with_state(
        state,
        crate::middleware::auth_middleware,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn passes_items_through_before_shutdown() {
        let (_tx, rx) = watch::channel(false);
        let mut wrapped = Box::pin(until_shutdown(stream::iter([1, 2, 3]), rx));

        assert_eq!(wrapped.next().await, Some(1));
        assert_eq!(wrapped.next().await, Some(2));
        assert_eq!(wrapped.next().await, Some(3));
        assert_eq!(wrapped.next().await, None, "natural end still propagates");
    }

    #[tokio::test]
    async fn ends_promptly_once_shutdown_fires() {
        let (tx, rx) = watch::channel(false);
        // An infinite stream — the only thing that can stop it is the wrapper.
        let mut wrapped = Box::pin(until_shutdown(stream::repeat(1u32), rx));

        assert_eq!(wrapped.next().await, Some(1));
        tx.send(true).expect("receiver still held by the stream");
        assert_eq!(
            wrapped.next().await,
            None,
            "must end on the very next poll, not run on until disconnect"
        );
    }

    #[tokio::test]
    async fn ends_immediately_if_already_shutdown_before_first_poll() {
        // Covers the subscribe-after-flip race: a receiver created after
        // shutdown already happened never observes a *change*, only the
        // pre-check against the current value catches this.
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let mut wrapped = Box::pin(until_shutdown(stream::repeat(1u32), rx));

        assert_eq!(wrapped.next().await, None);
    }
}
