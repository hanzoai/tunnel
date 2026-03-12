//! # hanzo-tunnel
//!
//! Cloud tunnel client for Hanzo apps — register, stream, and remote-control
//! local instances from `app.hanzo.bot`.
//!
//! ```no_run
//! # async fn example() -> Result<(), hanzo_tunnel::TunnelError> {
//! let conn = hanzo_tunnel::connect(hanzo_tunnel::TunnelConfig {
//!     relay_url: "wss://api.hanzo.ai/v1/relay".into(),
//!     auth_token: "hk-abc123".into(),
//!     app_kind: hanzo_tunnel::AppKind::Dev,
//!     display_name: "z-macbook".into(),
//!     capabilities: vec!["chat".into(), "exec".into()],
//!     ..Default::default()
//! }).await?;
//! println!("instance_id = {}", conn.instance_id);
//! # Ok(())
//! # }
//! ```

pub mod auth;
pub mod commands;
pub mod discovery;
pub mod expose;
pub mod gateway;
pub mod protocol;
pub mod registry;
pub mod terminal;
pub mod transport;

pub use protocol::{AppKind, CommandPayload, EventPayload, Frame, ResponsePayload};

use protocol::{RegisterPayload, RegisteredPayload};
use std::path::PathBuf;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, info};

/// Errors that can occur in the tunnel.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("discovery error: {0}")]
    Discovery(String),
    #[error("channel closed")]
    ChannelClosed,
    #[error("timeout")]
    Timeout,
}

/// Configuration for connecting to the cloud relay.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// Cloud relay URL (default: wss://api.hanzo.ai/v1/relay).
    pub relay_url: String,
    /// Auth token (hanzo.id JWT or API key).
    pub auth_token: String,
    /// What kind of app this is.
    pub app_kind: AppKind,
    /// Display name (e.g., hostname, custom label).
    pub display_name: String,
    /// Capabilities this instance advertises.
    pub capabilities: Vec<String>,
    /// Commands this instance supports.
    pub commands: Vec<String>,
    /// Working directory (for dev/desktop).
    pub cwd: Option<PathBuf>,
    /// App version string.
    pub version: String,
    /// Channel buffer size.
    pub channel_size: usize,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            relay_url: "wss://api.hanzo.ai/v1/relay".into(),
            auth_token: String::new(),
            app_kind: AppKind::Dev,
            display_name: String::new(),
            capabilities: vec![],
            commands: vec![],
            cwd: None,
            version: String::new(),
            channel_size: 256,
        }
    }
}

/// A live tunnel connection.
pub struct TunnelConnection {
    /// Send frames to the cloud.
    outgoing_tx: mpsc::Sender<Frame>,
    /// Receive frames from the cloud.
    incoming_rx: Mutex<mpsc::Receiver<Frame>>,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<bool>,
    /// This instance's unique ID.
    pub instance_id: String,
}

impl TunnelConnection {
    /// Send an event to the cloud.
    pub async fn send_event(&self, event: &str, data: serde_json::Value) -> Result<(), TunnelError> {
        self.outgoing_tx
            .send(Frame::Event(EventPayload {
                event: event.into(),
                data,
            }))
            .await
            .map_err(|_| TunnelError::ChannelClosed)
    }

    /// Receive the next command from the cloud (blocking).
    pub async fn recv_command(&self) -> Option<Frame> {
        self.incoming_rx.lock().await.recv().await
    }

    /// Send a response to a cloud command.
    pub async fn respond(
        &self,
        command_id: &str,
        data: Option<serde_json::Value>,
    ) -> Result<(), TunnelError> {
        self.outgoing_tx
            .send(Frame::Response(ResponsePayload {
                id: command_id.into(),
                ok: true,
                data,
                error: None,
            }))
            .await
            .map_err(|_| TunnelError::ChannelClosed)
    }

    /// Send an error response to a cloud command.
    pub async fn respond_error(
        &self,
        command_id: &str,
        error: &str,
    ) -> Result<(), TunnelError> {
        self.outgoing_tx
            .send(Frame::Response(ResponsePayload {
                id: command_id.into(),
                ok: false,
                data: None,
                error: Some(error.into()),
            }))
            .await
            .map_err(|_| TunnelError::ChannelClosed)
    }

