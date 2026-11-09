//! KiloCode OAuth provider - custom device auth flow

use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;


pub struct KiloCodeProvider;

impl OAuthProvider for KiloCodeProvider {
    fn config_id(&self) -> &'static str { "kilocode" }
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
        Ok(None)
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
        Err(nr_core::Error::NotSupported("KiloCode uses device code flow only".into()))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let device_url = str_of(config, "deviceUrl")
            .ok_or_else(|| nr_core::Error::Config("KiloCode: missing deviceUrl".into()))?;

        let response = reqwest::Client::new()
            .post(device_url)
            .header("Content-Type", "application/json")
            .json(&json!({}))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("KiloCode device request failed".into()));
        }

        let data: serde_json::Value = response.json().await?;
        let mut extra = serde_json::Map::new();
        if let Some(obj) = data.as_object() {
            for (k, v) in obj {
                if !matches!(k.as_str(), "device_code" | "user_code" | "verification_uri" | "expires_in" | "interval") {
                    extra.insert(k.clone(), v.clone());
                }
            }
        }

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
            expires_in: data.get("expires_in").and_then(Json::as_i64).unwrap_or(600),
            interval: data.get("interval").and_then(Json::as_i64).unwrap_or(5),
            extra,
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
        let poll_url = str_of(config, "pollUrl")
            .ok_or_else(|| nr_core::Error::Config("KiloCode: missing pollUrl".into()))?;

        let response = reqwest::Client::new()
            .post(poll_url)
            .header("Content-Type", "application/json")
            .json(&json!({"device_code": device_code}))
            .send()
            .await?;

        let data = response.json().await.unwrap_or_else(|_| json!({}));
        Ok(json!({
            "ok": response.status().is_success(),
            "data": data
        }))
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

pub static PROVIDER: KiloCodeProvider = KiloCodeProvider;
