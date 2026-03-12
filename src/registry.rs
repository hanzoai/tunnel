//! Instance registry types — used by the cloud relay and local gateways to
//! track connected Hanzo app instances.

use crate::protocol::AppKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A registered instance visible in the cloud.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub instance_id: String,
    pub app_kind: AppKind,
    pub display_name: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub connected_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
    #[serde(default)]
    pub exposed: Vec<crate::expose::ExposedUrl>,
    #[serde(default)]
    pub metadata: Value,
}

/// Parameters for invoking a command on a remote instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeParams {
    pub instance_id: String,
    pub command: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

/// Result of invoking a command on a remote instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_instance() {
        let inst = Instance {
            instance_id: "abc".into(),
            app_kind: AppKind::Dev,
            display_name: "z-macbook".into(),
            capabilities: vec!["chat".into()],
            commands: vec![],
            version: "0.6.74".into(),
            platform: "darwin-arm64".into(),
            cwd: Some("/Users/z/work".into()),
            connected: true,
            connected_at_ms: 1234567890000,
            session_url: Some("https://app.hanzo.bot/i/abc".into()),
            exposed: vec![],
            metadata: Value::Null,
        };
        let json = serde_json::to_string(&inst).unwrap();
        assert!(json.contains("\"app_kind\":\"dev\""));
        assert!(json.contains("\"connected\":true"));
    }

    #[test]
    fn invoke_params_default_timeout() {
        let json = r#"{"instance_id":"abc","command":"chat.send","params":{"message":"hi"}}"#;
        let params: InvokeParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.timeout_ms, 30_000);
    }

    #[test]
    fn invoke_result_roundtrip() {
        let result = InvokeResult {
            ok: true,
            payload: Some(serde_json::json!({"turn_id": "t1"})),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("\"error\""));
        let parsed: InvokeResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.ok);
    }
}
