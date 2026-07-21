//! End-to-end proof that an open SSE stream ends promptly once shutdown
//! starts, instead of blocking axum's graceful shutdown forever (the bug
//! fixed by `routes::sse::until_shutdown` — see CHANGELOG 0.17.2).

use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use futures_util::StreamExt;
use tower::ServiceExt;

use super::TestApp;

#[tokio::test]
async fn sse_stream_ends_promptly_on_shutdown() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let mut req = Request::builder()
        .method("GET")
        .uri("/sse/stats/cpu")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build request");
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));

    let resp = app.router.clone().oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();

    // No stats collector runs in the test harness, so nothing new is ever
    // broadcast — the stream is infinite by design and must NOT end on its
    // own. This is the exact shape of the original bug: an SSE connection
    // with nothing left to say still never terminates.
    let before_shutdown = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
    assert!(
        before_shutdown.is_err(),
        "stream ended on its own before shutdown — test setup doesn't exercise the bug"
    );

    app.state.shutdown.send(true).expect("send shutdown");

    let after_shutdown = tokio::time::timeout(Duration::from_secs(1), stream.next()).await;
    assert!(
        matches!(after_shutdown, Ok(None)),
        "stream must end within 1s of shutdown, got {after_shutdown:?}"
    );
}
