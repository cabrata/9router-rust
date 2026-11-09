//! Muse OAuth provider — device code + Model API key mint.
//! Port of src/lib/oauth/providers/muse.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct MuseProvider;

const MUSE_KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const API_VERSION: &str = "1.0.0";

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
impl OAuthProvider for MuseProvider {
    fn config_id(&self) -> &'static str { "muse" }
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
        Err(nr_core::Error::new("Muse: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let device_code_url = str_of(config, "deviceCodeUrl")
            .unwrap_or_else(|| "https://auth.meta.com/oidc/device/authorization/".to_string());
        let client_id = str_of(config, "clientId").unwrap_or_default();

        let response = reqwest::Client::new()
            .post(&device_code_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("x-api-version", API_VERSION)
            .body(form_body(&[("client_id", client_id.as_str())]))
            .send()
            .await?;

        let ok = response.status().is_success();
        let text = response.text().await.unwrap_or_default();
        if !ok {
            return Err(nr_core::Error::new(&format!(
                "Muse Code device code request failed: {text}"
            )));
        }
        let data: Json = serde_json::from_str(&text).map_err(|e| {
            nr_core::Error::new(&format!("Muse Code device code returned invalid JSON: {e}"))
        })?;
        log.debug("OAUTH", "muse device code granted");

        let device_code = str_of(&data, "device_code")
            .ok_or_else(|| nr_core::Error::new("Muse: device code response is missing device_code"))?;
        Ok(DeviceCodeResponse {
            device_code,
            user_code: str_of(&data, "user_code").unwrap_or_default(),
            verification_uri: str_of(&data, "verification_uri")
                .or_else(|| str_of(&data, "verification_url"))
                .unwrap_or_default(),
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
            .unwrap_or_else(|| "https://auth.meta.com/oidc/device/token/".to_string());
        let client_id = str_of(config, "clientId").unwrap_or_default();

        let response = reqwest::Client::new()
            .post(&token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("x-api-version", API_VERSION)
            .body(form_body(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", client_id.as_str()),
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
        let access_token = str_of(tokens, "access_token").unwrap_or_default();
        let client = reqwest::Client::new();

        // Mint the subscription API key; `onboard: true` enrolls on first login.
        // The endpoint is aggressively rate limited (429) and the device code is
        // one-shot, so transient failures are retried here instead of failing the
        // whole login.
        let mut last_status = 0u16;
        let mut last_body = String::new();
        for attempt in 0..3u32 {
            let response = client
                .post(MUSE_KEY_URL)
                .header("Accept", "application/json")
                .header("Authorization", format!("Bearer {access_token}"))
                .header("Content-Type", "application/json")
                .header("x-api-version", API_VERSION)
                .body(json!({ "onboard": true }).to_string())
                .send()
                .await;

            let Ok(response) = response else {
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_secs(5 * (attempt as u64 + 1))).await;
                    continue;
                }
                return Err(nr_core::Error::new("Muse Code key mint failed: transport error"));
            };

            last_status = response.status().as_u16();
            last_body = response.text().await.unwrap_or_default();
            let transient = last_status == 429 || last_status >= 500;
            if !transient || attempt >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5 * (attempt as u64 + 1))).await;
        }

        if !(200..300).contains(&last_status) {
            // Meta's error envelope (e.g. 403 code 4705002) carries the fix-it URL.
            let mut message = format!("{last_status} {}", &last_body.chars().take(200).collect::<String>());
            if let Ok(err) = serde_json::from_str::<Json>(&last_body) {
                let title = str_of(&err, "title");
                let detail = str_of(&err, "detail");
                let parts: Vec<String> = [title, detail].into_iter().flatten().collect();
                if !parts.is_empty() {
                    message = parts.join(": ");
                    if let Some(action_url) = str_of(&err, "action_url") {
                        message.push_str(&format!(" — {action_url}"));
                    }
                }
            }
            log.warn("OAUTH", format!("muse key mint failed: {last_status}"));
            return Err(nr_core::Error::new(&format!("Muse Code key mint failed: {message}")));
        }

        let payload: Json = serde_json::from_str(&last_body)
            .map_err(|_| nr_core::Error::new("Muse Code key mint returned invalid JSON"))?;

        if payload.get("is_subs_active") == Some(&Json::Bool(false)) {
            return Err(nr_core::Error::new(
                "Muse Code subscription is inactive — activate it on muse.ai first",
            ));
        }
        let action_url = str_of(&payload, "action_url")
            .or_else(|| str_of(&payload, "require_payment_action_url"));
        if payload.get("api_key").is_none() {
            if payload.get("require_payment").map(nr_core::json::truthy).unwrap_or(false) || action_url.is_some() {
                return Err(nr_core::Error::new(&format!(
                    "Muse Code subscription required{}",
                    action_url.map(|u| format!(": {u}")).unwrap_or_default()
                )));
            }
            return Err(nr_core::Error::new("Muse Code key response is missing api_key"));
        }

        Ok(Some(json!({ "key": payload })))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        // Muse uses the minted API key as the credential, not the OAuth token
        let api_key = extra
            .and_then(|e| e.get("key"))
            .and_then(|k| k.get("api_key"))
            .and_then(|v| v.as_str())
            .unwrap_or(tokens.get("access_token").and_then(|v| v.as_str()).unwrap_or(""));

        if api_key.is_empty() {
            return Err(nr_core::Error::new("Muse: missing api_key"));
        }

        let email = extra
            .and_then(|e| e.get("key"))
            .and_then(|k| k.get("user_email"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(MappedTokens {
            access_token: api_key.to_string(),
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            scope: None,
            project_id: None,
            id_token: None,
            email,
            display_name: None,
            provider_specific_data: None,
            ..Default::default()
        })
    }
}

pub static PROVIDER: MuseProvider = MuseProvider;
