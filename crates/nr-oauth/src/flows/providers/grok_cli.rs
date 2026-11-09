//! Grok CLI OAuth provider — device code flow.
//! Port of src/lib/oauth/providers/grok-cli.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct GrokCliProvider;

fn grok_cli_const(name: &str) -> String {
    nr_data::export("open-sse/config/grokCli", name)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn pager_user_agent() -> String {
    let ua = grok_cli_const("GROK_CLI_PAGER_USER_AGENT");
    if ua.is_empty() {
        "grok-pager/1.0.44 grok-shell/1.0.44 (linux; x86_64)".into()
    } else {
        ua
    }
}

/// Percent-encode a form body the way `URLSearchParams` does.
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
impl OAuthProvider for GrokCliProvider {
    fn config_id(&self) -> &'static str { "grok-cli" }
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
        Err(nr_core::Error::new("Grok CLI: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let device_code_url = str_of(config, "deviceCodeUrl")
            .unwrap_or_else(|| "https://auth.x.ai/oauth2/device/code".to_string());
        let client_id = str_of(config, "clientId").unwrap_or_default();
        let scope = str_of(config, "scope").unwrap_or_default();
        let referrer = str_of(config, "referrer").unwrap_or_default();

        // The official CLI sends `referrer=grok-build` alongside client_id/scope.
        let mut pairs = vec![("client_id", client_id.as_str()), ("scope", scope.as_str())];
        if !referrer.is_empty() {
            pairs.push(("referrer", referrer.as_str()));
        }

        let response = reqwest::Client::new()
            .post(&device_code_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", pager_user_agent())
            .body(form_body(&pairs))
            .send()
            .await?;

        let ok = response.status().is_success();
        let text = response.text().await.unwrap_or_default();
        if !ok {
            return Err(nr_core::Error::new(&format!(
                "Grok CLI device code request failed: {text}"
            )));
        }
        let data: Json = serde_json::from_str(&text).map_err(|e| {
            nr_core::Error::new(&format!("Grok CLI device code returned invalid JSON: {e}"))
        })?;
        log.debug("OAUTH", "grok-cli device code granted");

        let device_code = str_of(&data, "device_code")
            .ok_or_else(|| nr_core::Error::new("Grok CLI: device code response is missing device_code"))?;
        let user_code = str_of(&data, "user_code").unwrap_or_default();
        let verification_uri = str_of(&data, "verification_uri")
            .or_else(|| str_of(&data, "verification_url"))
            .unwrap_or_default();

        Ok(DeviceCodeResponse {
            device_code,
            user_code,
            verification_uri,
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
        let token_url =
            str_of(config, "tokenUrl").unwrap_or_else(|| "https://auth.x.ai/oauth2/token".to_string());
        let client_id = str_of(config, "clientId").unwrap_or_default();

        let body = form_body(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", client_id.as_str()),
        ]);
        let response = reqwest::Client::new()
            .post(&token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", pager_user_agent())
            .body(body)
            .send()
            .await?;

        let http_ok = response.status().is_success();
        let text = response.text().await.unwrap_or_default();
        let data: Json = serde_json::from_str(&text).unwrap_or_else(|_| {
            json!({ "error": "invalid_response", "error_description": text })
        });

        // 400 + authorization_pending / slow_down is the expected wait state, not
        // an error; the caller keeps polling.
        let pending = matches!(
            data.get("error").and_then(Json::as_str),
            Some("authorization_pending") | Some("slow_down")
        );

        Ok(json!({ "ok": http_ok || pending, "data": data }))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        // Best-effort profile lookup from cli-chat-proxy; a failure is not fatal.
        let Some(access_token) = _tokens.get("access_token").and_then(Json::as_str) else {
            return Ok(Some(json!({ "user": Json::Null })));
        };
        let user = reqwest::Client::new()
            .get("https://cli-chat-proxy.grok.com/v1/user")
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json")
            .header("User-Agent", pager_user_agent())
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-grok-client-version", grok_cli_const("GROK_CLI_VERSION"))
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success());
        let user = match user {
            Some(response) => response.json::<Json>().await.unwrap_or(Json::Null),
            None => Json::Null,
        };
        Ok(Some(json!({ "user": user })))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Grok CLI: missing access_token"));
        }

        let id_token = str_of(tokens, "id_token");
        let user = extra.and_then(|e| e.get("user")).filter(|u| u.is_object());
        let user_email = extra
            .and_then(|e| e.get("user"))
            .and_then(|u| u.get("email"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| super::decode_xai_id_token_email(id_token.as_deref()))
            .or_else(|| super::extract_email_from_access_token(&access_token));
        let display_name = user
            .map(|u| {
                ["firstName", "lastName"]
                    .iter()
                    .filter_map(|k| u.get(*k).and_then(Json::as_str))
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|name| !name.trim().is_empty());
        let expires_in = tokens.get("expires_in").and_then(Json::as_i64);

        // The executor reads identity from providerSpecificData, so mirror the
        // fields the JS store writes there (id_token included).
        let mut psd = serde_json::Map::new();
        psd.insert("authMethod".into(), json!("device_code"));
        psd.insert("idToken".into(), id_token.clone().map(Json::String).unwrap_or(Json::Null));
        psd.insert("email".into(), user_email.clone().map(Json::String).unwrap_or(Json::Null));
        for (key, out) in [
            ("userId", "userId"),
            ("principalId", "userId"),
            ("hasGrokCodeAccess", "hasGrokCodeAccess"),
            ("subscriptionTier", "subscriptionTier"),
        ] {
            if let Some(value) = user.and_then(|u| u.get(key)).filter(|v| !v.is_null()) {
                psd.insert(out.into(), value.clone());
            }
        }

        Ok(MappedTokens {
            access_token,
            refresh_token: tokens.get("refresh_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
            // The caller derives the absolute `expiresAt` string from `expires_in`
            // (JS does the same in mapTokens), so no epoch sneaks into the row.
            expires_in,
            expires_at: None,
            scope: str_of(tokens, "scope"),
            project_id: None,
            id_token,
            email: user_email,
            display_name,
            name: None,
            provider_specific_data: Some(psd),
        })
    }
}

pub static PROVIDER: GrokCliProvider = GrokCliProvider;
