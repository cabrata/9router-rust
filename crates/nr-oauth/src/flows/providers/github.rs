//! GitHub Copilot OAuth provider - device_code flow

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;


pub struct GitHubProvider;

#[async_trait]
impl OAuthProvider for GitHubProvider {
    fn config_id(&self) -> &'static str { "github" }
    fn flow_type(&self) -> &'static str { "device_code" }

    async fn build_auth_url(
        &self,
        _config: &Json,
        _redirect_uri: &str,
        _state: &str,
        _code_challenge: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        Ok(None) // Device code flow has no auth URL upfront
    }

    async fn exchange_token(
        &self,
        _config: &Json,
        _code: &str,
        _redirect_uri: &str,
        _code_verifier: Option<&str>,
        _state: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("GitHub does not support code exchange".into()))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let device_code_url = str_of(config, "deviceCodeUrl")
            .ok_or_else(|| nr_core::Error::Config("GitHub: missing deviceCodeUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("GitHub: missing clientId".into()))?;
        let scopes = str_of(config, "scopes")
            .ok_or_else(|| nr_core::Error::Config("GitHub: missing scopes".into()))?;

        let params = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("scope", scopes),
        ]).map_err(|e| nr_core::Error::Other(e.to_string()))?;

        let response = reqwest::Client::new()
            .post(device_code_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("GitHub device code request failed".into()));
        }

        let data: serde_json::Value = response.json().await?;
        Ok(DeviceCodeResponse {
            device_code: str_of(&data, "device_code")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing device_code".into()))?
                .to_string(),
            user_code: str_of(&data, "user_code")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing user_code".into()))?
                .to_string(),
            verification_uri: str_of(&data, "verification_uri")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing verification_uri".into()))?
                .to_string(),
            verification_uri_complete: None,
            expires_in: data.get("expires_in").and_then(Json::as_i64).unwrap_or(900),
            interval: data.get("interval").and_then(Json::as_i64).unwrap_or(5),
            extra: Some(serde_json::Map::new()),
        })
    }

    async fn poll_device_token(
        &self,
        config: &Json,
        device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let token_url = str_of(config, "tokenUrl")
            .ok_or_else(|| nr_core::Error::Config("GitHub: missing tokenUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("GitHub: missing clientId".into()))?;

        let params = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]).map_err(|e| nr_core::Error::Other(e.to_string()))?;

        let response = reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(params)
            .send()
            .await?;

        let status = response.status().is_success();
        let data: serde_json::Value = response.json().await.unwrap_or_else(|_| json!({}));
        Ok(json!({
            "ok": status,
            "data": data
        }))
    }

    async fn post_exchange(
        &self,
        tokens: &Json,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<Json>> {
        let access_token = str_of(tokens, "access_token")
            .ok_or_else(|| nr_core::Error::BadRequest("Missing access_token".into()))?;
        let api_version = "2022-11-28";

        // Fetch Copilot token
        let copilot_res = reqwest::Client::new()
            .get("https://api.github.com/copilot_internal/token")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("X-GitHub-Api-Version", api_version)
            .header("User-Agent", "9router")
            .send()
            .await;

        let copilot_token = if let Ok(res) = copilot_res {
            res.json::<serde_json::Value>().await.ok()
        } else {
            None
        };

        // Fetch user info
        let user_res = reqwest::Client::new()
            .get("https://api.github.com/user")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("X-GitHub-Api-Version", api_version)
            .header("User-Agent", "9router")
            .send()
            .await;

        let user_info = if let Ok(res) = user_res {
            res.json::<serde_json::Value>().await.ok()
        } else {
            None
        };

        Ok(Some(json!({
            "copilotToken": copilot_token,
            "userInfo": user_info
        })))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        let user_info = extra.and_then(|e| e.get("userInfo"));
        Ok(MappedTokens {
            access_token: str_of(tokens, "access_token")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing access_token".into()))?
                .to_string(),
            refresh_token: str_of(tokens, "refresh_token").map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(Json::as_i64),
            expires_at: None,
            name: user_info.and_then(|u| str_of(u, "login")).map(|s| s.to_string()),
            display_name: user_info.and_then(|u| str_of(u, "name")).map(|s| s.to_string()),
            email: user_info.and_then(|u| str_of(u, "email")).map(|s| s.to_string()),
            project_id: None,
            id_token: None,
            scope: None,
            provider_specific_data: extra.and_then(|v| v.as_object().cloned()),
        })
    }
}

pub static PROVIDER: GitHubProvider = GitHubProvider;
