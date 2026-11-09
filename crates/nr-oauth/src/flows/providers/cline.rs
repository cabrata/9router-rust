//! Cline OAuth provider - authorization_code flow with base64-encoded token

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;

pub struct ClineProvider;

#[async_trait]
impl OAuthProvider for ClineProvider {
    fn config_id(&self) -> &'static str { "cline" }
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
        let authorize_url = str_of(config, "authorizeUrl")
            .ok_or_else(|| nr_core::Error::Config("Cline: missing authorizeUrl".into()))?;

        let params = serde_urlencoded::to_string([
            ("client_type", "extension"),
            ("callback_url", redirect_uri),
            ("redirect_uri", redirect_uri),
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
        // Try to decode code as base64 first (direct token delivery from Cline)
        if let Ok(decoded) = decode_base64_token(code) {
            if let Ok(tokens) = serde_json::from_str::<serde_json::Value>(&decoded) {
                return Ok(tokens);
            }
        }

        // Fall back to token exchange endpoint
        let token_url = str_of(config, "tokenExchangeUrl")
            .ok_or_else(|| nr_core::Error::Config("Cline: missing tokenExchangeUrl".into()))?;

        let body = json!({
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
            return Err(nr_core::Error::Provider("Cline token exchange failed".into()));
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
        Err(nr_core::Error::NotSupported("Cline does not support device code flow".into()))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::NotSupported("Cline does not support device code flow".into()))
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
        let expires_at = str_of(tokens, "expires_at");
        let expires_in = if let Some(expires_at_str) = expires_at {
            if let Ok(ms) = expires_at_str.parse::<i64>() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                Some((ms - now) / 1000)
            } else {
                Some(3600)
            }
        } else {
            Some(3600)
        };

        Ok(MappedTokens {
            access_token: str_of(tokens, "access_token")
                .ok_or_else(|| nr_core::Error::BadRequest("Missing access_token".into()))?
                .to_string(),
            refresh_token: str_of(tokens, "refresh_token").map(|s| s.to_string()),
            expires_in,
            expires_at: None,
            email: str_of(tokens, "email").map(|s| s.to_string()),
            display_name: None,
            name: None,
            project_id: None,
            id_token: None,
            scope: None,
            provider_specific_data: None,
        })
    }
}

fn decode_base64_token(code: &str) -> Result<String> {
    use base64::Engine;
    let mut padded = code.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .map_err(|e| nr_core::Error::Other(e.to_string()))?;
    Ok(String::from_utf8(bytes).map_err(|e| nr_core::Error::Other(e.to_string()))?)
}

pub static PROVIDER: ClineProvider = ClineProvider;
