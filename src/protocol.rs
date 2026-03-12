//! Wire protocol types for the tunnel connection.
//!
//! All communication between a Hanzo app instance and the cloud relay is
//! via JSON frames over WebSocket. Each frame has a `type` discriminator.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A wire frame — the unit of communication over the tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// Instance → Cloud: Register this instance.
    Register(RegisterPayload),
    /// Cloud → Instance: Registration confirmed.
    Registered(RegisteredPayload),
    /// Instance → Cloud: An event (streaming output, state change, etc.).
    Event(EventPayload),
    /// Cloud → Instance: A command (chat message, approval, config).
    Command(CommandPayload),
    /// Instance → Cloud: Response to a command.
    Response(ResponsePayload),
    /// Bidirectional heartbeat.
    Ping,
    /// Bidirectional heartbeat response.
    Pong,
}

/// What kind of Hanzo app this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    Dev,
    Node,
    Desktop,
    Bot,
    Extension,
}

impl std::fmt::Display for AppKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppKind::Dev => write!(f, "dev"),
            AppKind::Node => write!(f, "node"),
            AppKind::Desktop => write!(f, "desktop"),
            AppKind::Bot => write!(f, "bot"),
            AppKind::Extension => write!(f, "extension"),
        }
    }
}

/// Registration payload sent by the instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub instance_id: String,
    pub app_kind: AppKind,
    pub display_name: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
}

/// Registration confirmation from the cloud.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredPayload {
    pub instance_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
}

/// An event frame from instance to cloud.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPayload {
    pub event: String,
    pub data: Value,
}

/// A command frame from cloud to instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPayload {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A response to a cloud command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsePayload {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_register() {
        let frame = Frame::Register(RegisterPayload {
            instance_id: "abc-123".into(),
            app_kind: AppKind::Dev,
            display_name: "z-macbook".into(),
            capabilities: vec!["chat".into(), "exec".into()],
            version: "0.6.74".into(),
            platform: "darwin-arm64".into(),
            cwd: Some("/Users/z/work".into()),
            commands: vec![],
            metadata: Value::Null,
        });

        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"register\""));
        assert!(json.contains("\"app_kind\":\"dev\""));
    }

    #[test]
    fn deserialize_registered() {
        let json = r#"{"type":"registered","instance_id":"abc-123","session_url":"https://app.hanzo.bot/i/abc-123"}"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::Registered(r) => {
                assert_eq!(r.instance_id, "abc-123");
                assert_eq!(
                    r.session_url.unwrap(),
                    "https://app.hanzo.bot/i/abc-123"
                );
            }
            _ => panic!("expected Registered frame"),
        }
    }

    #[test]
    fn serialize_event() {
        let frame = Frame::Event(EventPayload {
            event: "chat.delta".into(),
            data: serde_json::json!({"text": "Let me fix..."}),
        });
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"event\""));
        assert!(json.contains("chat.delta"));
    }

    #[test]
    fn deserialize_command() {
        let json = r#"{"type":"command","id":"req-1","method":"chat.send","params":{"message":"fix the bug"}}"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::Command(c) => {
                assert_eq!(c.id, "req-1");
                assert_eq!(c.method, "chat.send");
                assert_eq!(c.params["message"], "fix the bug");
            }
            _ => panic!("expected Command frame"),
        }
    }

    #[test]
    fn serialize_response() {
        let frame = Frame::Response(ResponsePayload {
            id: "req-1".into(),
            ok: true,
            data: Some(serde_json::json!({"turn_id": "t1"})),
            error: None,
        });
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"ok\":true"));
    }

    #[test]
    fn ping_pong_roundtrip() {
        let ping_json = serde_json::to_string(&Frame::Ping).unwrap();
        assert!(ping_json.contains("\"type\":\"ping\""));

        let pong: Frame = serde_json::from_str(&ping_json.replace("ping", "pong")).unwrap();
        assert!(matches!(pong, Frame::Pong));
    }

    #[test]
    fn app_kind_display() {
        assert_eq!(AppKind::Dev.to_string(), "dev");
        assert_eq!(AppKind::Node.to_string(), "node");
        assert_eq!(AppKind::Extension.to_string(), "extension");
    }
}
