//! OAuth flows: authorize URL generation, code exchange, device code polling.
//! Ports src/lib/oauth/** and src/app/api/oauth/** (dashboard OAuth routes).
//!
//! Each provider handles its own flow type (authorization_code, device_code, etc.)
//! through the OAuthProvider trait in flows/providers/mod.rs.

pub mod constants;
pub mod providers;
pub mod utils;

use serde::{Deserialize, Serialize};
use nr_core::Json;

/// Mapped tokens in standard connection record format.
/// Returned by OAuthProvider::map_tokens(...).
/// Port of the connection record schema from src/lib/oauth services.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MappedTokens {
    #[serde(rename = "accessToken")]
    pub access_token: String,

    #[serde(rename = "refreshToken")]
    pub refresh_token: Option<String>,

    #[serde(rename = "expiresIn")]
    pub expires_in: Option<i64>,

    #[serde(rename = "expiresAt")]
    pub expires_at: Option<i64>,

    /// User email (best-effort, non-fatal if missing)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Display name (best-effort, non-fatal if missing)
    #[serde(rename = "displayName")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Provider-specific fields (scope, idToken, userId, etc.)
    #[serde(rename = "providerSpecificData")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_specific_data: Option<serde_json::Map<String, Json>>,
}

impl MappedTokens {
    /// Convert to a JSON value for storage/transmission
    pub fn to_json(&self) -> Json {
        serde_json::to_value(self).unwrap_or(Json::Null)
    }

    /// Convert to a JSON map for storage/transmission
    pub fn to_map(&self) -> serde_json::Map<String, Json> {
        match serde_json::to_value(self).ok() {
            Some(Json::Object(m)) => m,
            _ => serde_json::Map::new(),
        }
    }
}

/// Device code flow response: user code and verification URI.
/// Port of DeviceCodeResponse from JS src/lib/oauth/services.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,

    #[serde(rename = "verificationUri")]
    pub verification_uri: String,

    #[serde(rename = "verificationUriComplete")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,

    #[serde(rename = "expiresIn")]
    pub expires_in: i64,

    /// Poll interval in seconds
    #[serde(default = "default_interval")]
    pub interval: i64,

    /// Provider-specific extra data (e.g. _minimaxPollByUser, _zcodePollToken) for polling
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Map<String, Json>>,
}

fn default_interval() -> i64 {
    5
}

pub use providers::{OAuthProvider, get_provider};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mapped_tokens_to_json() {
        let tokens = MappedTokens {
            access_token: "test_token".to_string(),
            refresh_token: None,
            expires_in: Some(3600),
            expires_at: None,
            email: Some("user@example.com".to_string()),
            display_name: None,
            provider_specific_data: None,
        };

        let json = tokens.to_json();
        assert_eq!(json.get("accessToken").and_then(|v| v.as_str()), Some("test_token"));
        assert_eq!(json.get("email").and_then(|v| v.as_str()), Some("user@example.com"));
    }

    #[test]
    fn test_device_code_response() {
        let resp = DeviceCodeResponse {
            device_code: "ABC123".to_string(),
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://example.com/verify".to_string(),
            verification_uri_complete: Some("https://example.com/verify?code=ABC123".to_string()),
            expires_in: 600,
            interval: 5,
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json.get("deviceCode").and_then(|v| v.as_str()), Some("ABC123"));
        assert_eq!(json.get("interval").and_then(|v| v.as_i64()), Some(5));
    }
}
