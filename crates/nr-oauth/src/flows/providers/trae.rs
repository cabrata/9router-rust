//! Trae (ByteDance Marscode) OAuth provider — custom authorization flow.
//! Port of src/lib/oauth/providers/trae.js
//! Flow type: "authorization_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct TraeProvider;

#[async_trait]
impl OAuthProvider for TraeProvider {
    fn config_id(&self) -> &'static str { "trae" }
    fn flow_type(&self) -> &'static str { "authorization_code" }

    async fn build_auth_url(
        &self,
        config: &Json,
        redirect_uri: &str,
        state: &str,
        _code_challenge: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        // TODO: Fetch login guidance, build verification URL with device context
        let client_id = config.get("clientId").and_then(|v| v.as_str()).unwrap_or("");

        let params = vec![
            ("client_id", client_id),
            ("auth_callback_url", redirect_uri),
            ("login_trace_id", state),
        ];

        let qs = params.into_iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        Ok(Some(format!("https://login.trae.ai/authorization?{}", qs)))
    }

    async fn exchange_token(
        &self,
        config: &Json,
        code: &str,
        _redirect_uri: &str,
        _code_verifier: Option<&str>,
        _state: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // TODO: Parse callback URL, exchange refresh token for access token
        let client_id = config.get("clientId").and_then(|v| v.as_str()).unwrap_or("");

        let mut result = serde_json::Map::new();
        result.insert("_exchange_code".to_string(), json!(code));
        result.insert("_client_id".to_string(), json!(client_id));

        Ok(Json::Object(result))
    }

    async fn request_device_code(
        &self, _config: &Json, _code_challenge: Option<&str>, _options: Option<&Json>, _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::new("Trae does not support device code flow"))
    }

    async fn poll_device_token(
        &self, _config: &Json, _device_code: &str, _code_verifier: Option<&str>, _extra_data: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Trae does not support device code flow"))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        // TODO: Fetch user info from Trae API with x-cloudide-token header
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("accessToken")
            .or_else(|| tokens.get("access_token"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Trae: missing accessToken"));
        }

        Ok(MappedTokens {
            access_token,
            refresh_token: tokens.get("refreshToken")
                .or_else(|| tokens.get("refresh_token"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            expires_in: tokens.get("expiresIn").or_else(|| tokens.get("expires_in"))
                .and_then(|v| v.as_i64()),
            expires_at: None,
            scope: None,
            project_id: None,
            id_token: None,
            email: None,
            display_name: None,
            name: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: TraeProvider = TraeProvider;
