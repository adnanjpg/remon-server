use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{Stream, StreamExt};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::BroadcastStream;

use crate::{models::stats::StatsEvent, state::AppState};

/// SSE endpoint for unified stats stream
/// Subscribes to the new unified stats broadcast channel
pub async fn stream_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|result| match result {
        Ok(event) => {
            let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Ok(Event::default().data(json))
        }
        Err(_) => Ok(Event::default().comment("lagged")),
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
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
