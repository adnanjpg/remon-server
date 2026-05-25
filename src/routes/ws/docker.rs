#![cfg(feature = "docker")]

//! WebSocket Docker `exec` endpoint.
//!
//! Hardening notes:
//! - `cmd` is parsed with `shell_words::split` so `sh -c "echo hi"` is one
//!   argv with three elements (the old `split_whitespace` broke quoted
//!   strings into four).
//! - `max_message_size` / `max_frame_size` capped at 64 KiB so a runaway
//!   client can't push the server into unbounded buffering.
//! - A 30-second server-driven ping keeps the connection alive across NAT
//!   timeouts; the same tick checks last-activity and closes idle sessions
//!   after `IDLE_TIMEOUT`.
//! - `state.docker_exec_enabled` is a master kill-switch; flipping it to
//!   false (config) refuses new upgrades with 503.
//! - This endpoint still has no per-token scope check — every authenticated
//!   device that lands here can spawn a shell inside any container. That's
//!   a known gap; the kill-switch above is the workaround until JWT scope
//!   claims land.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::{
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bollard::{
    Docker,
    exec::{CreateExecOptions, StartExecResults},
};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::routes::extractors::Claims;
use crate::state::AppState;

/// Cap WebSocket frame & message size. Plenty for an interactive shell;
/// anything larger is suspicious (client dumping a binary into the socket).
const WS_MAX_BYTES: usize = 64 * 1024;
/// Server-driven ping cadence — keeps NAT/load-balancer idle timers warm.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Maximum time without inbound activity (client message OR exec output)
/// before the session is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecParams {
    /// Command line to run inside the container. Parsed with shell-words,
    /// so quoted segments stay intact (`sh -c "echo hi"`). Defaults to
    /// `/bin/sh` (interactive shell).
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default = "default_stdin")]
    pub stdin: bool,
}

fn default_stdin() -> bool {
    true
}

pub async fn docker_exec(
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
    Path(container_id): Path<String>,
    _claims: Claims,
    Query(params): Query<ExecParams>,
) -> Response {
    if !state.docker_exec_enabled.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Docker exec endpoint is disabled by server configuration",
        )
            .into_response();
    }

    let cmd_str = params.cmd.clone().unwrap_or_else(|| "/bin/sh".to_string());
    let cmd_argv = match shell_words::split(&cmd_str) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "cmd cannot be empty after shell parsing",
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("cmd is not valid shell syntax: {}", e),
            )
                .into_response();
        }
    };

    ws.max_message_size(WS_MAX_BYTES)
        .max_frame_size(WS_MAX_BYTES)
        .on_upgrade(move |socket| handle_docker_exec(socket, container_id, cmd_argv, params))
}

async fn handle_docker_exec(
    mut socket: WebSocket,
    container_id: String,
    cmd_argv: Vec<String>,
    params: ExecParams,
) {
    debug!(
        "WebSocket exec started for container: {}, cmd: {:?}, tty: {}",
        container_id, cmd_argv, params.tty
    );

    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to connect to Docker: {}", e);
            let _ = socket
                .send(Message::Text(format!("Error: {}", e).into()))
                .await;
            close_socket(&mut socket).await;
            return;
        }
    };

    let exec_config = CreateExecOptions {
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        attach_stdin: Some(params.stdin),
        tty: Some(params.tty),
        cmd: Some(cmd_argv),
        ..Default::default()
    };

    let exec_id = match docker.create_exec(&container_id, exec_config).await {
        Ok(exec) => exec.id,
        Err(e) => {
            error!("Failed to create exec: {}", e);
            let _ = socket
                .send(Message::Text(format!("Error: {}", e).into()))
                .await;
            close_socket(&mut socket).await;
            return;
        }
    };

    let start_exec = match docker.start_exec(&exec_id, None).await {
        Ok(exec) => exec,
        Err(e) => {
            error!("Failed to start exec: {}", e);
            let _ = socket
                .send(Message::Text(format!("Error starting exec: {}", e).into()))
                .await;
            close_socket(&mut socket).await;
            return;
        }
    };

    match start_exec {
        StartExecResults::Attached {
            mut output,
            mut input,
        } => {
            debug!("Exec attached successfully");

            let mut last_activity = Instant::now();
            let mut ping_tick = tokio::time::interval(PING_INTERVAL);
            // Skip the first immediate tick — we don't want to ping at t=0.
            ping_tick.tick().await;

            loop {
                tokio::select! {
                    ws_msg = socket.recv() => {
                        last_activity = Instant::now();
                        match ws_msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Err(e) = input.write_all(text.as_bytes()).await {
                                    warn!("Failed to write to exec stdin: {}", e);
                                    break;
                                }
                            }
                            Some(Ok(Message::Binary(data))) => {
                                if let Err(e) = input.write_all(&data).await {
                                    warn!("Failed to write to exec stdin: {}", e);
                                    break;
                                }
                            }
                            // Browser/wscat answer our ping with Pong; we
                            // only need to refresh `last_activity` (already
                            // done above) and keep going.
                            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Ping(_))) => {}
                            Some(Ok(Message::Close(_))) | None => {
                                debug!("WebSocket closed by client");
                                break;
                            }
                            Some(Err(e)) => {
                                warn!("WebSocket error: {}", e);
                                break;
                            }
                        }
                    }

                    exec_output = output.next() => {
                        last_activity = Instant::now();
                        match exec_output {
                            Some(Ok(log_output)) => {
                                let text = log_output.to_string();
                                if !text.is_empty()
                                    && let Err(e) = socket.send(Message::Text(text.into())).await
                                {
                                    warn!("Failed to send to WebSocket: {}", e);
                                    break;
                                }
                            }
                            Some(Err(e)) => {
                                error!("Exec output error: {}", e);
                                let _ = socket
                                    .send(Message::Text(format!("Error: {}", e).into()))
                                    .await;
                                break;
                            }
                            None => {
                                debug!("Exec completed");
                                break;
                            }
                        }
                    }

                    _ = ping_tick.tick() => {
                        if last_activity.elapsed() >= IDLE_TIMEOUT {
                            debug!(
                                "WebSocket exec idle for {:?}, closing",
                                last_activity.elapsed()
                            );
                            break;
                        }
                        if let Err(e) = socket
                            .send(Message::Ping(axum::body::Bytes::new()))
                            .await
                        {
                            warn!("Failed to send keepalive ping: {}", e);
                            break;
                        }
                    }
                }
            }

            close_socket(&mut socket).await;
            debug!("WebSocket exec session closed");
        }
        StartExecResults::Detached => {
            warn!("Exec started in detached mode (unexpected)");
            let _ = socket
                .send(Message::Text("Error: Exec detached unexpectedly".into()))
                .await;
            close_socket(&mut socket).await;
        }
    }
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = socket.close().await;
}
