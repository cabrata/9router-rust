//! Port of open-sse/shared/zedAuth.js.
//!
//! Zed hosted LLM aggregator authentication + model-catalog helpers.
//! Uses self-generated RSA keypairs for native app auth instead of OAuth.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pkcs8::DecodePrivateKey;
use rsa::RsaPrivateKey;
use serde_json::{json, Value as JsonValue};

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

type LlmTokenCacheEntry = (String, u64); // (token, expires_at)
type ModelCacheEntry = (JsonValue, u64); // (models_data, expires_at)

lazy_static::lazy_static! {
    static ref LLM_TOKEN_CACHE: Mutex<HashMap<String, LlmTokenCacheEntry>> = Mutex::new(HashMap::new());
    static ref MODEL_CACHE: Mutex<HashMap<String, ModelCacheEntry>> = Mutex::new(HashMap::new());
}

/// Encode a PEM private key as an opaque verifier (flows through codeVerifier slot).
pub fn encode_zed_private_key_verifier(private_key_pem: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(private_key_pem);
    format!("{}{}", PRIVATE_KEY_PREFIX, encoded)
}

/// Decode the opaque verifier back to PEM private key.
pub fn decode_zed_private_key_verifier(verifier: &str) -> Result<String> {
    if !verifier.starts_with(PRIVATE_KEY_PREFIX) {
        return Err(Error::other(
            "Missing Zed private key verifier; restart the login flow",
        ));
    }
    let encoded = &verifier[PRIVATE_KEY_PREFIX.len()..];
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| Error::other(format!("Failed to decode Zed private key: {}", e)))?;
    String::from_utf8(bytes).map_err(|e| Error::other(format!("Invalid UTF-8 in private key: {}", e)))
}

/// Decrypt RSA-encrypted access token using the stored private key.
pub fn decrypt_zed_access_token(
    encrypted_access_token: &str,
    private_key_verifier: &str,
) -> Result<String> {
    let private_key_pem = decode_zed_private_key_verifier(private_key_verifier)?;

    // Try OAEP with SHA-256 first (standard)
    let encrypted = URL_SAFE_NO_PAD
        .decode(encrypted_access_token)
        .map_err(|e| Error::other(format!("Failed to decode encrypted token: {}", e)))?;

    let private_key = RsaPrivateKey::from_pkcs8_pem(&private_key_pem)
        .map_err(|e| Error::other(format!("Failed to parse RSA private key: {}", e)))?;

    // Try OAEP with SHA-256 first
    match private_key.decrypt(rsa::Oaep::new::<sha2::Sha256>(), &encrypted) {
        Ok(decrypted) => {
            let token = String::from_utf8(decrypted)
                .map_err(|e| Error::other(format!("Invalid UTF-8 in decrypted token: {}", e)))?;
            Ok(token)
        }
        Err(oaep_err) => {
            // Fallback to PKCS#1 v1.5 for older clients
            match private_key.decrypt(rsa::Pkcs1v15Encrypt, &encrypted) {
                Ok(decrypted) => {
                    let token = String::from_utf8(decrypted)
                        .map_err(|e| {
                            Error::other(format!("Invalid UTF-8 in decrypted token: {}", e))
                        })?;
                    // PKCS#1 v1.5 unpadding is not integrity-checked; wrong-key decrypt
                    // can "succeed" with garbage. Replacement chars prove wrong decryption.
                    if token.contains('\u{FFFD}') {
                        return Err(Error::other(format!(
                            "Failed to decrypt Zed access token: {}",
                            oaep_err
                        )));
                    }
                    Ok(token)
                }
                Err(_pkcs1_err) => Err(Error::other(format!(
                    "Failed to decrypt Zed access token (tried OAEP and PKCS#1): {}",
                    oaep_err
                ))),
            }
        }
    }
}

