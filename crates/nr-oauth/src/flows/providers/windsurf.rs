//! Windsurf OAuth provider — Firebase OAuth + API key registration.
//! Port of src/lib/oauth/providers/windsurf.js
//! Flow type: "authorization_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct WindsurfProvider;

#[async_trait]
impl OAuthProvider for WindsurfProvider {
    fn config_id(&self) -> &'static str { "windsurf" }
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
        let client_id = config.get("clientId").and_then(|v| v.as_str()).unwrap_or("");
        let auth_url = config.get("authBaseUrl").and_then(|v| v.as_str()).unwrap_or("https://www.windsurf.com");
        let sign_in_path = config.get("signInPath").and_then(|v| v.as_str()).unwrap_or("/windsurf/signin");

        let params = [
            ("response_type", "token"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("prompt", "login"),
        ];

        let qs = params.into_iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        Ok(Some(format!("{}{}{}{}", auth_url, sign_in_path, "?", qs)))
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
        // TODO: Parse callback URL and register user to get API key
        let mut result = serde_json::Map::new();
        result.insert("firebaseIdToken".to_string(), json!(code));
        result.insert("_exchange_code".to_string(), json!(code));
        Ok(Json::Object(result))
    }

    async fn request_device_code(
        &self, _config: &Json, _code_challenge: Option<&str>, _options: Option<&Json>, _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::new("Windsurf does not support device code flow"))
    }

    async fn poll_device_token(
        &self, _config: &Json, _device_code: &str, _code_verifier: Option<&str>, _extra_data: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Windsurf does not support device code flow"))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        // TODO: Call RegisterUser endpoint and fetch user info
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("apiKey")
            .or_else(|| tokens.get("accessToken"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Windsurf: missing apiKey"));
        }

        Ok(MappedTokens {
            access_token,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            scope: None,
            project_id: None,
            id_token: None,
            email: None,
            display_name: None,
            provider_specific_data: None,
            ..Default::default()
        })
    }
}

pub static PROVIDER: WindsurfProvider = WindsurfProvider;
