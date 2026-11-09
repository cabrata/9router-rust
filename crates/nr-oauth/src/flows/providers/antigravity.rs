//! Google Antigravity OAuth provider - authorization_code flow with custom postExchange

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;

pub struct AntigravityProvider;

#[async_trait]
impl OAuthProvider for AntigravityProvider {
    fn config_id(&self) -> &'static str { "antigravity" }
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
            .ok_or_else(|| nr_core::Error::Config("Antigravity: missing authorizeUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("Antigravity: missing clientId".into()))?;
        let scopes = config.get("scopes")
            .and_then(Json::as_array)
            .ok_or_else(|| nr_core::Error::Config("Antigravity: missing scopes".into()))?;

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
            ("access_type", "offline"),
            ("prompt", "consent"),
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
            .ok_or_else(|| nr_core::Error::Config("Antigravity: missing tokenUrl".into()))?;
        let client_id = str_of(config, "clientId")
            .ok_or_else(|| nr_core::Error::Config("Antigravity: missing clientId".into()))?;
        let client_secret = str_of(config, "clientSecret")
            .ok_or_else(|| nr_core::Error::Config("Antigravity: missing clientSecret".into()))?;

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
            .header("Accept", "application/json")
            .body(params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("Antigravity token exchange failed".into()));
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
        Err(nr_core::Error::NotSupported("Antigravity does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("Antigravity does not support device code flow".into()))
    }

    async fn post_exchange(
        &self,
        tokens: &Json,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<Json>> {
        let access_token = str_of(tokens, "access_token")
            .ok_or_else(|| nr_core::Error::BadRequest("Antigravity: missing access_token".into()))?;

        // The transport config carries the endpoints; the provider row has none.
        let config = nr_data::provider_oauth("antigravity").cloned().unwrap_or_else(|| json!({}));
        let user_info_url = str_of(&config, "userInfoUrl")
            .unwrap_or("https://www.googleapis.com/oauth2/v1/userinfo");

        // Fetch user info
        let user_res = reqwest::Client::new()
            .get(format!("{user_info_url}?alt=json"))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("x-request-source", "local")
            .send()
            .await;

        let user_info: Option<Json> = if let Ok(res) = user_res {
            res.json().await.ok()
        } else {
            None
        };

        // Load Code Assist for the project id and default tier. A failure only
        // leaves projectId empty, exactly like the JS try/catch.
        let mut project_id = String::new();
        let mut tier_id = "legacy-tier".to_string();
        if let Some(load_url) = str_of(&config, "loadCodeAssistEndpoint") {
            let ua = str_of(&config, "loadCodeAssistUserAgent")
                .unwrap_or("antigravity/hub/2.24.0 darwin/arm64");
            let metadata = crate::flows::constants::get_oauth_client_metadata();
            let load = reqwest::Client::new()
                .post(load_url)
                .header("Authorization", format!("Bearer {}", access_token))
                .header("Content-Type", "application/json")
                .header("User-Agent", ua)
                .header("x-request-source", "local")
                .json(&json!({ "metadata": Json::Object(metadata) }))
                .send()
                .await;

            if let Ok(res) = load {
                if res.status().is_success() {
                    let data: Json = res.json().await.unwrap_or(Json::Null);
                    project_id = data
                        .get("cloudaicompanionProject")
                        .and_then(|p| p.get("id").or(Some(p)))
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string();
                    // The first tier marked default wins; JS trims the id.
                    if let Some(tiers) = data.get("allowedTiers").and_then(Json::as_array) {
                        if let Some(tier) = tiers
                            .iter()
                            .find(|t| t.get("isDefault") == Some(&Json::Bool(true)))
                            .and_then(|t| t.get("id"))
                            .and_then(Json::as_str)
                        {
                            tier_id = tier.trim().to_string();
                        }
                    }
                }
            }
        }

        Ok(Some(json!({
            "userInfo": user_info,
            "projectId": project_id,
            "tierId": tier_id,
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
            scope: str_of(tokens, "scope").map(|s| s.to_string()),
            email: user_info.and_then(|u| str_of(u, "email")).map(|s| s.to_string()),
            display_name: None,
            name: None,
            project_id: extra.and_then(|e| str_of(e, "projectId")).map(|s| s.to_string()),
            id_token: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: AntigravityProvider = AntigravityProvider;
