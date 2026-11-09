//! Claude OAuth provider - authorization_code_pkce flow

use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;

pub struct ClaudeProvider;

impl OAuthProvider for ClaudeProvider {
    fn config_id(&self) -> &'static str { "claude" }
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
            .ok_or_else(|| nr_core::Error::Config("Claude: missing authorizeUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("Claude: missing clientId".into()))?;
        let scopes = config.get("scopes")
            .and_then(Json::as_array)
            .ok_or_else(|| nr_core::Error::Config("Claude: missing scopes".into()))?;
        let code_challenge = code_challenge
            .ok_or_else(|| nr_core::Error::Config("Claude: PKCE code_challenge required".into()))?;

        let scope = scopes.iter()
            .filter_map(Json::as_str)
            .collect::<Vec<_>>()
            .join(" ");

        let params = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", &scope),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("code", "true"),
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
            .ok_or_else(|| nr_core::Error::Config("Claude: missing tokenUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("Claude: missing clientId".into()))?;
        let code_verifier = code_verifier
            .ok_or_else(|| nr_core::Error::BadRequest("Claude: code_verifier required".into()))?;

        let body = json!({
            "code": code,
            "grant_type": "authorization_code",
            "client_id": client_id,
            "redirect_uri": redirect_uri,
            "code_verifier": code_verifier,
        });

        let response = reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(nr_core::Error::Provider(format!("Claude token exchange failed: {}", text)));
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
        Err(nr_core::Error::NotSupported("Claude does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("Claude does not support device code flow".into()))
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
            scope: str_of(tokens, "scope").map(|s| s.to_string()),
            email: None,
            display_name: None,
            name: None,
            project_id: None,
            id_token: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: ClaudeProvider = ClaudeProvider;
