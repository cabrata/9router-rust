//! Nous Research OAuth provider — device code + agent key mint.
//! Port of src/lib/oauth/providers/nous.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct NousProvider;

fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn str_of(v: &Json, key: &str) -> Option<String> {
    v.get(key).and_then(Json::as_str).map(str::to_string)
}

#[async_trait]
impl OAuthProvider for NousProvider {
    fn config_id(&self) -> &'static str { "nous" }
    fn flow_type(&self) -> &'static str { "device_code" }

    async fn build_auth_url(
        &self, _config: &Json, _redirect_uri: &str, _state: &str, _code_challenge: Option<&str>,
        _meta: Option<&Json>, _log: &Logger,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    async fn exchange_token(
        &self, _config: &Json, _code: &str, _redirect_uri: &str, _code_verifier: Option<&str>,
        _state: Option<&str>, _meta: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Nous: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let device_code_url = str_of(config, "deviceCodeUrl")
            .unwrap_or_else(|| "https://portal.nousresearch.com/api/oauth/device/code".to_string());
        let client_id = str_of(config, "clientId").unwrap_or_default();
        let scope = str_of(config, "scope").unwrap_or_default();

        let response = reqwest::Client::new()
            .post(&device_code_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(form_body(&[("client_id", client_id.as_str()), ("scope", scope.as_str())]))
            .send()
            .await?;

        let ok = response.status().is_success();
        let text = response.text().await.unwrap_or_default();
        if !ok {
            return Err(nr_core::Error::new(&format!(
                "Nous device code request failed: {text}"
            )));
        }
        let data: Json = serde_json::from_str(&text)
            .map_err(|e| nr_core::Error::new(&format!("Nous device code returned invalid JSON: {e}")))?;
        log.debug("OAUTH", "nous device code granted");

        let device_code = str_of(&data, "device_code")
            .ok_or_else(|| nr_core::Error::new("Nous: device code response is missing device_code"))?;
        Ok(DeviceCodeResponse {
            device_code,
            user_code: str_of(&data, "user_code").unwrap_or_default(),
            verification_uri: str_of(&data, "verification_uri").unwrap_or_default(),
            verification_uri_complete: str_of(&data, "verification_uri_complete"),
            expires_in: data.get("expires_in").and_then(Json::as_i64).unwrap_or(600),
            interval: data.get("interval").and_then(Json::as_i64).unwrap_or(5),
            extra: None,
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
            .unwrap_or_else(|| "https://portal.nousresearch.com/api/oauth/token".to_string());
        let client_id = str_of(config, "clientId").unwrap_or_default();

        let response = reqwest::Client::new()
            .post(&token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(form_body(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id.as_str()),
                ("device_code", device_code),
            ]))
            .send()
            .await?;

        let http_ok = response.status().is_success();
        let text = response.text().await.unwrap_or_default();
        let data: Json = serde_json::from_str(&text).unwrap_or_else(|_| {
            json!({ "error": "invalid_response", "error_description": text })
        });
        let pending = matches!(
            data.get("error").and_then(Json::as_str),
            Some("authorization_pending") | Some("slow_down")
        );
        Ok(json!({ "ok": http_ok || pending, "data": data }))
    }

    async fn post_exchange(&self, tokens: &Json, _meta: Option<&Json>, log: &Logger) -> Result<Option<Json>> {
        // Best-effort agent key mint: a failure only means inference falls back to
        // the plain access token, so it must not fail the login.
        let agent_key_url = nr_data::provider_oauth("nous")
            .and_then(|c| c.get("agentKeyUrl"))
            .and_then(Json::as_str)
            .unwrap_or("https://portal.nousresearch.com/api/oauth/agent-key");
        let Some(access_token) = str_of(tokens, "access_token") else {
            return Ok(Some(json!({ "agentKey": Json::Null })));
        };

        let response = reqwest::Client::new()
            .post(agent_key_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access_token}"))
            .body(json!({ "min_ttl_seconds": 7200 }).to_string())
            .send()
            .await;

        let Ok(response) = response else {
            log.warn("OAUTH", "nous agent key mint request failed");
            return Ok(Some(json!({ "agentKey": Json::Null })));
        };
        if !response.status().is_success() {
            log.warn("OAUTH", format!("nous agent key mint failed ({})", response.status().as_u16()));
            return Ok(Some(json!({ "agentKey": Json::Null })));
        }
        let data: Json = response.json().await.unwrap_or(Json::Null);
        Ok(Some(json!({
            "agentKey": data.get("api_key").cloned().unwrap_or(Json::Null),
            "agentKeyId": data.get("key_id").cloned().unwrap_or(Json::Null),
            "agentKeyExpiresAt": data.get("expires_at").cloned().unwrap_or(Json::Null),
            "agentKeyExpiresIn": data.get("expires_in").cloned().unwrap_or(Json::Null),
        })))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Nous: missing access_token"));
        }

        // Agent key is the actual API key used
        let api_key = extra
            .and_then(|e| e.get("agentKey"))
            .and_then(|v| v.as_str())
            .unwrap_or(&access_token);

        // JS mirrors the inference base and agent-key metadata into
        // providerSpecificData so the executor knows where to send requests.
        let mut psd = serde_json::Map::new();
        let inference_base = nr_data::provider_oauth("nous")
            .and_then(|c| c.get("inferenceBaseUrl"))
            .and_then(Json::as_str)
            .unwrap_or("https://inference-api.nousresearch.com/v1")
            .to_string();
        psd.insert("inferenceBaseUrl".into(), Json::String(inference_base));
        for (key, out) in [
            ("agentKeyId", "agentKeyId"),
            ("agentKeyExpiresAt", "agentKeyExpiresAt"),
        ] {
            if let Some(value) = extra.and_then(|e| e.get(key)).filter(|v| !v.is_null()) {
                psd.insert(out.into(), value.clone());
            }
        }

        Ok(MappedTokens {
            access_token: api_key.to_string(),
            refresh_token: tokens.get("refresh_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
            expires_in: tokens.get("expires_in").and_then(|v| v.as_i64()),
            expires_at: None,
            scope: tokens.get("scope").and_then(|v| v.as_str()).map(|s| s.to_string()),
            project_id: None,
            id_token: None,
            email: None,
            display_name: None,
            name: None,
            provider_specific_data: Some(psd),
        })
    }
}

pub static PROVIDER: NousProvider = NousProvider;
