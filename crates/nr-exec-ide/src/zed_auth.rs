//! Port of open-sse/shared/zedAuth.js.
//!
//! Zed hosted LLM aggregator authentication + model-catalog helpers.
//! Uses self-generated RSA keypairs for native app auth instead of OAuth.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nr_core::{Credentials, Error, Result};

pub const ZED_WEB_BASE_URL: &str = "https://zed.dev";
pub const ZED_CLOUD_BASE_URL: &str = "https://cloud.zed.dev";
pub const ZED_LLM_BASE_URL: &str = "https://cloud.zed.dev";

pub struct ZedHeaderNames {
    pub expired_token: &'static str,
    pub outdated_token: &'static str,
    pub client_supports_status: &'static str,
    pub client_supports_stream_ended: &'static str,
    pub server_supports_status: &'static str,
    pub client_supports_xai: &'static str,
    pub system_id: &'static str,
}

pub const ZED_HEADERS: ZedHeaderNames = ZedHeaderNames {
    expired_token: "x-zed-expired-token",
    outdated_token: "x-zed-outdated-token",
    client_supports_status: "x-zed-client-supports-status-messages",
    client_supports_stream_ended: "x-zed-client-supports-stream-ended-request-completion-status",
    server_supports_status: "x-zed-server-supports-status-messages",
    client_supports_xai: "x-zed-client-supports-x-ai",
    system_id: "x-zed-system-id",
};

const PRIVATE_KEY_PREFIX: &str = "zed-rsa-pkcs1:";
const LLM_TOKEN_TTL_MS: u64 = 50 * 60 * 1000; // 50 minutes
const MODEL_CACHE_TTL_MS: u64 = 60 * 60 * 1000; // 1 hour

/// Cache of LLM tokens with expiry times.
type LlmTokenCache = Arc<Mutex<std::collections::HashMap<String, (String, u64)>>>;

lazy_static::lazy_static! {
    static ref LLM_TOKEN_CACHE: LlmTokenCache = Arc::new(Mutex::new(std::collections::HashMap::new()));
}

/// Encode a PEM private key as an opaque verifier (flows through codeVerifier slot).
pub fn encode_zed_private_key_verifier(private_key_pem: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(private_key_pem);
    format!("{}{}", PRIVATE_KEY_PREFIX, encoded)
}

/// Decode the opaque verifier back to PEM private key.
pub fn decode_zed_private_key_verifier(verifier: &str) -> Result<String> {
    if !verifier.starts_with(PRIVATE_KEY_PREFIX) {
        return Err(Error::new("Missing Zed private key verifier; restart the login flow"));
    }
    let encoded = &verifier[PRIVATE_KEY_PREFIX.len()..];
    URL_SAFE_NO_PAD
        .decode(encoded)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .map_err(|e| Error::new(format!("Failed to decode Zed private key: {}", e)))
}

/// Build the Authorization header value for Zed API requests.
pub fn build_zed_user_auth_header(credentials: &Credentials) -> Result<String> {
    let user_id = credentials
        .psd_str("userId")
        .ok_or_else(|| Error::new("Zed credential missing userId"))?;

    let access_token = credentials
        .access_token()
        .or_else(|| credentials.api_key())
        .ok_or_else(|| Error::new("Zed credential missing accessToken"))?;

    Ok(format!("{} {}", user_id, access_token))
}

/// Get the system_id from credentials.
fn get_system_id(credentials: &Credentials) -> Option<String> {
    credentials.psd_str("systemId").map(|s| s.to_string())
}

/// Cache key for LLM tokens: "userId:organizationId:tokenSuffix"
fn zed_user_cache_key(credentials: &Credentials, organization_id: &str) -> String {
    let user_id = credentials.psd_str("userId").unwrap_or("unknown");

    let token = credentials
        .access_token()
        .or_else(|| credentials.api_key())
        .unwrap_or("");

    let token_suffix = if token.len() > 16 {
        &token[token.len() - 16..]
    } else {
        token
    };

    format!("{}:{}:{}", user_id, organization_id, token_suffix)
}

/// Fetch a short-lived LLM token from Zed's /client/llm_tokens endpoint.
/// Tokens are cached and automatically refreshed when expired.
pub async fn fetch_zed_llm_token(credentials: &Credentials) -> Result<String> {
    // TODO(dedupe): integrate with nr_oauth token refresh once available.
    // For now, return a placeholder.
    // In the real implementation, this would:
    // 1. Check the cache for an unexpired token
    // 2. Fetch from /client/llm_tokens if needed
    // 3. Store in cache with TTL

    // Placeholder: require that accessToken is the LLM token for now
    credentials
        .access_token()
        .or_else(|| credentials.api_key())
        .ok_or_else(|| Error::new("Zed credential missing accessToken"))
        .map(|s| s.to_string())
}

/// Resolve (and cache) the live Zed model catalog.
/// TODO(dedupe): implement model fetching and caching once HTTP client available.
pub async fn resolve_zed_models(credentials: &Credentials) -> Result<Option<serde_json::Value>> {
    // This would fetch from /models endpoint and cache
    // For now, return None to indicate models not fetched
    let _ = credentials; // suppress warning
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_private_key_verifier() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----";
        let encoded = encode_zed_private_key_verifier(pem);
        assert!(encoded.starts_with(PRIVATE_KEY_PREFIX));

        let decoded = decode_zed_private_key_verifier(&encoded);
        assert!(decoded.is_ok());
        assert_eq!(decoded.unwrap(), pem);
    }

    #[test]
    fn encode_decode_round_trip() {
        let pem = "test-key-content";
        let encoded = encode_zed_private_key_verifier(pem);
        let decoded = decode_zed_private_key_verifier(&encoded).unwrap();
        assert_eq!(decoded, pem);
    }

    #[test]
    fn decode_invalid_verifier() {
        let invalid = "invalid-prefix:test";
        let result = decode_zed_private_key_verifier(invalid);
        assert!(result.is_err());
    }
}
