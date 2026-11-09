//! iFlow OAuth provider - authorization_code flow

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;

pub struct IFlowProvider;

#[async_trait]
impl OAuthProvider for IFlowProvider {
    fn config_id(&self) -> &'static str { "iflow" }
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
        let authorize_url = str_of(config, "authorizeUrl")
            .ok_or_else(|| nr_core::Error::Config("iFlow: missing authorizeUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("iFlow: missing clientId".into()))?;
        let scopes = str_of(config, "scopes")
            .unwrap_or("openid profile email");

        let params = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", scopes),
            ("state", state),
        ]).map_err(|e| nr_core::Error::Other(e.to_string()))?;

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
        let token_url = str_of(config, "tokenUrl")
            .ok_or_else(|| nr_core::Error::Config("iFlow: missing tokenUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("iFlow: missing clientId".into()))?;
        let client_secret = str_of(config, "clientSecret")
            .ok_or_else(|| nr_core::Error::Config("iFlow: missing clientSecret".into()))?;

        let params = serde_urlencoded::to_string([
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ]).map_err(|e| nr_core::Error::Other(e.to_string()))?;

        let response = reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .json(&json!({"grant_type": "authorization_code"}))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("iFlow token exchange failed".into()));
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
        Err(nr_core::Error::NotSupported("iFlow does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("iFlow does not support device code flow".into()))
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
            expires_at: None,
            email: None,
            display_name: None,
            name: None,
            project_id: None,
            id_token: str_of(tokens, "id_token").map(|s| s.to_string()),
            scope: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: IFlowProvider = IFlowProvider;
