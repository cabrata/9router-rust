//! Kimchi OAuth provider - browser token callback flow

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;


pub struct KimchiProvider;

#[async_trait]
impl OAuthProvider for KimchiProvider {
    fn config_id(&self) -> &'static str { "kimchi" }
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
        Err(nr_core::Error::NotSupported("Kimchi uses device code flow only".into()))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let auth_url = str_of(config, "authUrl")
            .ok_or_else(|| nr_core::Error::Config("Kimchi: missing authUrl".into()))?;

        Ok(DeviceCodeResponse {
            device_code: uuid::Uuid::new_v4().to_string(),
            user_code: "kimchi".to_string(),
            verification_uri: auth_url.to_string(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
            extra: Some(serde_json::Map::new()),
        })
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // Browser callback delivers token, polling is not typical
        Ok(json!({
            "ok": false,
            "data": {"error": "authorization_pending"}
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
            expires_at: None,
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

pub static PROVIDER: KimchiProvider = KimchiProvider;