    /// Get a clone of the event sender (for the bridge to forward events).
    pub fn event_sender(&self) -> mpsc::Sender<Frame> {
        self.outgoing_tx.clone()
    }

    /// Signal shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Connect to the cloud relay and register this instance.
///
/// Returns a `TunnelConnection` that can be used to send events and receive
/// commands. The WebSocket transport runs in a background task with automatic
/// reconnection.
pub async fn connect(config: TunnelConfig) -> Result<TunnelConnection, TunnelError> {
    let instance_id = uuid::Uuid::new_v4().to_string();

    let (outgoing_tx, outgoing_rx) = mpsc::channel::<Frame>(config.channel_size);
    let (incoming_tx, incoming_rx) = mpsc::channel::<Frame>(config.channel_size);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let auth = auth::AuthToken::from_string(&config.auth_token);

    let transport_config = transport::TransportConfig {
        url: config.relay_url.clone(),
        auth_bearer: auth.bearer().to_string(),
        ..Default::default()
    };

    // Send the registration frame.
    let platform = format!(
        "{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let register_frame = Frame::Register(RegisterPayload {
        instance_id: instance_id.clone(),
        app_kind: config.app_kind,
        display_name: config.display_name.clone(),
        capabilities: config.capabilities.clone(),
        version: config.version.clone(),
        platform,
        cwd: config.cwd.as_ref().map(|p| p.display().to_string()),
        commands: config.commands.clone(),
        metadata: serde_json::Value::Null,
    });

    outgoing_tx
        .send(register_frame)
        .await
        .map_err(|_| TunnelError::ChannelClosed)?;

    // Spawn the transport.
    #[cfg(feature = "reconnect")]
    {
        tokio::spawn(transport::run_with_reconnect(
            transport_config,
            outgoing_rx,
            incoming_tx,
            shutdown_rx,
        ));
    }

    #[cfg(not(feature = "reconnect"))]
    {
        let mut outgoing_rx = outgoing_rx;
        let mut shutdown_rx = shutdown_rx;
        tokio::spawn(async move {
            let _ = transport::run_transport(
                &transport_config,
                &mut outgoing_rx,
                &incoming_tx,
                &mut shutdown_rx,
            )
            .await;
        });
    }

    info!(
        instance_id = %instance_id,
        relay_url = %config.relay_url,
        app_kind = %config.app_kind,
        "tunnel connected"
    );

    Ok(TunnelConnection {
        outgoing_tx,
        incoming_rx: Mutex::new(incoming_rx),
        shutdown_tx,
        instance_id,
    })
}

/// Connect and wait for the registration confirmation from the cloud.
pub async fn connect_and_register(
    config: TunnelConfig,
) -> Result<(TunnelConnection, Option<String>), TunnelError> {
    let conn = connect(config).await?;

    // Wait for the Registered response (with timeout).
    let session_url = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        if let Some(Frame::Registered(RegisteredPayload { session_url, .. })) =
            conn.recv_command().await
        {
            session_url
        } else {
            None
        }
    })
    .await
    .unwrap_or(None);

    debug!(session_url = ?session_url, "registration complete");

    Ok((conn, session_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = TunnelConfig::default();
        assert!(config.relay_url.contains("api.hanzo.ai"));
        assert_eq!(config.channel_size, 256);
        assert!(matches!(config.app_kind, AppKind::Dev));
    }

    #[test]
    fn tunnel_error_display() {
        let err = TunnelError::Connection("refused".into());
        assert_eq!(err.to_string(), "connection error: refused");

        let err = TunnelError::ChannelClosed;
        assert_eq!(err.to_string(), "channel closed");
    }

    #[test]
    fn config_with_all_fields() {
        let config = TunnelConfig {
            relay_url: "ws://localhost:8080".into(),
            auth_token: "test-key".into(),
            app_kind: AppKind::Node,
            display_name: "test-node".into(),
            capabilities: vec!["agent".into(), "p2p".into()],
            commands: vec!["status".into()],
            cwd: Some(PathBuf::from("/tmp")),
            version: "1.0.0".into(),
            channel_size: 64,
        };
        assert_eq!(config.app_kind, AppKind::Node);
        assert_eq!(config.capabilities.len(), 2);
    }
}