/// Build the Authorization header value for Zed API requests.
pub fn build_zed_user_auth_header(credentials: &Credentials) -> Result<String> {
    let user_id = credentials
        .psd_str("userId")
        .ok_or_else(|| Error::other("Zed credential missing userId"))?;

    let access_token = credentials
        .access_token()
        .or_else(|| credentials.api_key())
        .ok_or_else(|| Error::other("Zed credential missing accessToken"))?;

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

/// Cache key for model catalog
fn zed_model_cache_key(credentials: &Credentials) -> String {
    let org = credentials
        .psd_str("organizationId")
        .or_else(|| credentials.psd_str("defaultOrganizationId"))
        .unwrap_or("default");

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

    format!("{}:{}:{}", user_id, org, token_suffix)
}

/// Normalize organization ID from various formats
fn normalize_organization_id(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Array(arr) if !arr.is_empty() => {
            if let JsonValue::String(s) = &arr[0] {
                Some(s.clone())
            } else {
                None
            }
        }
        JsonValue::Object(obj) => obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// Resolve Zed organization ID from credentials and user info
pub fn resolve_zed_organization_id(
    credentials: &Credentials,
    user_info: Option<&JsonValue>,
) -> Option<String> {
    // Check explicit org ID in credentials
    if let Some(org_id) = credentials
        .psd_str("organizationId")
        .or_else(|| credentials.psd_str("defaultOrganizationId"))
        .map(|s| s.to_string())
    {
        return Some(org_id);
    }

    if let Some(info) = user_info {
        if let Some(org_id) = info
            .get("default_organization_id")
            .or_else(|| info.get("defaultOrganizationId"))
            .and_then(normalize_organization_id)
        {
            return Some(org_id);
        }

        if let Some(orgs) = info.get("organizations").and_then(|v| v.as_array()) {
            // Find personal org first
            if let Some(org) = orgs
                .iter()
                .find(|o| o.get("is_personal").map_or(false, |v| v.as_bool().unwrap_or(false)))
            {
                if let Some(org_id) = normalize_organization_id(&org["id"]) {
                    return Some(org_id);
                }
            }
            // Fall back to first org
            if let Some(org) = orgs.first() {
                if let Some(org_id) = normalize_organization_id(&org["id"]) {
                    return Some(org_id);
                }
            }
        }
    }

    None
}

/// Fetch authenticated user info from Zed
pub async fn fetch_zed_authenticated_user(
    credentials: &Credentials,
    base_url: &str,
    _proxy_options: Option<&nr_core::http::ProxyOptions>,
) -> Result<JsonValue> {
    let client = reqwest::Client::new();
    let auth_header = build_zed_user_auth_header(credentials)?;
    let url = format!("{}/client/users/me", base_url.trim_end_matches('/'));

    let mut req = client.get(&url).header("Authorization", auth_header);

    if let Some(system_id) = get_system_id(credentials) {
        req = req.header(ZED_HEADERS.system_id, system_id);
    }

    let response = req
        .send()
        .await
        .map_err(|e| Error::other(format!("Failed to fetch Zed user info: {}", e)))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| Error::other(format!("Failed to read Zed user info response: {}", e)))?;

    if !status.is_success() {
        return Err(Error::other(format!("Zed API returned {}: {}", status, text)));
    }

    serde_json::from_str(&text)
        .map_err(|e| Error::other(format!("Failed to parse Zed user info: {}", e)))
}

/// Fetch LLM token from Zed
pub async fn fetch_zed_llm_token(
    credentials: &Credentials,
    base_url: &str,
    organization_id: Option<&str>,
    proxy_options: Option<&nr_core::http::ProxyOptions>,
) -> Result<String> {
    let mut org_id = organization_id.map(|s| s.to_string());

    // Resolve org ID if not provided
    if org_id.is_none() {
        let user_info = fetch_zed_authenticated_user(credentials, base_url, proxy_options).await?;
        org_id = resolve_zed_organization_id(credentials, Some(&user_info));
    }

    let org_id = org_id.ok_or_else(|| Error::other("No Zed organization selected"))?;

    let cache_key = zed_user_cache_key(credentials, &org_id);

    // Check cache
    if let Ok(cache) = LLM_TOKEN_CACHE.lock() {
        if let Some((token, expires_at)) = cache.get(&cache_key) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if now < *expires_at {
                return Ok(token.clone());
            }
        }
    }

    // Fetch new token
    let client = reqwest::Client::new();
    let auth_header = build_zed_user_auth_header(credentials)?;
    let url = format!("{}/client/llm_tokens", base_url.trim_end_matches('/'));

    let mut req = client
        .post(&url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json");

    if let Some(system_id) = get_system_id(credentials) {
        req = req.header(ZED_HEADERS.system_id, system_id);
    }

    let response = req
        .json(&json!({"organization_id": org_id}))
        .send()
        .await
        .map_err(|e| Error::other(format!("Failed to fetch Zed LLM token: {}", e)))?;

    let text = response
        .text()
        .await
        .map_err(|e| Error::other(format!("Failed to read Zed LLM token response: {}", e)))?;

    let data: JsonValue = serde_json::from_str(&text)
        .map_err(|e| Error::other(format!("Failed to parse Zed LLM token response: {}", e)))?;

    let token = data
        .get("token")
        .and_then(|v| {
            if let JsonValue::String(s) = v {
                Some(s.clone())
            } else if let JsonValue::Array(arr) = v {
                arr.first()
                    .and_then(|item| item.as_str())
                    .map(|s| s.to_string())
            } else {
                v.get("value").and_then(|v| v.as_str()).map(|s| s.to_string())
            }
        })
        .ok_or_else(|| Error::other("Zed did not return an LLM token"))?;

    // Cache the token
    if let Ok(mut cache) = LLM_TOKEN_CACHE.lock() {
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_millis() as u64) + LLM_TOKEN_TTL_MS)
            .unwrap_or(0);
        cache.insert(cache_key, (token.clone(), expires_at));
    }

    Ok(token)
}

