//! Muse OAuth provider — device code + Model API key mint.
//! Port of src/lib/oauth/providers/muse.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct MuseProvider;

#[async_trait]
impl OAuthProvider for MuseProvider {
    fn config_id(&self) -> &'static str { "muse" }
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
        Err(nr_core::Error::new("Muse: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        // TODO: POST to config.deviceCodeUrl with client_id
        Ok(DeviceCodeResponse {
            device_code: "MUSE_DEVICE_CODE".to_string(),
            user_code: "MUSE_USER_CODE".to_string(),
            verification_uri: "https://auth.meta.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
            extra: None,
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
        // TODO: POST to config.tokenUrl with device_code
        let mut result = serde_json::Map::new();
        result.insert("error".to_string(), json!("authorization_pending"));
        result.insert("_device_code".to_string(), json!(device_code));
        Ok(Json::Object(result))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        // TODO: Mint subscription API key via POST to https://api.meta.ai/muse-code/key
        // Returns: { api_key, user_email, is_subs_active, subs_tier_name }
        Ok(Some(json!({ "key": serde_json::Value::Null })))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        // Muse uses the minted API key as the credential, not the OAuth token
        let api_key = extra
            .and_then(|e| e.get("key"))
            .and_then(|k| k.get("api_key"))
            .and_then(|v| v.as_str())
            .unwrap_or(tokens.get("access_token").and_then(|v| v.as_str()).unwrap_or(""));

        if api_key.is_empty() {
            return Err(nr_core::Error::new("Muse: missing api_key"));
        }

        let email = extra
            .and_then(|e| e.get("key"))
            .and_then(|k| k.get("user_email"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(MappedTokens {
            access_token: api_key.to_string(),
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            scope: None,
            project_id: None,
            id_token: None,
            email,
            display_name: None,
            provider_specific_data: None,
            ..Default::default()
        })
    }
}

pub static PROVIDER: MuseProvider = MuseProvider;
