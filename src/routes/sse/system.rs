use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{Stream, StreamExt};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::BroadcastStream;

use crate::state::AppState;

/// SSE endpoint for CPU stats
/// Subscribes to broadcast channel and streams CPU metrics to client
pub async fn stream_cpu_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.cpu_stats_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|result| {
        match result {
            Ok(data) => {
                let json = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
                Ok(Event::default().data(json))
            }
            Err(_) => {
                // Lagged (missed some messages due to slow client)
                Ok(Event::default().comment("lagged"))
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}

/// SSE endpoint for memory stats
pub async fn stream_memory_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.mem_stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|result| match result {
        Ok(data) => {
            let json = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
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

/// SSE endpoint for disk stats
pub async fn stream_disk_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.disk_stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|result| match result {
        Ok(data) => {
            let json = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
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

/// SSE endpoint for network stats
pub async fn stream_network_stats(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.network_stats_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|result| match result {
        Ok(data) => {
            let json = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
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
