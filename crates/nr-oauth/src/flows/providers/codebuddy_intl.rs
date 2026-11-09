//! CodeBuddy Intl OAuth provider — browser polling flow with .ai domain.
//! Port of src/lib/oauth/providers/codebuddy-intl.js
//! Flow type: "device_code"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct CodebuddyIntlProvider;

#[async_trait]
impl OAuthProvider for CodebuddyIntlProvider {
    fn config_id(&self) -> &'static str { "codebuddy-intl" }
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
        Err(nr_core::Error::new("CodeBuddy Intl: exchange_token not supported for device_code flow"))
    }

    async fn request_device_code(
        &self,
        config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        let state_url = config.get("stateUrl").and_then(|v| v.as_str()).unwrap_or("");

        Ok(DeviceCodeResponse {
            device_code: "CODEBUDDY_INTL_STATE".to_string(),
            user_code: "CODEBUDDY_INTL".to_string(),
            verification_uri: format!("{}?auth=true", state_url),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 2,
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
        let token_url = config.get("tokenUrl").and_then(|v| v.as_str()).unwrap_or("");

        let mut result = serde_json::Map::new();
        result.insert("code".to_string(), json!(11217i32)); // pending
        result.insert("error".to_string(), json!("authorization_pending"));
        result.insert("_token_url".to_string(), json!(token_url));
        result.insert("_state".to_string(), json!(device_code));

        Ok(Json::Object(result))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens.get("access_token")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("CodeBuddy Intl: missing access_token"));
        }

        let expires_in = tokens.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(86400);

        Ok(MappedTokens {
            access_token,
            refresh_token: tokens.get("refresh_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
            expires_in: Some(expires_in),
            expires_at: None,
            scope: None,
            project_id: None,
            id_token: None,
            email: None,
            display_name: None,
            name: None,
            provider_specific_data: None,
        })
    }
}

pub static PROVIDER: CodebuddyIntlProvider = CodebuddyIntlProvider;
