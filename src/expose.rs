//! Expose local services through the tunnel (ngrok-like functionality).

use crate::protocol::{EventPayload, Frame};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// A local service to expose through the tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposedService {
    /// Service name (e.g. "api", "web", "mcp").
    pub name: String,
    /// Local address to forward to (e.g. "127.0.0.1:3000").
    pub local_addr: String,
    /// Protocol type.
    pub protocol: ExposedProtocol,
    /// Desired subdomain (optional, cloud assigns one if empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdomain: Option<String>,
}

/// Protocol for exposed services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposedProtocol {
    Http,
    WebSocket,
    Tcp,
}

/// A URL assigned by the cloud for an exposed service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposedUrl {
    /// Service name.
    pub name: String,
    /// The public URL assigned by the cloud.
    pub url: String,
    /// TTL in seconds (0 = permanent while connected).
    #[serde(default)]
    pub ttl: u64,
}

/// Request to expose a local service.
pub async fn expose(
    tx: &mpsc::Sender<Frame>,
    service: &ExposedService,
) -> Result<(), crate::TunnelError> {
    let frame = Frame::Event(EventPayload {
        event: "expose.request".into(),
        data: serde_json::to_value(service)
            .map_err(|e| crate::TunnelError::Protocol(e.to_string()))?,
    });
    tx.send(frame)
        .await
        .map_err(|_| crate::TunnelError::ChannelClosed)
}

/// Request to stop exposing a service.
pub async fn unexpose(
    tx: &mpsc::Sender<Frame>,
    name: &str,
) -> Result<(), crate::TunnelError> {
    let frame = Frame::Event(EventPayload {
        event: "expose.stop".into(),
        data: serde_json::json!({"name": name}),
    });
    tx.send(frame)
        .await
        .map_err(|_| crate::TunnelError::ChannelClosed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_exposed_service() {
        let svc = ExposedService {
            name: "api".into(),
            local_addr: "127.0.0.1:8080".into(),
            protocol: ExposedProtocol::Http,
            subdomain: Some("my-api".into()),
        };
        let json = serde_json::to_string(&svc).unwrap();
        assert!(json.contains("\"protocol\":\"http\""));
        assert!(json.contains("\"subdomain\":\"my-api\""));
    }

    #[tokio::test]
    async fn expose_sends_frame() {
        let (tx, mut rx) = mpsc::channel(4);
        let svc = ExposedService {
            name: "web".into(),
            local_addr: "127.0.0.1:3000".into(),
            protocol: ExposedProtocol::Http,
            subdomain: None,
        };
        expose(&tx, &svc).await.unwrap();
        let frame = rx.recv().await.unwrap();
        match frame {
            Frame::Event(e) => assert_eq!(e.event, "expose.request"),
            _ => panic!("expected Event frame"),
        }
    }
}
