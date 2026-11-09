//! Cursor OAuth provider — token imported from local SQLite database.
//! Port of src/lib/oauth/providers/cursor.js
//! Flow type: "import_token"

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct CursorProvider;

#[async_trait]
impl OAuthProvider for CursorProvider {
    fn config_id(&self) -> &'static str {
        "cursor"
    }

    fn flow_type(&self) -> &'static str {
        "import_token"
    }

    async fn build_auth_url(
        &self,
        _config: &Json,
        _redirect_uri: &str,
        _state: &str,
        _code_challenge: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        // Cursor uses import_token flow; no auth URL needed
        Ok(None)
    }

    async fn exchange_token(
        &self,
        _config: &Json,
        code: &str,
        _redirect_uri: &str,
        _code_verifier: Option<&str>,
        _state: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // Code is the raw token extracted from local SQLite database
        // The executor/CLI handles the extraction; we just pass it through
        let mut result = serde_json::Map::new();
        result.insert("accessToken".to_string(), json!(code));
        result.insert("expiresIn".to_string(), json!(86400i64));
        Ok(Json::Object(result))
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::new("Cursor does not support device code flow"))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Cursor does not support device code flow"))
    }

    async fn post_exchange(
        &self,
        _tokens: &Json,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<Json>> {
        // No post-exchange processing for imported tokens
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens
            .get("accessToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Cursor: missing accessToken"));
        }

        let expires_in = tokens
            .get("expiresIn")
            .and_then(|v| v.as_i64())
            .unwrap_or(86400);

        let mut provider_specific_data = serde_json::Map::new();
        provider_specific_data.insert("authMethod".to_string(), json!("imported"));

        // Extract machine ID if present
        if let Some(machine_id) = tokens.get("machineId").and_then(|v| v.as_str()) {
            provider_specific_data.insert("machineId".to_string(), json!(machine_id));
        }

        Ok(MappedTokens {
            access_token,
            refresh_token: None,
            expires_in: Some(expires_in),
            expires_at: None,
            email: None,
            display_name: None,
            provider_specific_data: Some(provider_specific_data),
        })
    }
}

pub static PROVIDER: CursorProvider = CursorProvider;
