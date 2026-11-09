//! Zed OAuth provider — RSA keypair native-app flow (NOT standard OAuth2).
//!
//! Zed uses a native-app sign-in flow with ephemeral RSA keypairs:
//! 1. prepareConfig() generates a fresh RSA-2048 keypair
//! 2. buildAuthUrl() returns the native_app_signin URL
//! 3. exchangeToken() decrypts the RSA-encrypted access token from the local callback
//!
//! Port of src/lib/oauth/providers/zed.js
//! Flow type: "authorization_code" (non-standard native app variant)

use serde_json::json;
use nr_core::{Result, Json, JsonMap, Logger};
use crate::flows::{OAuthProvider, MappedTokens, DeviceCodeResponse};
use async_trait::async_trait;

/// Zed OAuth provider instance for static dispatch.
pub struct ZedProvider;

#[async_trait]
impl OAuthProvider for ZedProvider {
    fn config_id(&self) -> &'static str {
        "zed"
    }

    fn flow_type(&self) -> &'static str {
        "authorization_code"
    }

    async fn build_auth_url(
        &self,
        config: &Json,
        _redirect_uri: &str,
        _state: &str,
        _code_challenge: Option<&str>,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<String>> {
        // Zed native app flow: return the native_app_signin URL directly
        let web_base_url = config
            .get("webBaseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("https://zed.dev");

        let auth_url = format!("{}/native_app_signin", web_base_url);
        Ok(Some(auth_url))
    }

    async fn exchange_token(
        &self,
        _config: &Json,
        code: &str,
        _redirect_uri: &str,
        _code_verifier: Option<&str>,
        _state: Option<&str>,
        meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // Zed flow: "code" is actually the raw callback payload
        // containing {userId, encryptedAccessToken}
        // TODO: Implement RSA decryption using open-sse/shared/zedAuth.js helpers

        // For now, pass through the raw callback and let postExchange handle it
        // In the real implementation, we'd:
        // 1. Parse the callback payload: {userId, encryptedAccessToken}
        // 2. Decrypt using the private key stored in meta.systemId
        // 3. Return {accessToken, userId, systemId}

        // Placeholder: treat code as a JSON payload
        let parsed: Result<JsonMap> = serde_json::from_str(code)
            .map_err(|e| nr_core::Error::new(&format!("Failed to parse Zed callback: {}", e)));

        match parsed {
            Ok(payload) => Ok(Json::Object(payload)),
            Err(_) => {
                // Fall back to treating code as raw payload
                let mut result = serde_json::Map::new();
                result.insert("_raw_code".to_string(), json!(code));
                if let Some(meta_obj) = meta {
                    if let Some(system_id) = meta_obj.get("systemId") {
                        result.insert("systemId".to_string(), system_id.clone());
                    }
                }
                Ok(Json::Object(result))
            }
        }
    }

    async fn request_device_code(
        &self,
        _config: &Json,
        _code_challenge: Option<&str>,
        _options: Option<&Json>,
        _log: &Logger,
    ) -> Result<DeviceCodeResponse> {
        // Zed doesn't support device code flow
        Err(nr_core::Error::new("Zed does not support device code flow"))
    }

    async fn poll_device_token(
        &self,
        _config: &Json,
        _device_code: &str,
        _code_verifier: Option<&str>,
        _extra_data: Option<&Json>,
        _log: &Logger,
    ) -> Result<Json> {
        // Zed doesn't support device code flow
        Err(nr_core::Error::new("Zed does not support device code flow"))
    }

    async fn post_exchange(
        &self,
        tokens: &Json,
        _meta: Option<&Json>,
        _log: &Logger,
    ) -> Result<Option<Json>> {
        // TODO: Fetch user info from Zed Cloud API using the access token
        // Best-effort lookup of email, name, organizationId
        // Returns: { userInfo: { email, name }, organizationId }

        // For now, extract what we can from tokens
        let user_id = tokens
            .get("userId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let system_id = tokens
            .get("systemId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut result = serde_json::Map::new();

        // Best-effort user info lookup (would call Zed API here)
        let mut user_info = serde_json::Map::new();
        user_info.insert("email".to_string(), Json::Null);
        user_info.insert("name".to_string(), Json::Null);

        result.insert("userInfo".to_string(), Json::Object(user_info));
        result.insert("organizationId".to_string(), json!(""));

        Ok(Some(Json::Object(result)))
    }

    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens> {
        let access_token = tokens
            .get("accessToken")
            .or_else(|| tokens.get("_decrypted_access_token"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if access_token.is_empty() {
            return Err(nr_core::Error::new("Zed: missing accessToken"));
        }

        let user_id = tokens
            .get("userId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let system_id = tokens
            .get("systemId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Extract email and name from extra (post_exchange result)
        let email = extra
            .and_then(|e| e.get("userInfo"))
            .and_then(|u| u.get("email"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let display_name = extra
            .and_then(|e| e.get("userInfo"))
            .and_then(|u| u.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let organization_id = extra
            .and_then(|e| e.get("organizationId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Build provider-specific data
        let mut provider_specific_data = serde_json::Map::new();
        provider_specific_data.insert("authMethod".to_string(), json!("oauth"));
        provider_specific_data.insert("userId".to_string(), json!(user_id));
        provider_specific_data.insert("systemId".to_string(), json!(system_id));
        provider_specific_data.insert("organizationId".to_string(), json!(organization_id));

        Ok(MappedTokens {
            access_token,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            email,
            display_name,
            provider_specific_data: Some(provider_specific_data),
        })
    }
}

/// Static instance for provider registry
pub static PROVIDER: ZedProvider = ZedProvider;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_auth_url() {
        let config = ZedConfig::default();
        let url = build_auth_url(&config);
        assert!(url.contains("native_app_signin"));
        assert!(url.contains("zed.dev"));
    }

    #[test]
    fn test_resolve_organization_id_with_user_info() {
        let mut user_info = serde_json::Map::new();
        user_info.insert(
            "organizationId".to_string(),
            json!("org_12345"),
        );
        let org_id = resolve_organization_id(Some(&user_info));
        assert_eq!(org_id, "org_12345");
    }

    #[test]
    fn test_resolve_organization_id_empty() {
        let org_id = resolve_organization_id(None);
        assert_eq!(org_id, "");
    }

    #[test]
    fn test_map_tokens() {
        let tokens = ZedTokens {
            access_token: "test_token".to_string(),
            user_id: "user_123".to_string(),
            system_id: "system_456".to_string(),
        };

        let mut extra = serde_json::Map::new();
        extra.insert("email".to_string(), json!("user@example.com"));
        extra.insert("name".to_string(), json!("Test User"));
        extra.insert("organizationId".to_string(), json!("org_789"));

        let mapped = map_tokens(&tokens, Some(&extra));

        assert_eq!(mapped.get("accessToken").and_then(|v| v.as_str()), Some("test_token"));
        assert_eq!(mapped.get("email").and_then(|v| v.as_str()), Some("user@example.com"));

        let provider_data = mapped.get("providerSpecificData").and_then(|v| v.as_object()).unwrap();
        assert_eq!(provider_data.get("userId").and_then(|v| v.as_str()), Some("user_123"));
        assert_eq!(provider_data.get("systemId").and_then(|v| v.as_str()), Some("system_456"));
        assert_eq!(provider_data.get("organizationId").and_then(|v| v.as_str()), Some("org_789"));
    }
}
