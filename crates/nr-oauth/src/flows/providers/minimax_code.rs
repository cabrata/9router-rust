//! MiniMax Code OAuth provider - device_code flow (CN and global variants)

use async_trait::async_trait;
use super::*;
use nr_core::{Result, Logger, Json, json::str_of};
use serde_json::json;


struct MinimaxCodeVariant {
    account_url: &'static str,
}

impl MinimaxCodeVariant {
    const DEVICE_GRANT: &'static str = "urn:ietf:params:oauth:grant-type:device_code";

    async fn request_device_code_impl(
        &self,
        code_challenge: &str,
    ) -> Result<DeviceCodeResponse> {
        let device_url = format!("{}/oauth2/device/code", self.account_url);

        let params = serde_urlencoded::to_string([
            ("client_id", "mcode-public"),
            ("scope", "agent.default"),
            ("audience", "agent-backend"),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
        ]).map_err(|e| nr_core::Error::Other(e.to_string()))?;

        let response = reqwest::Client::new()
            .post(&device_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(params)
            .send()
            .await?;

        let body: serde_json::Value = response.json().await.unwrap_or_else(|_| json!({}));

        let user_code = str_of(&body, "user_code")
            .ok_or_else(|| nr_core::Error::BadRequest("MiniMax: missing user_code".into()))?
            .to_string();
        let uri = str_of(&body, "verification_uri_complete")
            .or_else(|| str_of(&body, "verification_uri"))
            .ok_or_else(|| nr_core::Error::BadRequest("MiniMax: missing verification_uri".into()))?
            .to_string();
        let device_code = str_of(&body, "device_code").map(|s| s.to_string());

        let poll_by_user = device_code.is_none();
        let expires_in = body.get("expires_in")
            .and_then(Json::as_i64)
            .or_else(|| body.get("expired_in").and_then(Json::as_i64))
            .map(|e| if e > 1_000_000_000_000 {
                ((e - std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64) / 1000).max(1)
            } else {
                e
            })
            .unwrap_or(600);

        let interval = body.get("interval")
            .and_then(Json::as_i64)
            .map(|i| if poll_by_user && i >= 60 {
                (i / 1000).max(1)
            } else {
                i
            })
            .unwrap_or(5);

        let mut extra = serde_json::Map::new();
        extra.insert("_minimaxPollByUser".to_string(), json!(poll_by_user));

        Ok(DeviceCodeResponse {
            device_code: device_code.unwrap_or_else(|| user_code.clone()),
            user_code,
            verification_uri: uri,
            verification_uri_complete: None,
            expires_in,
            interval,
            extra: Some(extra),
        })
    }

    async fn poll_device_token_impl(
        &self,
        device_code: &str,
        code_verifier: Option<&str>,
        poll_by_user: bool,
    ) -> Result<Json> {
        let token_url = format!("{}/oauth2/token", self.account_url);

        let mut fields: Vec<(&str, &str)> = vec![
            ("grant_type", Self::DEVICE_GRANT),
            ("client_id", "mcode-public"),
        ];

        if poll_by_user {
            fields.push(("user_code", device_code));
        } else {
            fields.push(("device_code", device_code));
        }

        let code_verifier_str;
        if let Some(verifier) = code_verifier {
            code_verifier_str = verifier.to_string();
            fields.push(("code_verifier", &code_verifier_str));
        }

        let params = serde_urlencoded::to_string(&fields).map_err(|e| nr_core::Error::Other(e.to_string()))?;

        let response = reqwest::Client::new()
            .post(&token_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(params)
            .send()
            .await?;

        let body: serde_json::Value = response.json().await.unwrap_or_else(|_| json!({}));

        if let Some(status) = str_of(&body, "status") {
            match status {
                "pending" => return Ok(json!({"ok": true, "data": {"error": "authorization_pending"}})),
                "slow_down" => return Ok(json!({"ok": true, "data": {"error": "slow_down"}})),
                "denied" | "access_denied" => return Ok(json!({"ok": true, "data": {"error": "access_denied"}})),
                "expired" | "expired_token" => return Ok(json!({"ok": true, "data": {"error": "expired_token"}})),
                _ => return Ok(json!({"ok": true, "data": {"error": format!("unexpected_status:{}", status)}})),
            }
        }

        if let Some(error) = str_of(&body, "error") {
            return Ok(json!({
                "ok": true,
                "data": {
                    "error": error,
                    "error_description": str_of(&body, "error_description").unwrap_or("")
                }
            }));
        }

        if str_of(&body, "access_token").is_none() {
            return Ok(json!({"ok": true, "data": {"error": "no_access_token"}}));
        }

        Ok(json!({"ok": true, "data": body}))
    }
}

pub struct MinimaxCnProvider;

#[async_trait]
impl OAuthProvider for MinimaxCnProvider {
    fn config_id(&self) -> &'static str { "minimax-code" }
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
        Err(nr_core::Error::NotSupported("MiniMax uses device code flow".into()))
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let challenge = code_challenge
            .ok_or_else(|| nr_core::Error::BadRequest("MiniMax: code_challenge required".into()))?;
        let variant = MinimaxCodeVariant { account_url: "https://account.minimax.cn" };
        variant.request_device_code_impl(challenge).await
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        device_code: &str,
        code_verifier: Option<&str>,
        extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let poll_by_user = extra_data
            .and_then(|e| e.get("_minimaxPollByUser"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let variant = MinimaxCodeVariant { account_url: "https://account.minimax.cn" };
        variant.poll_device_token_impl(device_code, code_verifier, poll_by_user).await
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

pub struct MinimaxGlobalProvider;

#[async_trait]
impl OAuthProvider for MinimaxGlobalProvider {
    fn config_id(&self) -> &'static str { "minimax-code-global" }
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
        Err(nr_core::Error::NotSupported("MiniMax uses device code flow".into()))
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let challenge = code_challenge
            .ok_or_else(|| nr_core::Error::BadRequest("MiniMax: code_challenge required".into()))?;
        let variant = MinimaxCodeVariant { account_url: "https://account.minimax.io" };
        variant.request_device_code_impl(challenge).await
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        device_code: &str,
        code_verifier: Option<&str>,
        extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        let poll_by_user = extra_data
            .and_then(|e| e.get("_minimaxPollByUser"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let variant = MinimaxCodeVariant { account_url: "https://account.minimax.io" };
        variant.poll_device_token_impl(device_code, code_verifier, poll_by_user).await
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

pub static PROVIDER_CN: MinimaxCnProvider = MinimaxCnProvider;
pub static PROVIDER_GLOBAL: MinimaxGlobalProvider = MinimaxGlobalProvider;
