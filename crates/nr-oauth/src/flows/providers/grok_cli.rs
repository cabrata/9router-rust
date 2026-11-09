//! Grok CLI OAuth provider — device code flow.
//! Port of src/lib/oauth/providers/grok-cli.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct GrokCliProvider;

#[async_trait]
impl OAuthProvider for GrokCliProvider {
    fn config_id(&self) -> &'static str { "grok-cli" }
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
        Err(nr_core::Error::new("Grok CLI: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        // TODO: POST to config.deviceCodeUrl with client_id and scope
        let device_code_url = config.get("deviceCodeUrl").and_then(|v| v.as_str())
            .unwrap_or("https://auth.x.ai/device");

        Ok(DeviceCodeResponse {
            device_code: "GROK_DEVICE_CODE_PLACEHOLDER".to_string(),
            user_code: "GROK_USER_CODE".to_string(),
            verification_uri: format!("{}/verify", device_code_url),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
            extra: None,
        })
    }

    async fn poll_device_token(
        &self,
        config: &Json,
        device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // TODO: POST to config.tokenUrl with device_code
        let token_url = config.get("tokenUrl").and_then(|v| v.as_str()).unwrap_or("");

        let mut result = serde_json::Map::new();
        result.insert("error".to_string(), json!("authorization_pending"));
        result.insert("_device_code".to_string(), json!(device_code));
        result.insert("_token_url".to_string(), json!(token_url));

        Ok(Json::Object(result))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        // TODO: Fetch user info from cli-chat-proxy.grok.com/v1/user with Bearer token
        Ok(Some(json!({ "user": serde_json::Value::Null })))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Grok CLI: missing access_token"));
        }

        let user_email = extra
            .and_then(|e| e.get("user"))
            .and_then(|u| u.get("email"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(MappedTokens {
            access_token,
            refresh_token: tokens.get("refresh_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(|v| v.as_i64()),
            expires_at: None,
            scope: None,
            project_id: None,
            id_token: None,
            email: user_email,
            display_name: None,
            name: None,
            provider_specific_data: Some(serde_json::Map::new()),
        })
    }
}

pub static PROVIDER: GrokCliProvider = GrokCliProvider;
