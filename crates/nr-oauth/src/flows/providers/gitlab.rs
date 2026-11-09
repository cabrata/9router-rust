//! GitLab Duo OAuth provider - authorization_code_pkce flow

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;

pub struct GitLabProvider;

#[async_trait]
impl OAuthProvider for GitLabProvider {
    fn config_id(&self) -> &'static str { "gitlab" }
    fn flow_type(&self) -> &'static str { "authorization_code_pkce" }

    async fn build_auth_url(
        &self,
        config: &Json,
        redirect_uri: &str,
        state: &str,
        code_challenge: Option<&str>,
        meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        let base_url = meta.and_then(|m| str_of(m, "baseUrl"))
            .or_else(|| str_of(config, "baseUrl"))
            .unwrap_or("https://gitlab.com");
        let client_id = meta.and_then(|m| str_of(m, "clientId"))
            .or_else(|| str_of(config, "clientId"))
            .ok_or_else(|| nr_core::Error::Config("GitLab: missing clientId".into()))?;
        let scopes = config.get("scopes")
            .and_then(Json::as_array)
            .ok_or_else(|| nr_core::Error::Config("GitLab: missing scopes".into()))?;

        let scope = scopes.iter()
            .filter_map(Json::as_str)
            .collect::<Vec<_>>()
            .join(" ");

        let params = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", &scope),
            ("state", state),
            ("code_challenge", code_challenge.unwrap_or("")),
            ("code_challenge_method", if code_challenge.is_some() { "S256" } else { "" }),
        ]).map_err(|e| nr_core::Error::Other(e.to_string()))?;

        Ok(Some(format!("{}/oauth/authorize?{}", base_url, params)))
    }

    async fn exchange_token(
        &self,
        config: &Json,
        code: &str,
        redirect_uri: &str,
        code_verifier: Option<&str>,
        _state: Option<&str>,
        meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let base_url = meta.and_then(|m| str_of(m, "baseUrl"))
            .or_else(|| str_of(config, "baseUrl"))
            .unwrap_or("https://gitlab.com");
        let client_id = meta.and_then(|m| str_of(m, "clientId"))
            .or_else(|| str_of(config, "clientId"))
            .ok_or_else(|| nr_core::Error::Config("GitLab: missing clientId".into()))?;
        let client_secret = meta.and_then(|m| str_of(m, "clientSecret"))
            .or_else(|| str_of(config, "clientSecret"))
            .ok_or_else(|| nr_core::Error::Config("GitLab: missing clientSecret".into()))?;

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
            .post(format!("{}/oauth/token", base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("GitLab token exchange failed".into()));
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
        Err(nr_core::Error::NotSupported("GitLab does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("GitLab does not support device code flow".into()))
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

pub static PROVIDER: GitLabProvider = GitLabProvider;
