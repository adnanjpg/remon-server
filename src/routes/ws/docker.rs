use axum::{
    extract::{
        Path, Query, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use bollard::{
    Docker,
    exec::{CreateExecOptions, StartExecResults},
};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::api::extractors::Claims;

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecParams {
    #[serde(default = "default_cmd")]
    pub cmd: String,
    #[serde(default)]
    pub tty: bool,
    #[serde(default = "default_stdin")]
    pub stdin: bool,
}

fn default_cmd() -> String {
    "/bin/sh".to_string()
}

fn default_stdin() -> bool {
    true
}

pub async fn docker_exec(
    ws: WebSocketUpgrade,
    Path(container_id): Path<String>,
    _claims: Claims,
    Query(params): Query<ExecParams>,
) -> Response {
    ws.on_upgrade(move |socket| handle_docker_exec(socket, container_id, params))
}

async fn handle_docker_exec(mut socket: WebSocket, container_id: String, params: ExecParams) {
    debug!(
        "WebSocket exec started for container: {}, cmd: {}, tty: {}",
        container_id, params.cmd, params.tty
    );

    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to connect to Docker: {}", e);
            let _ = socket
                .send(Message::Text(format!("Error: {}", e).into()))
                .await;
            let _ = socket.close().await;
            return;
        }
    };

    let cmd_parts: Vec<&str> = params.cmd.split_whitespace().collect();
    let exec_config = CreateExecOptions {
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        attach_stdin: Some(params.stdin),
        tty: Some(params.tty),
        cmd: Some(cmd_parts.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    };

    let exec_id = match docker.create_exec(&container_id, exec_config).await {
        Ok(exec) => exec.id,
        Err(e) => {
            error!("Failed to create exec: {}", e);
            let _ = socket
                .send(Message::Text(format!("Error: {}", e).into()))
                .await;
            let _ = socket.close().await;
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
            let _ = socket.close().await;
            return;
        }
    };

    match start_exec {
        StartExecResults::Attached {
            mut output,
            mut input,
        } => {
            debug!("Exec attached successfully");

            loop {
                tokio::select! {
                    ws_msg = socket.recv() => {
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
                            Some(Ok(Message::Close(_))) | None => {
                                debug!("WebSocket closed by client");
                                break;
                            }
                            Some(Err(e)) => {
                                warn!("WebSocket error: {}", e);
                                break;
                            }
                            _ => {}
                        }
                    }

                    exec_output = output.next() => {
                        match exec_output {
                            Some(Ok(log_output)) => {
                                let text = log_output.to_string();
                                if !text.is_empty() {
                                    if let Err(e) = socket.send(Message::Text(text.into())).await {
                                        warn!("Failed to send to WebSocket: {}", e);
                                        break;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                error!("Exec output error: {}", e);
                                let _ = socket.send(Message::Text(format!("Error: {}", e).into())).await;
                                break;
                            }
                            None => {
                                debug!("Exec completed");
                                break;
                            }
                        }
                    }
                }
            }

            let _ = socket.close().await;
            debug!("WebSocket exec session closed");
        }
        StartExecResults::Detached => {
            warn!("Exec started in detached mode (unexpected)");
            let _ = socket
                .send(Message::Text("Error: Exec detached unexpectedly".into()))
                .await;
            let _ = socket.close().await;
        }
    }
}
