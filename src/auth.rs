//! Auth token handling for tunnel connections.

use serde::{Deserialize, Serialize};

/// An authentication token for the tunnel relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthToken {
    /// A JWT from hanzo.id.
    Jwt { token: String },
    /// A Hanzo API key.
    ApiKey { key: String },
}

impl AuthToken {
    /// Auto-detect token type from a raw string.
    /// JWTs have dots (header.payload.signature), API keys don't.
    pub fn from_string(s: &str) -> Self {
        if s.contains('.') && s.split('.').count() >= 3 {
            AuthToken::Jwt {
                token: s.to_string(),
            }
        } else {
            AuthToken::ApiKey {
                key: s.to_string(),
            }
        }
    }

    /// Get the bearer value for an Authorization header.
    pub fn bearer(&self) -> &str {
        match self {
            AuthToken::Jwt { token } => token,
            AuthToken::ApiKey { key } => key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_jwt() {
        let token = AuthToken::from_string("eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(matches!(token, AuthToken::Jwt { .. }));
        assert!(token.bearer().starts_with("eyJ"));
    }

    #[test]
    fn detect_api_key() {
        let token = AuthToken::from_string("hk-abc123def456");
        assert!(matches!(token, AuthToken::ApiKey { .. }));
        assert_eq!(token.bearer(), "hk-abc123def456");
    }
}
