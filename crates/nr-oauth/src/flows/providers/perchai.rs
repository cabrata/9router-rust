//! Perch AI OAuth provider — Supabase PKCE with multi-input support.
//! Port of src/lib/oauth/providers/perchai.js
//! Flow type: "authorization_code_pkce"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct PerchaiProvider;

#[async_trait]
impl OAuthProvider for PerchaiProvider {
    fn config_id(&self) -> &'static str { "perchai" }
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
        let auth_url = config.get("authorizeUrl").and_then(|v| v.as_str()).unwrap_or("https://auth.perchai.app");

        let mut params = vec![
            ("provider", "google"),
            ("redirect_to", redirect_uri),
            ("state", state),
        ];

        if let Some(challenge) = code_challenge {
            params.push(("code_challenge", challenge));
            params.push(("code_challenge_method", "s256"));
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
        _redirect_uri: &str,
        code_verifier: Option<&str>,
        _state: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // TODO: Support multiple input formats: JSON session, JWT, callback URL, PKCE code
        let token_url = config.get("tokenUrl").and_then(|v| v.as_str()).unwrap_or("");
        let mut result = serde_json::Map::new();
        result.insert("access_token".to_string(), json!(code));
        if let Some(verifier) = code_verifier {
            result.insert("_verifier".to_string(), json!(verifier));
        }
        result.insert("_token_url".to_string(), json!(token_url));
        Ok(Json::Object(result))
    }

    async fn request_device_code(
        &self, _config: &Json, _code_challenge: Option<&str>, _options: Option<&Json>, _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::new("Perch AI does not support device code flow"))
    }

    async fn poll_device_token(
        &self, _config: &Json, _device_code: &str, _code_verifier: Option<&str>, _extra_data: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Perch AI does not support device code flow"))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Perch AI: missing access_token"));
        }

        Ok(MappedTokens {
            access_token,
            refresh_token: tokens.get("refresh_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(|v| v.as_i64()),
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

pub static PROVIDER: PerchaiProvider = PerchaiProvider;
