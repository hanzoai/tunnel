//! WebSocket transport with reconnection and heartbeat.

use crate::protocol::Frame;
use crate::TunnelError;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Transport configuration.
pub struct TransportConfig {
    pub url: String,
    pub auth_bearer: String,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub heartbeat_interval: Duration,
    pub connect_timeout: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            auth_bearer: String::new(),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// Connect to the WebSocket relay.
async fn ws_connect(
    config: &TransportConfig,
) -> Result<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    TunnelError,
> {
    let mut request = config
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| TunnelError::Connection(e.to_string()))?;

    if !config.auth_bearer.is_empty() {
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", config.auth_bearer)
                .parse()
                .map_err(|e: http::header::InvalidHeaderValue| {
                    TunnelError::Connection(e.to_string())
                })?,
        );
    }

    let (ws, _response) =
        tokio::time::timeout(config.connect_timeout, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| TunnelError::Connection("connect timeout".into()))?
            .map_err(|e| TunnelError::Connection(e.to_string()))?;

    Ok(ws)
}

/// Run the WebSocket transport loop. Forwards frames between the tunnel
/// channels and the WebSocket connection.
pub async fn run_transport(
    config: &TransportConfig,
    outgoing_rx: &mut mpsc::Receiver<Frame>,
    incoming_tx: &mpsc::Sender<Frame>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), TunnelError> {
    let ws = ws_connect(config).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut heartbeat = tokio::time::interval(config.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Shutdown signal.
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("transport shutdown");
                    let _ = ws_tx.close().await;
                    return Ok(());
                }
            }

            // Outgoing frame from local → cloud.
            Some(frame) = outgoing_rx.recv() => {
                let json = serde_json::to_string(&frame)
                    .map_err(|e| TunnelError::Protocol(e.to_string()))?;
                ws_tx.send(Message::Text(json.into())).await
                    .map_err(|e| TunnelError::Connection(e.to_string()))?;
            }

            // Incoming message from cloud → local.
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Frame>(&text) {
                            Ok(Frame::Ping) => {
                                let pong = serde_json::to_string(&Frame::Pong)
                                    .map_err(|e| TunnelError::Protocol(e.to_string()))?;
                                ws_tx.send(Message::Text(pong.into())).await
                                    .map_err(|e| TunnelError::Connection(e.to_string()))?;
                            }
                            Ok(Frame::Pong) => {
                                debug!("received pong");
                            }
                            Ok(frame) => {
                                if incoming_tx.send(frame).await.is_err() {
                                    return Ok(()); // receiver dropped
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, text = %text, "invalid frame");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        ws_tx.send(Message::Pong(data)).await
                            .map_err(|e| TunnelError::Connection(e.to_string()))?;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("websocket closed");
                        return Err(TunnelError::Connection("websocket closed".into()));
                    }
                    Some(Ok(_)) => {} // Binary, Pong, Frame — ignore
                    Some(Err(e)) => {
                        error!(error = %e, "websocket error");
                        return Err(TunnelError::Connection(e.to_string()));
                    }
                }
            }

            // Heartbeat tick.
            _ = heartbeat.tick() => {
                let ping = serde_json::to_string(&Frame::Ping)
                    .map_err(|e| TunnelError::Protocol(e.to_string()))?;
                if ws_tx.send(Message::Text(ping.into())).await.is_err() {
                    return Err(TunnelError::Connection("heartbeat send failed".into()));
                }
            }
        }
    }
}

/// Run the transport with automatic reconnection (exponential backoff).
#[cfg(feature = "reconnect")]
pub async fn run_with_reconnect(
    config: TransportConfig,
    mut outgoing_rx: mpsc::Receiver<Frame>,
    incoming_tx: mpsc::Sender<Frame>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut backoff = config.initial_backoff;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        match run_transport(&config, &mut outgoing_rx, &incoming_tx, &mut shutdown_rx).await {
            Ok(()) => break, // clean shutdown
            Err(e) => {
                warn!(error = %e, backoff_secs = backoff.as_secs(), "transport error, reconnecting");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
                backoff = (backoff * 2).min(config.max_backoff);
            }
        }
    }
}
