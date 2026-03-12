//! Bot gateway protocol adapter.
//!
//! Connects to the Hanzo bot gateway using its native WebSocket protocol
//! (challenge/connect/hello-ok handshake, req/res/event frames). This lets
//! tunnel clients register as nodes in the gateway's NodeRegistry and
//! handle `node.invoke` commands.

use crate::protocol::AppKind;
use crate::TunnelError;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Gateway frame types (matches bot gateway protocol).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GatewayFrame {
    /// Request from client to gateway.
    Req {
        id: String,
        method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    /// Response from gateway to client.
    Res {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<GatewayError>,
    },
    /// Event broadcast.
    Event {
        event: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// Configuration for connecting to the bot gateway.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Gateway WebSocket URL (e.g., ws://127.0.0.1:18789 or wss://gw.hanzo.bot).
    pub url: String,
    /// Auth token (device token or password).
    pub auth_token: String,
    /// Auth method: "token" or "password".
    pub auth_method: String,
    /// Client ID (used as node ID).
    pub client_id: String,
    /// Display name shown in the gateway.
    pub display_name: String,
    /// App kind (mapped to platform string).
    pub app_kind: AppKind,
    /// Version string.
    pub version: String,
    /// Capabilities advertised to the gateway.
    pub capabilities: Vec<String>,
    /// Commands this node can handle.
    pub commands: Vec<String>,
    /// Connect timeout.
    pub connect_timeout: Duration,
    /// Heartbeat interval.
    pub heartbeat_interval: Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:18789".into(),
            auth_token: String::new(),
            auth_method: "token".into(),
            client_id: uuid::Uuid::new_v4().to_string(),
            display_name: String::new(),
            app_kind: AppKind::Dev,
            version: String::new(),
            capabilities: vec![],
            commands: vec![],
            connect_timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(30),
        }
    }
}

/// A command received from the gateway via node.invoke.
#[derive(Debug, Clone)]
pub struct NodeInvokeRequest {
    /// Request ID (must be echoed in the result).
    pub id: String,
    /// Target node ID.
    pub node_id: String,
    /// Command name (e.g., "dev.chat", "terminal.open", "system.run").
    pub command: String,
    /// Command parameters (JSON).
    pub params: Option<Value>,
    /// Timeout in ms.
    pub timeout_ms: Option<u64>,
}

/// A live connection to the bot gateway.
pub struct GatewayConnection {
    /// Send gateway frames.
    outgoing_tx: mpsc::Sender<GatewayFrame>,
    /// Receive node.invoke requests.
    invoke_rx: Mutex<mpsc::Receiver<NodeInvokeRequest>>,
    /// Receive raw gateway events.
    event_rx: Mutex<mpsc::Receiver<(String, Option<Value>)>>,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<bool>,
    /// Node ID assigned by gateway.
    pub node_id: String,
    /// Connection ID from hello-ok.
    pub conn_id: String,
    /// Next request ID.
    next_id: std::sync::atomic::AtomicU64,
}

impl GatewayConnection {
    /// Send the result of a node.invoke back to the gateway.
    pub async fn send_invoke_result(
        &self,
        request_id: &str,
        ok: bool,
        payload: Option<Value>,
        error: Option<GatewayError>,
    ) -> Result<(), TunnelError> {
        let frame = GatewayFrame::Req {
            id: uuid::Uuid::new_v4().to_string(),
            method: "node.invoke.result".into(),
            params: Some(serde_json::json!({
                "id": request_id,
                "nodeId": self.node_id,
                "ok": ok,
                "payloadJSON": payload.map(|v| v.to_string()),
                "error": error,
            })),
        };
        self.outgoing_tx
            .send(frame)
            .await
            .map_err(|_| TunnelError::ChannelClosed)
    }

    /// Send a node event to the gateway (broadcast to operators).
    pub async fn send_node_event(
        &self,
        event: &str,
        payload: Option<Value>,
    ) -> Result<(), TunnelError> {
        let frame = GatewayFrame::Req {
            id: uuid::Uuid::new_v4().to_string(),
            method: "node.event".into(),
            params: Some(serde_json::json!({
                "event": event,
                "payloadJSON": payload.map(|v| v.to_string()),
            })),
        };
        self.outgoing_tx
            .send(frame)
            .await
            .map_err(|_| TunnelError::ChannelClosed)
    }