/// Check if Zed LLM token should be refreshed
pub fn should_refresh_zed_llm_token(response: &reqwest::Response) -> bool {
    response.status() == 401
        || response.headers().contains_key(ZED_HEADERS.expired_token)
        || response.headers().contains_key(ZED_HEADERS.outdated_token)
}

/// Fetch LLM token with automatic refresh if needed
pub async fn zed_llm_fetch(
    credentials: &Credentials,
    path: &str,
    base_url: &str,
    proxy_options: Option<&nr_core::http::ProxyOptions>,
) -> Result<reqwest::Response> {
    let token = fetch_zed_llm_token(credentials, base_url, None, proxy_options).await?;

    let client = reqwest::Client::new();
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let req = client.get(&url).header("Authorization", format!("Bearer {}", token));

    // TODO(dedupe): apply fetch_options headers and method
    let response = req
        .send()
        .await
        .map_err(|e| Error::other(format!("Zed LLM fetch failed: {}", e)))?;

    // Auto-refresh on token expiry
    if should_refresh_zed_llm_token(&response) {
        let token = fetch_zed_llm_token(credentials, base_url, None, proxy_options).await?;
        let req = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token));
        return req
            .send()
            .await
            .map_err(|e| Error::other(format!("Zed LLM fetch (retry) failed: {}", e)));
    }

    Ok(response)
}

