use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{self, Stream, StreamExt};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::BroadcastStream;

use crate::{
    models::stats::{AllStats, StatsEvent},
    state::AppState,
};

/// Unified live stats stream — primes from the cached snapshot, then
/// chains the live broadcast.
pub async fn stream_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.stats_tx.subscribe();

    let cached = state.stats_latest.read().await.clone();
    let primer = stream::iter(primer_events_from_cache(cached));

    let live = BroadcastStream::new(rx).map(|result| match result {
        Ok(event) => {
            let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Ok(Event::default().data(json))
        }
        Err(_) => Ok(Event::default().comment("lagged")),
    });

    Sse::new(primer.chain(live)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}

fn primer_events_from_cache(cached: Option<AllStats>) -> Vec<Result<Event, Infallible>> {
    let Some(bundle) = cached else {
        return Vec::new();
    };
    let mut events: Vec<StatsEvent> = Vec::with_capacity(6);
    events.push(StatsEvent::Cpu(bundle.cpu));
    events.push(StatsEvent::Memory(bundle.memory));
    events.push(StatsEvent::Disk(bundle.disks));
    events.push(StatsEvent::Network(bundle.network));
    if let Some(p) = bundle.pressure {
        events.push(StatsEvent::Pressure(p));
    }
    if let Some(c) = bundle.components {
        events.push(StatsEvent::Components(c));
    }
    events
        .into_iter()
        .map(|e| {
            let json = serde_json::to_string(&e).unwrap_or_else(|_| "{}".to_string());
            Ok(Event::default().data(json))
        })
        .collect()
}

/// SSE endpoint for CPU stats only
pub async fn stream_cpu_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(StatsEvent::Cpu(cpu_stats)) => {
                let json = serde_json::to_string(&cpu_stats).unwrap_or_else(|_| "{}".to_string());
                Some(Ok(Event::default().data(json)))
            }
            Ok(_) => None, // Filter out non-CPU events
            Err(_) => Some(Ok(Event::default().comment("lagged"))),
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}

/// SSE endpoint for memory stats only
pub async fn stream_memory_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(StatsEvent::Memory(mem_stats)) => {
                let json = serde_json::to_string(&mem_stats).unwrap_or_else(|_| "{}".to_string());
                Some(Ok(Event::default().data(json)))
            }
            Ok(_) => None,
            Err(_) => Some(Ok(Event::default().comment("lagged"))),
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}

/// SSE endpoint for disk stats only
pub async fn stream_disk_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(StatsEvent::Disk(disk_stats)) => {
                let json = serde_json::to_string(&disk_stats).unwrap_or_else(|_| "{}".to_string());
                Some(Ok(Event::default().data(json)))
            }
            Ok(_) => None,
            Err(_) => Some(Ok(Event::default().comment("lagged"))),
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}

/// SSE endpoint for network stats only
pub async fn stream_network_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(StatsEvent::Network(net_stats)) => {
                let json = serde_json::to_string(&net_stats).unwrap_or_else(|_| "{}".to_string());
                Some(Ok(Event::default().data(json)))
            }
            Ok(_) => None,
            Err(_) => Some(Ok(Event::default().comment("lagged"))),
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}
