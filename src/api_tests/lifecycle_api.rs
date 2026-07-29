//! The deliberate way to end the server, and the wiring that carries the
//! decision out to the supervisor.
//!
//! The generic control endpoints refuse to name this server (see
//! `process_api`), so these are the only paths left — which makes it worth
//! proving they answer before they act and that the intent survives to the
//! point where the exit code is chosen.

use std::time::Duration;

use axum::http::StatusCode;

use super::TestApp;
use crate::state::ExitIntent;

#[tokio::test]
async fn restart_requires_auth() {
    let app = TestApp::spawn().await;
    let (st, _) = app.request("POST", "/system/restart", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn shutdown_requires_auth() {
    let app = TestApp::spawn().await;
    let (st, _) = app.request("POST", "/system/shutdown", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

/// The response has to go out before the process ends, so the handler records
/// the intent and returns; something else does the ending. Anything that
/// stopped the runtime inline would never reach the client.
#[tokio::test]
async fn restart_answers_before_it_acts() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    assert_eq!(
        *app.state.exit_intent.borrow(),
        None,
        "nothing should have asked to exit yet"
    );

    let (st, body) = app
        .request("POST", "/system/restart", Some(&token), None)
        .await;

    assert_eq!(st, StatusCode::ACCEPTED);
    assert_eq!(*app.state.exit_intent.borrow(), Some(ExitIntent::Restart));
    // The client cannot see the exit code, so the message is the only place
    // "you will get it back" can be said.
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("supervisor"),
        "expected the reply to say the agent is coming back, got: {message}"
    );
}

/// Nothing supervises the test harness, so shutdown degrades to exiting. The
/// reply has to say that without claiming the process stays down: under Docker
/// `restart: always` or supervisord no unit is identifiable and the container
/// comes straight back.
#[tokio::test]
async fn shutdown_without_a_supervisor_exits_without_promising_to_stay_down() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request("POST", "/system/shutdown", Some(&token), None)
        .await;

    assert_eq!(st, StatusCode::ACCEPTED);
    assert_eq!(*app.state.exit_intent.borrow(), Some(ExitIntent::Shutdown));
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("No supervising unit"),
        "expected the reply to name why it can only exit, got: {message}"
    );
    assert!(
        !message.contains("Nothing will restart"),
        "the reply must not promise an outcome it cannot know, got: {message}"
    );
}

/// The intent is what `main` reads to pick the exit code, and the shutdown
/// signal is what gets it there. Proving the signal fires on a request closes
/// the loop between the handler and the process ending — without it the
/// endpoint would answer 202 and the server would keep running.
#[tokio::test]
async fn a_requested_exit_wakes_the_shutdown_signal() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let signal = crate::shutdown::signal(app.state.exit_intent.subscribe());
    tokio::pin!(signal);

    // Nothing has been requested, so it must not resolve on its own.
    let idle = tokio::time::timeout(Duration::from_millis(100), &mut signal).await;
    assert!(
        idle.is_err(),
        "the shutdown signal fired without anything asking for it"
    );

    let (st, _) = app
        .request("POST", "/system/restart", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);

    let fired = tokio::time::timeout(Duration::from_secs(1), signal).await;
    assert!(
        fired.is_ok(),
        "the shutdown signal did not fire after a restart request"
    );
}
