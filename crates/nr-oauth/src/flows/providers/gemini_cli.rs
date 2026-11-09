//! Google Gemini CLI OAuth provider - standard OAuth2

use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;

pub struct GeminiCliProvider;

impl OAuthProvider for GeminiCliProvider {
    fn config_id(&self) -> &'static str { "gemini-cli" }
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
        let authorize_url = str_of(config, "authorizeUrl")
            .ok_or_else(|| nr_core::Error::Config("Gemini: missing authorizeUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("Gemini: missing clientId".into()))?;

        let params = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("code_challenge", code_challenge.unwrap_or("")),
            ("code_challenge_method", if code_challenge.is_some() { "S256" } else { "" }),
        ])?;

        Ok(Some(format!("{}?{}", authorize_url, params)))
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
        let token_url = str_of(config, "tokenUrl")
            .ok_or_else(|| nr_core::Error::Config("Gemini: missing tokenUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("Gemini: missing clientId".into()))?;
        let client_secret = str_of(config, "clientSecret")
            .ok_or_else(|| nr_core::Error::Config("Gemini: missing clientSecret".into()))?;

        let mut body = json!({
            "code": code,
            "grant_type": "authorization_code",
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": redirect_uri,
        });

        if let Some(verifier) = code_verifier {
            body["code_verifier"] = json!(verifier);
        }

        let response = reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("Gemini token exchange failed".into()));
        }

        Ok(response.json().await?)
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::NotSupported("Gemini does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("Gemini does not support device code flow".into()))
    }

    async fn post_exchange(
        &self,
        _tokens: &Json,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<Json>> {
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        Ok(MappedTokens {
            access_token: str_of(tokens, "access_token")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing access_token".into()))?
                .to_string(),
            refresh_token: str_of(tokens, "refresh_token").map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(Json::as_i64),
            id_token: str_of(tokens, "id_token").map(|s| s.to_string()),
            email: None,
            display_name: None,
            name: None,
            project_id: None,
            scope: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: GeminiCliProvider = GeminiCliProvider;