    /// Receive the next node.invoke request (blocking).
    pub async fn recv_invoke(&self) -> Option<NodeInvokeRequest> {
        self.invoke_rx.lock().await.recv().await
    }

    /// Receive a raw gateway event.
    pub async fn recv_event(&self) -> Option<(String, Option<Value>)> {
        self.event_rx.lock().await.recv().await
    }

    /// Send a request to the gateway and get a response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), TunnelError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let frame = GatewayFrame::Req {
            id: format!("req-{id}"),
            method: method.into(),
            params,
        };
        self.outgoing_tx
            .send(frame)
            .await
            .map_err(|_| TunnelError::ChannelClosed)
    }

    /// Shutdown the connection.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Connect to the bot gateway using its native protocol.
///
/// Performs the challenge/connect/hello-ok handshake, then runs the
/// transport in a background task. Returns a `GatewayConnection` for
/// sending invoke results and receiving invoke requests.
pub async fn connect_gateway(config: GatewayConfig) -> Result<GatewayConnection, TunnelError> {
    // Connect WebSocket.
    let mut request = config
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| TunnelError::Connection(e.to_string()))?;

    // Don't add auth header — gateway uses its own handshake.
    let (ws, _) =
        tokio::time::timeout(config.connect_timeout, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| TunnelError::Connection("connect timeout".into()))?
            .map_err(|e| TunnelError::Connection(e.to_string()))?;

    let (mut ws_tx, mut ws_rx) = ws.split();

    // Step 1: Wait for connect.challenge event.
    let challenge = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(msg) = ws_rx.next().await {
            if let Ok(Message::Text(text)) = msg {
                if let Ok(frame) = serde_json::from_str::<GatewayFrame>(&text) {
                    if let GatewayFrame::Event {
                        event, payload, ..
                    } = &frame
                    {
                        if event == "connect.challenge" {
                            return Ok(payload.clone());
                        }
                    }
                }
            }
        }
        Err(TunnelError::Connection("no challenge received".into()))
    })
    .await
    .map_err(|_| TunnelError::Connection("challenge timeout".into()))??;

    let _nonce = challenge
        .as_ref()
        .and_then(|p| p.get("nonce"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Step 2: Send connect request.
    let connect_id = uuid::Uuid::new_v4().to_string();
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    let mut auth_obj = serde_json::Map::new();
    match config.auth_method.as_str() {
        "password" => {
            auth_obj.insert("password".into(), Value::String(config.auth_token.clone()));
        }
        _ => {
            auth_obj.insert("token".into(), Value::String(config.auth_token.clone()));
        }
    }

    let connect_frame = GatewayFrame::Req {
        id: connect_id.clone(),
        method: "connect".into(),
        params: Some(serde_json::json!({
            "minProtocol": 1,
            "maxProtocol": 1,
            "client": {
                "id": config.client_id,
                "displayName": config.display_name,
                "version": config.version,
                "platform": platform,
                "mode": "node",
            },
            "caps": config.capabilities,
            "commands": config.commands,
            "role": "node",
            "auth": auth_obj,
        })),
    };

    let connect_json = serde_json::to_string(&connect_frame)
        .map_err(|e| TunnelError::Protocol(e.to_string()))?;
    ws_tx
        .send(Message::Text(connect_json.into()))
        .await
        .map_err(|e| TunnelError::Connection(e.to_string()))?;

    // Step 3: Wait for hello-ok or error response.
    let hello = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws_rx.next().await {
            if let Ok(Message::Text(text)) = msg {
                // hello-ok is a special frame (not wrapped in res).
                if let Ok(val) = serde_json::from_str::<Value>(&text) {
                    if val.get("type").and_then(|t| t.as_str()) == Some("hello-ok") {
                        return Ok(val);
                    }
                    // Also check for error response to our connect.
                    if let Ok(frame) = serde_json::from_str::<GatewayFrame>(&text) {
                        if let GatewayFrame::Res { id, ok, error, .. } = &frame {
                            if id == &connect_id && !ok {
                                let msg = error
                                    .as_ref()
                                    .map(|e| e.message.clone())
                                    .unwrap_or_else(|| "connect rejected".into());
                                return Err(TunnelError::Auth(msg));
                            }
                        }
                    }
                }
            }
        }
        Err(TunnelError::Connection("no hello-ok received".into()))
    })
    .await
    .map_err(|_| TunnelError::Connection("hello-ok timeout".into()))??;

    let conn_id = hello
        .get("server")
        .and_then(|s| s.get("connId"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    info!(
        node_id = %config.client_id,
        conn_id = %conn_id,
        "connected to bot gateway"
    );

    // Set up channels.
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<GatewayFrame>(256);
    let (invoke_tx, invoke_rx) = mpsc::channel::<NodeInvokeRequest>(64);
    let (event_tx, event_rx) = mpsc::channel::<(String, Option<Value>)>(256);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let node_id = config.client_id.clone();
    let heartbeat_interval = config.heartbeat_interval;

    // Spawn transport loop.
    tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        debug!("gateway transport shutdown");
                        let _ = ws_tx.close().await;
                        return;
                    }
                }

                Some(frame) = outgoing_rx.recv() => {
                    let json = match serde_json::to_string(&frame) {
                        Ok(j) => j,
                        Err(e) => {
                            error!(error = %e, "failed to serialize gateway frame");
                            continue;
                        }
                    };
                    if ws_tx.send(Message::Text(json.into())).await.is_err() {
                        warn!("gateway websocket send failed");
                        return;
                    }
                }

                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<GatewayFrame>(&text) {
                                Ok(GatewayFrame::Event { event, payload, .. }) => {
                                    if event == "node.invoke.request" {
                                        if let Some(ref p) = payload {
                                            let req = NodeInvokeRequest {
                                                id: p.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                                                node_id: p.get("nodeId").and_then(|v| v.as_str()).unwrap_or("").into(),
                                                command: p.get("command").and_then(|v| v.as_str()).unwrap_or("").into(),
                                                params: p.get("paramsJSON")
                                                    .and_then(|v| v.as_str())
                                                    .and_then(|s| serde_json::from_str(s).ok()),
                                                timeout_ms: p.get("timeoutMs").and_then(|v| v.as_u64()),
                                            };
                                            let _ = invoke_tx.send(req).await;
                                        }
                                    } else if event == "tick" {
                                        // Heartbeat from gateway, no action needed.
                                    } else {
                                        let _ = event_tx.send((event, payload)).await;
                                    }
                                }
                                Ok(GatewayFrame::Res { .. }) => {
                                    // Response to our requests — currently we don't await them.
                                    debug!("gateway response: {}", text);
                                }
                                Ok(GatewayFrame::Req { .. }) => {
                                    // Gateway shouldn't send us requests (except via events).
                                    debug!("unexpected gateway request: {}", text);
                                }
                                Err(e) => {
                                    // Might be hello-ok or other special frames.
                                    debug!(error = %e, "unrecognized gateway frame");
                                }
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = ws_tx.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            info!("gateway websocket closed");
                            return;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            error!(error = %e, "gateway websocket error");
                            return;
                        }
                    }
                }

                _ = heartbeat.tick() => {
                    // Gateway expects ticks, but the server sends them to us.
                    // We can send a health status if needed.
                }
            }
        }
    });

    Ok(GatewayConnection {
        outgoing_tx,
        invoke_rx: Mutex::new(invoke_rx),
        event_rx: Mutex::new(event_rx),
        shutdown_tx,
        node_id,
        conn_id,
        next_id: std::sync::atomic::AtomicU64::new(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_frame_serialization() {
        let frame = GatewayFrame::Req {
            id: "req-1".into(),
            method: "connect".into(),
            params: Some(serde_json::json!({"minProtocol": 1})),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"req\""));
        assert!(json.contains("\"method\":\"connect\""));
    }

    #[test]
    fn gateway_event_deserialization() {
        let json = r#"{"type":"event","event":"node.invoke.request","payload":{"id":"abc","nodeId":"n1","command":"system.run","paramsJSON":"{\"cmd\":\"ls\"}"}}"#;
        let frame: GatewayFrame = serde_json::from_str(json).unwrap();
        match frame {
            GatewayFrame::Event { event, payload, .. } => {
                assert_eq!(event, "node.invoke.request");
                assert!(payload.is_some());
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn default_gateway_config() {
        let config = GatewayConfig::default();
        assert_eq!(config.url, "ws://127.0.0.1:18789");
        assert_eq!(config.auth_method, "token");
        assert!(matches!(config.app_kind, AppKind::Dev));
    }
}
