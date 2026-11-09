//! Z.ai GLM Coding OAuth provider - CLI polling flow with business token exchange

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;


pub struct GlmProvider;

#[async_trait]
impl OAuthProvider for GlmProvider {
    fn config_id(&self) -> &'static str { "glm" }
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
        Err(nr_core::Error::NotSupported("GLM uses device code flow only".into()))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        use rand::Rng;
        let poll_token: String = {
            let mut rng = rand::thread_rng();
            (0..32).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()
        };

        let cli_init_url = str_of(config, "cliInitUrl")
            .ok_or_else(|| nr_core::Error::Config("GLM: missing cliInitUrl".into()))?;

        let response = reqwest::Client::new()
            .post(cli_init_url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", poll_token))
            .json(&json!({"provider": "zai"}))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(nr_core::Error::Provider("GLM init failed".into()));
        }

        let payload: serde_json::Value = response.json().await?;
        let data = payload.get("data")
            .ok_or_else(|| nr_core::Error::BadRequest("GLM: no data in init response".into()))?;

        let flow_id = str_of(data, "flow_id")
            .ok_or_else(|| nr_core::Error::BadRequest("GLM: missing flow_id".into()))?
            .to_string();
        let authorize_url = str_of(data, "authorize_url")
            .ok_or_else(|| nr_core::Error::BadRequest("GLM: missing authorize_url".into()))?
            .to_string();

        let expires_in = data.get("expires_at")
            .and_then(|e| e.as_i64())
            .map(|e| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let ms = if e > 1_000_000_000_000 { e } else { e * 1000 };
                (ms - now).max(0) / 1000
            })
            .unwrap_or(300);

        let interval = data.get("poll_interval_sec")
            .and_then(Json::as_i64)
            .unwrap_or(3);

        let mut extra = serde_json::Map::new();
        extra.insert("_zcodePollToken".to_string(), json!(poll_token));

        Ok(DeviceCodeResponse {
            device_code: flow_id,
            user_code: "zai".to_string(),
            verification_uri: authorize_url,
            verification_uri_complete: None,
            expires_in,
            interval,
            extra: Some(extra),
        })
    }

    async fn poll_device_token(
        &self,
        config: &Json,
        device_code: &str,
        _code_verifier: Option<&str>,
        extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let poll_token = extra_data
            .and_then(|e| e.get("_zcodePollToken"))
            .and_then(|t| t.as_str());

        if poll_token.is_none() {
            return Ok(json!({
                "ok": true,
                "data": {
                    "error": "access_denied",
                    "error_description": "Missing ZCode poll token"
                }
            }));
        }

        let cli_poll_url = str_of(config, "cliPollUrl")
            .ok_or_else(|| nr_core::Error::Config("GLM: missing cliPollUrl".into()))?;

        let response = reqwest::Client::new()
            .get(format!("{}/{}", cli_poll_url, urlencoding::encode(device_code)))
            .header("Authorization", format!("Bearer {}", poll_token.unwrap()))
            .send()
            .await?;

        if !response.status().is_success() {
            return Ok(json!({
                "ok": true,
                "data": {
                    "error": "access_denied",
                    "error_description": format!("GLM poll failed (HTTP {})", response.status())
                }
            }));
        }

        let payload: serde_json::Value = response.json().await.unwrap_or_else(|_| json!({}));
        let data = payload.get("data").cloned().unwrap_or_else(|| json!({}));
        let status = str_of(&data, "status");

        if status == Some("pending") {
            return Ok(json!({
                "ok": true,
                "data": {"error": "authorization_pending"}
            }));
        }

        if status == Some("ready") {
            // For now, extract access token directly. Full implementation would
            // handle business token exchange for API keys.
            // TODO(dedupe): implement resolveCodingPlanApiKey once full flow is needed
            let zai_token = str_of(&data, "accessToken").or_else(|| str_of(&data, "access_token"));
            if let Some(token) = zai_token {
                return Ok(json!({
                    "ok": true,
                    "data": {
                        "access_token": token,
                        "_zcodeJwtToken": str_of(&data, "token").unwrap_or(""),
                        "_zaiRefreshToken": str_of(&data, "refresh_token").or_else(|| str_of(&data, "refreshToken")).unwrap_or("")
                    }
                }));
            }
        }

        Ok(json!({
            "ok": true,
            "data": {
                "error": status.unwrap_or("unknown"),
                "error_description": "GLM poll returned unexpected status"
            }
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
            refresh_token: str_of(tokens, "_zaiRefreshToken").map(|s| s.to_string()),
            expires_in: None,
            expires_at: None,
            email: None,
            display_name: None,
            name: None,
            project_id: None,
            id_token: None,
            scope: None,
            provider_specific_data: Some(json!({
                "authMethod": "cli_poll",
                "zcodeJwtToken": str_of(tokens, "_zcodeJwtToken"),
            }).as_object().cloned().unwrap_or_default()),
        })
    }
}

pub static PROVIDER: GlmProvider = GlmProvider;
