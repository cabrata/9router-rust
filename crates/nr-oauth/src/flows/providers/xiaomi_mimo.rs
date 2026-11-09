//! Xiaomi MiMo OAuth provider — custom ECDH encrypted callback flow.
//! Port of src/lib/oauth/providers/xiaomi-mimo.js
//! Flow type: "authorization_code" (custom encrypted variant)

use nr_core::{Result, Json, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;
use serde_json::json;

pub struct XiaomiMimoProvider;

#[async_trait]
impl OAuthProvider for XiaomiMimoProvider {
    fn config_id(&self) -> &'static str { "xiaomi-mimo" }
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
        // TODO: Generate X25519 keypair and build authorize URL with public key
        let platform_url = config.get("platformUrl").and_then(|v| v.as_str())
            .unwrap_or("https://platform.xiaomimimo.com");

        let params = [
            ("pk", "PLACEHOLDER_PUBLIC_KEY"),
            ("redirect_uri", redirect_uri),
            ("kn", "mimocode"),
        ];

        let qs = params.into_iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        Ok(Some(format!("{}/authorize?{}", platform_url, qs)))
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
        // TODO: Decrypt the encrypted callback (u= param) using X25519 ECDH + AES-256-GCM
        // Returns: { uid, sk, url }

        let mut result = serde_json::Map::new();
        result.insert("_encrypted_payload".to_string(), json!(code));

        Ok(Json::Object(result))
    }

    async fn request_device_code(
        &self, _config: &Json, _code_challenge: Option<&str>, _options: Option<&Json>, _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        Err(nr_core::Error::new("Xiaomi MiMo does not support device code flow"))
    }

    async fn poll_device_token(
        &self, _config: &Json, _device_code: &str, _code_verifier: Option<&str>, _extra_data: Option<&Json>, _log: &Logger,
    ) -> Result<Json> {
        Err(nr_core::Error::new("Xiaomi MiMo does not support device code flow"))
    }

    async fn post_exchange(&self, _tokens: &Json, _meta: Option<&Json>, _log: &Logger) -> Result<Option<Json>> {
        Ok(None)
    }

    fn map_tokens(&self, tokens: &Json, _extra: Option<&Json>) -> Result<MappedTokens> {
        // TODO: After decryption, sk is the API key
        let sk = tokens.get("sk")
            .and_then(|v| v.as_str()).unwrap_or("").to_string();

        if sk.is_empty() {
            return Err(nr_core::Error::new("Xiaomi MiMo: missing sk (secret key)"));
        }

        let uid = tokens.get("uid").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let mut provider_specific_data = serde_json::Map::new();
        provider_specific_data.insert("uid".to_string(), json!(uid));
        if let Some(url) = tokens.get("url") {
            provider_specific_data.insert("url".to_string(), url.clone());
        }

        Ok(MappedTokens {
            access_token: sk,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            email: None,
            display_name: None,
            provider_specific_data: Some(provider_specific_data),
        })
    }
}

pub static PROVIDER: XiaomiMimoProvider = XiaomiMimoProvider;
