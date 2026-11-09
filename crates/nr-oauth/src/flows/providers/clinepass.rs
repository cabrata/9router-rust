//! ClinePass OAuth provider - shares Cline's endpoints

use super::*;
use nr_core::{Result, Logger, Json};

pub struct ClinePassProvider;

impl OAuthProvider for ClinePassProvider {
    fn config_id(&self) -> &'static str { "clinepass" }
    fn flow_type(&self) -> &'static str { "authorization_code" }

    async fn build_auth_url(
        &self,
        config: &Json,
        redirect_uri: &str,
        _state: &str,
        _code_challenge: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        let authorize_url = nr_core::json::str_of(config, "authorizeUrl")
            .ok_or_else(|| nr_core::Error::Config("ClinePass: missing authorizeUrl".into()))?;

        let params = serde_urlencoded::to_string([
            ("client_type", "extension"),
            ("callback_url", redirect_uri),
            ("redirect_uri", redirect_uri),
        ])?;

        Ok(Some(format!("{}?{}", authorize_url, params)))
    }

    async fn exchange_token(
        &self,
        config: &Json,
        code: &str,
        redirect_uri: &str,
        _code_verifier: Option<&str>,
        _state: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let token_url = nr_core::json::str_of(config, "tokenExchangeUrl")
            .ok_or_else(|| nr_core::Error::Config("ClinePass: missing tokenExchangeUrl".into()))?;

        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_type": "extension",
            "redirect_uri": redirect_uri,
        });

        let response = reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("ClinePass token exchange failed".into()));
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
        Err(nr_core::Error::NotSupported("ClinePass does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("ClinePass does not support device code flow".into()))
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
            access_token: nr_core::json::str_of(tokens, "access_token")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing access_token".into()))?
                .to_string(),
            refresh_token: nr_core::json::str_of(tokens, "refresh_token").map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(Json::as_i64),
            email: None,
            display_name: None,
            name: None,
            project_id: None,
            id_token: None,
            scope: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: ClinePassProvider = ClinePassProvider;
