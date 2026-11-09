//! xAI OAuth provider — PKCE authorization code flow with dynamic endpoint discovery.
//! Port of src/lib/oauth/providers/xai.js
//! Flow type: "authorization_code_pkce"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct XaiProvider;

#[async_trait]
impl OAuthProvider for XaiProvider {
    fn config_id(&self) -> &'static str { "xai" }
    fn flow_type(&self) -> &'static str { "authorization_code_pkce" }

    async fn build_auth_url(
        &self,
        config: &Json,
        redirect_uri: &str,
        state: &str,
        code_challenge: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        let client_id = config.get("clientId").and_then(|v| v.as_str()).unwrap_or("");
        let scope = config.get("scope").and_then(|v| v.as_str()).unwrap_or("");
        let auth_url = config.get("authorizeUrl").and_then(|v| v.as_str())
            .unwrap_or("https://auth.x.ai/authorize");

        let nonce = uuid::Uuid::new_v4().to_string();
        let mut params = vec![
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", scope),
            ("state", state),
            ("nonce", &nonce),
            ("plan", "generic"),
            ("referrer", "cli-proxy-api"),
        ];

        if let Some(challenge) = code_challenge {
            params.push(("code_challenge", challenge));
            params.push(("code_challenge_method", "S256"));
        }

        let qs = params.into_iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        Ok(Some(format!("{}?{}", auth_url, qs)))
    }

    async fn exchange_token(
        &self,
        config: &Json,
        code: &str,
        redirect_uri: &str,
        code_verifier: Option<&str>,
        _state: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let token_url = config.get("tokenUrl").and_then(|v| v.as_str())
            .unwrap_or("https://auth.x.ai/token");
        let client_id = config.get("clientId").and_then(|v| v.as_str()).unwrap_or("");

        // TODO: POST to tokenUrl with code, client_id, code_verifier, redirect_uri
        // Returns { access_token, refresh_token, expires_in, id_token, scope }

        let mut result = serde_json::Map::new();
        result.insert("access_token".to_string(), json!(code)); // Placeholder
        result.insert("_exchange_url".to_string(), json!(token_url));
        result.insert("_client_id".to_string(), json!(client_id));
        if let Some(verifier) = code_verifier {
            result.insert("_code_verifier".to_string(), json!(verifier));
        }
        Ok(Json::Object(result))
    }

    async fn request_device_code(
        &self, _config: &Json, _code_challenge: Option<&str>, _options: Option<&Json>, _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::new("xAI does not support device code flow"))
    }

    async fn poll_device_token(
        &self, _config: &Json, _device_code: &str, _code_verifier: Option<&str>, _extra_data: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("xAI does not support device code flow"))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("xAI: missing access_token"));
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

pub static PROVIDER: XaiProvider = XaiProvider;
