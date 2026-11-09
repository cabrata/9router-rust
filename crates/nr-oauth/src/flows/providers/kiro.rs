//! AWS Kiro OAuth provider — AWS SSO OIDC device code flow.
//! Port of src/lib/oauth/providers/kiro.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct KiroProvider;

#[async_trait]
impl OAuthProvider for KiroProvider {
    fn config_id(&self) -> &'static str { "kiro" }
    fn flow_type(&self) -> &'static str { "device_code" }

    async fn build_auth_url(
        &self, _config: &Json, _redirect_uri: &str, _state: &str, _code_challenge: Option<&str>,
        _meta: Option<&Json>, _log: &Logger,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    async fn exchange_token(
        &self, _config: &Json, _code: &str, _redirect_uri: &str, _code_verifier: Option<&str>,
        _state: Option<&str>, _meta: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Kiro: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        // TODO: Implement AWS SSO OIDC client registration and device auth
        Ok(DeviceCodeResponse {
            device_code: "KIRO_DEVICE_CODE".to_string(),
            user_code: "KIRO_USER_CODE".to_string(),
            verification_uri: "https://oidc.amazonaws.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
        })
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let mut result = serde_json::Map::new();
        result.insert("error".to_string(), json!("authorization_pending"));
        result.insert("_device_code".to_string(), json!(device_code));
        Ok(Json::Object(result))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Kiro: missing access_token"));
        }

        Ok(MappedTokens {
            access_token,
            refresh_token: tokens.get("refresh_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(|v| v.as_i64()),
            expires_at: None,
            email: None,
            display_name: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: KiroProvider = KiroProvider;