/// Normalize model ID from various formats
fn normalize_zed_model_id(id: &JsonValue) -> Option<String> {
    match id {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Array(arr) if !arr.is_empty() => {
            if let JsonValue::String(s) = &arr[0] {
                Some(s.clone())
            } else {
                None
            }
        }
        JsonValue::Object(obj) => obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// Map Zed model to normalized format
pub fn map_zed_model(model: &JsonValue) -> Option<JsonValue> {
    let id = normalize_zed_model_id(&model["id"])?;
    Some(json!({
        "id": id,
        "name": model.get("display_name")
            .or_else(|| model.get("displayName"))
            .and_then(|v| v.as_str())
            .unwrap_or(&id),
        "provider": model.get("provider").and_then(|v| v.as_str()),
        "isLatest": model.get("is_latest").and_then(|v| v.as_bool()).unwrap_or(false),
        "contextLength": model.get("max_token_count")
            .or_else(|| model.get("maxTokenCount"))
            .and_then(|v| v.as_i64()),
        "contextLengthInMaxMode": model.get("max_token_count_in_max_mode")
            .or_else(|| model.get("maxTokenCountInMaxMode"))
            .and_then(|v| v.as_i64()),
        "maxOutputTokens": model.get("max_output_tokens")
            .or_else(|| model.get("maxOutputTokens"))
            .and_then(|v| v.as_i64()),
        "supportsTools": model.get("supports_tools").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportsImages": model.get("supports_images").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportsThinking": model.get("supports_thinking").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportsDisablingThinking": model.get("supports_disabling_thinking").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportsFastMode": model.get("supports_fast_mode").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportsServerSideCompaction": model.get("supports_server_side_compaction").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportedEffortLevels": model.get("supported_effort_levels")
            .or_else(|| model.get("supportedEffortLevels"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        "supportsStreamingTools": model.get("supports_streaming_tools").and_then(|v| v.as_bool()).unwrap_or(false),
        "supportsParallelToolCalls": model.get("supports_parallel_tool_calls").and_then(|v| v.as_bool()).unwrap_or(false),
        "isDisabled": model.get("is_disabled").and_then(|v| v.as_bool()).unwrap_or(false),
        "disabledReason": model.get("disabled_reason")
            .and_then(|v| v.as_str())
            .map(|s| JsonValue::String(s.to_string()))
    }))
}

/// Resolve (and cache) the live Zed model catalog
pub async fn resolve_zed_models(
    credentials: &Credentials,
    base_url: &str,
    force_refresh: bool,
    proxy_options: Option<&nr_core::http::ProxyOptions>,
) -> Result<Option<JsonValue>> {
    if credentials.access_token().is_none() {
        return Ok(None);
    }

    let cache_key = zed_model_cache_key(credentials);

    // Check cache
    if !force_refresh {
        if let Ok(cache) = MODEL_CACHE.lock() {
            if let Some((models_data, expires_at)) = cache.get(&cache_key) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now < *expires_at {
                    return Ok(Some(models_data.clone()));
                }
            }
        }
    }

    // Fetch models
    let response = zed_llm_fetch(credentials, "/models", base_url, proxy_options).await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "unknown error".to_string());
        return Err(Error::other(format!(
            "Zed models fetch failed: {} {}",
            status,
            text
        )));
    }

    let data: JsonValue = response
        .json()
        .await
        .map_err(|e| Error::other(format!("Failed to parse Zed models response: {}", e)))?;

    let raw_models = data
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let models: Vec<JsonValue> = raw_models
        .iter()
        .filter_map(map_zed_model)
        .filter(|m| !m.get("isDisabled").and_then(|v| v.as_bool()).unwrap_or(false))
        .collect();

    let result = json!({
        "models": models,
        "rawModels": raw_models,
        "defaultModel": data.get("default_model")
            .or_else(|| data.get("defaultModel"))
            .and_then(|v| normalize_zed_model_id(v)),
        "defaultFastModel": data.get("default_fast_model")
            .or_else(|| data.get("defaultFastModel"))
            .and_then(|v| normalize_zed_model_id(v)),
        "recommendedModels": data.get("recommended_models")
            .or_else(|| data.get("recommendedModels"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(normalize_zed_model_id).collect::<Vec<_>>())
            .unwrap_or_default(),
    });

    // Cache the result
    if let Ok(mut cache) = MODEL_CACHE.lock() {
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_millis() as u64) + MODEL_CACHE_TTL_MS)
            .unwrap_or(0);
        cache.insert(cache_key, (result.clone(), expires_at));
    }

    Ok(Some(result))
}

/// Clear all caches (for testing)
#[cfg(test)]
pub fn clear_zed_caches() {
    let _ = LLM_TOKEN_CACHE.lock().map(|mut c| c.clear());
    let _ = MODEL_CACHE.lock().map(|mut c| c.clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_private_key_verifier() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----";
        let encoded = encode_zed_private_key_verifier(pem);
        assert!(encoded.starts_with(PRIVATE_KEY_PREFIX));

        let decoded = decode_zed_private_key_verifier(&encoded).unwrap();
        assert_eq!(decoded, pem);
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

    #[test]
    fn normalize_organization_id_string() {
        let val = JsonValue::String("org-123".to_string());
        assert_eq!(normalize_organization_id(&val), Some("org-123".to_string()));
    }

    #[test]
    fn normalize_organization_id_array() {
        let val = json!(["org-123"]);
        assert_eq!(normalize_organization_id(&val), Some("org-123".to_string()));
    }

    #[test]
    fn normalize_organization_id_object() {
        let val = json!({"id": "org-123"});
        assert_eq!(normalize_organization_id(&val), Some("org-123".to_string()));
    }

    #[test]
    fn normalize_zed_model_id_string() {
        let val = JsonValue::String("gpt-4".to_string());
        assert_eq!(normalize_zed_model_id(&val), Some("gpt-4".to_string()));
    }
}
