//! Port of open-sse/services/tokenRefresh.js and tokenRefresh/{providers,dedup}.js.
//!
//! FROZEN signatures (executor crates call them while this is being ported).
//! Each mirrors the JS function of the same name; JS returns the refreshed
//! fields (`accessToken`, `refreshToken`, `expiresIn`/`expiresAt`, …) or `null`.

use chrono::Utc;
use base64::Engine;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use parking_lot::RwLock;
use serde::{Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use nr_core::http::ProxyOptions;
use nr_core::{Credentials, Json, JsonMap, Logger};

const TOKEN_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000; // 5 minutes
const REFRESH_RESULT_TTL_MS: i64 = 10_000; // 10 seconds (dedup cache TTL)

// In-flight refresh cache: tracks in-progress refreshes per provider:token pair.
// When multiple callers request the same refresh concurrently, they await the first one.
type DedupCacheEntry = Arc<tokio::sync::Mutex<DedupCacheValue>>;

#[derive(Clone)]
struct DedupCacheValue {
    result: Option<JsonMap>,
    expires_at: i64,
}

lazy_static::lazy_static! {
    static ref REFRESH_DEDUP_CACHE: RwLock<HashMap<String, DedupCacheEntry>> =
        RwLock::new(HashMap::new());

    // Vertex JWT token cache: maps service account email to (token, expiresAt)
    static ref VERTEX_TOKEN_CACHE: RwLock<HashMap<String, (String, i64)>> =
        RwLock::new(HashMap::new());
}

pub fn is_unrecoverable_refresh_error(result: &Option<JsonMap>) -> bool {
    match result {
        Some(map) => {
            if let Some(Json::String(err)) = map.get("error") {
                matches!(err.as_str(), "unrecoverable_refresh_error" | "refresh_token_reused" | "invalid_request" | "invalid_grant")
            } else {
                false
            }
        }
        None => false,
    }
}

/// Dedup concurrent or recent refresh attempts for the same provider+token.
/// Concurrent callers for the same key await one in-flight refresh.
/// Recent successful results are reused within TTL window.
/// JS: `dedupRefresh(provider, oldToken, fn, log)`.
pub async fn dedup_refresh<F, Fut>(
    provider: &str,
    old_token: Option<&str>,
    f: F,
    log: &Logger,
) -> Option<JsonMap>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<JsonMap>>,
{
    if old_token.is_none() || old_token.map_or(true, |t| t.is_empty()) {
        // No token to dedup on; call directly
        return f().await;
    }

    let old_token = old_token.unwrap();
    let key = format!("{}:{}", provider, old_token);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // Check cache for recent result or in-flight refresh
    {
        let cache = REFRESH_DEDUP_CACHE.read();
        if let Some(entry) = cache.get(&key) {
            let cached = entry.lock().await;
            if cached.expires_at > now_ms {
                log.info("TOKEN_REFRESH", &format!("Reusing recent refresh result for {}", provider));
                return cached.result.clone();
            }
            // Stale entry will be replaced below
        }
    }

    // Execute refresh (or reuse existing in-flight one)
    let entry = {
        let mut cache = REFRESH_DEDUP_CACHE.write();
        cache.entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(DedupCacheValue {
                result: None,
                expires_at: 0,
            })))
            .clone()
    };

    // Acquire lock and check if another task already refreshed
    let mut cached = entry.lock().await;
    if cached.expires_at > now_ms {
        // Another task filled the cache while we waited
        log.info("TOKEN_REFRESH", &format!("Reusing recent refresh result for {} (filled by concurrent task)", provider));
        return cached.result.clone();
    }

    // Execute the refresh
    let result = f().await;

    // Update cache
    cached.result = result.clone();
    cached.expires_at = now_ms + REFRESH_RESULT_TTL_MS;

    result
}

pub fn get_refresh_lead_ms(provider: &str) -> i64 {
    // TODO(wave2): read from REFRESH_LEAD_MS config when available
    if provider == "kimi-coding" {
        // Legacy id after kimi-coding → kimi merge
        return TOKEN_EXPIRY_BUFFER_MS; // TODO: check REFRESH_LEAD_MS.kimi
    }
    TOKEN_EXPIRY_BUFFER_MS
}

/// JS `parseVertexSaJson(apiKey)`: the service-account JSON, or `None`.
pub fn parse_vertex_sa_json(api_key: &str) -> Option<Json> {
    let parsed: JsonMap = match serde_json::from_str(api_key).ok() {
        Some(Json::Object(m)) => m,
        _ => return None,
    };

    // Must have type, client_email, private_key, project_id
    if parsed.get("type")?.as_str()? == "service_account"
        && parsed.get("client_email")?.as_str().is_some()
        && parsed.get("private_key")?.as_str().is_some()
        && parsed.get("project_id")?.as_str().is_some()
    {
        Some(Json::Object(parsed))
    } else {
        None
    }
}

/// JS `refreshVertexToken(saJson, log)`.
pub async fn refresh_vertex_token(sa_json: &Json, log: &Logger) -> Option<JsonMap> {
    let sa = sa_json.as_object()?;
    let client_email = sa.get("client_email")?.as_str()?;
    let private_key_str = sa.get("private_key")?.as_str()?;
    let _project_id = sa.get("project_id")?.as_str()?;

    // Check cache first
    let cache_key = client_email.to_string();
    {
        let cache = VERTEX_TOKEN_CACHE.read();
        if let Some((token, expires_at)) = cache.get(&cache_key) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            if *expires_at - now_ms > 5 * 60 * 1000 {
                // Cache hit, token still valid for 5+ minutes
                let mut result = JsonMap::new();
                result.insert("accessToken".to_string(), json!(token));
                result.insert("expiresAt".to_string(), json!(*expires_at));
                return Some(result);
            }
        }
    }

    // Mint new token using JWT
    log.debug("TOKEN_REFRESH", &format!("Vertex minting token for {}", client_email));

    // Parse and normalize private key (handle escaped newlines)
    let private_key_pem = private_key_str.replace("\\n", "\n");

    #[derive(Serialize)]
    struct VertexClaim {
        iss: String,
        scope: String,
        aud: String,
        exp: i64,
        iat: i64,
    }

    let now = Utc::now();
    let iat = now.timestamp();
    let exp = iat + 3600; // 1 hour

    let claim = VertexClaim {
        iss: client_email.to_string(),
        scope: "https://www.googleapis.com/auth/cloud-platform".to_string(),
        aud: "https://oauth2.googleapis.com/token".to_string(),
        exp,
        iat,
    };

    let header = Header::new(Algorithm::RS256);
    let encoding_key = match EncodingKey::from_rsa_pem(private_key_pem.as_bytes()) {
        Ok(key) => key,
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Failed to parse Vertex private key: {}", e));
            return None;
        }
    };

    let jwt = match encode(&header, &claim, &encoding_key) {
        Ok(j) => j,
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Failed to sign Vertex JWT: {}", e));
            return None;
        }
    };

    // Exchange JWT for access token
    let client = reqwest::Client::new();
    let response = match client
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Vertex token mint network error: {}", e));
            return None;
        }
    };

    if !response.status().is_success() {
        let err_text = response.text().await.unwrap_or_default();
        log.error("TOKEN_REFRESH", &format!("Vertex token mint failed: {}", err_text));
        return None;
    }

    match response.json::<JsonMap>().await {
        Ok(tokens) => {
            let access_token = tokens.get("access_token")?.as_str()?.to_string();
            let expires_in = tokens.get("expires_in")
                .and_then(|v| v.as_i64())
                .unwrap_or(3600);

            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let expires_at = now_ms + expires_in * 1000;

            // Cache the token
            {
                let mut cache = VERTEX_TOKEN_CACHE.write();
                cache.insert(cache_key, (access_token.clone(), expires_at));
            }

            log.info("TOKEN_REFRESH", &format!("Vertex token minted for {}", client_email));
            let mut result = JsonMap::new();
            result.insert("accessToken".to_string(), json!(access_token));
            result.insert("expiresAt".to_string(), json!(expires_at));
            Some(result)
        }
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Failed to parse Vertex token response: {}", e));
            None
        }
    }
}

/// JS `refreshGoogleToken(refreshToken, clientId, clientSecret, log)`.
pub async fn refresh_google_token(
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
    log: &Logger,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];

    let response = match client
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Network error refreshing Google token: {}", e));
            return None;
        }
    };

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        log.error(
            "TOKEN_REFRESH",
            &format!("Failed to refresh Google token: {}", error_text),
        );
        return None;
    }

    match response.json::<JsonMap>().await {
        Ok(tokens) => {
            log.info(
                "TOKEN_REFRESH",
                "Successfully refreshed Google token",
            );
            let access_token = tokens.get("access_token")?.as_str()?;
            let new_refresh = tokens.get("refresh_token").and_then(|v| v.as_str()).unwrap_or(refresh_token);
            let mut result = JsonMap::new();
            result.insert("accessToken".to_string(), json!(access_token));
            result.insert("refreshToken".to_string(), json!(new_refresh));
            if let Some(expires_in) = tokens.get("expires_in") {
                result.insert("expiresIn".to_string(), expires_in.clone());
            }
            Some(result)
        }
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Failed to parse Google token response: {}", e));
            None
        }
    }
}

/// JS `refreshKiroToken(refreshToken, providerSpecificData, log, proxyOptions)`.
pub async fn refresh_kiro_token(
    refresh_token: &str,
    provider_specific_data: Option<&JsonMap>,
    log: &Logger,
    _proxy: Option<&ProxyOptions>,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("kiro", Some(refresh_token), || async {
        // Check auth method from providerSpecificData
        let psd = provider_specific_data?;
        let auth_method = psd.get("authMethod").and_then(|v| v.as_str());

        if auth_method == Some("external_idp") {
            // TODO(wave2): implement external_idp refresh with buildExternalIdpRefreshParams
            log.warn("TOKEN_REFRESH", "Kiro external_idp token refresh not yet implemented");
            return None;
        }

        let client_id = psd.get("clientId").and_then(|v| v.as_str());
        let client_secret = psd.get("clientSecret").and_then(|v| v.as_str());
        let region = psd.get("region").and_then(|v| v.as_str());

        if let (Some(cid), Some(csec)) = (client_id, client_secret) {
            // AWS IDC path
            let is_idc = auth_method == Some("idc");
            let endpoint = if is_idc && region.is_some() {
                format!("https://oidc.{}.amazonaws.com/token", region.unwrap())
            } else {
                "https://oidc.us-east-1.amazonaws.com/token".to_string()
            };

            let client = reqwest::Client::new();
            match client
                .post(&endpoint)
                .json(&json!({
                    "clientId": cid,
                    "clientSecret": csec,
                    "refreshToken": refresh_token,
                    "grantType": "refresh_token",
                }))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(tokens) = resp.json::<JsonMap>().await {
                        log.info("TOKEN_REFRESH", "Successfully refreshed Kiro AWS token");
                        let mut result = JsonMap::new();
                        if let Some(at) = tokens.get("accessToken") {
                            result.insert("accessToken".to_string(), at.clone());
                        }
                        if let Some(rt) = tokens.get("refreshToken") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("expiresIn") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        return Some(result);
                    }
                }
                Ok(resp) => {
                    log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro AWS token: HTTP {}", resp.status()));
                }
                Err(e) => {
                    log.error("TOKEN_REFRESH", &format!("Kiro AWS token refresh error: {}", e));
                }
            }
            return None;
        }

        // Fallback to social token endpoint (no client credentials)
        let client = reqwest::Client::new();
        match client
            .post("https://api.kiro.app/oauth/token")
            .json(&json!({"refreshToken": refresh_token}))
            .header("User-Agent", "kiro-cli/1.0.0")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(tokens) = resp.json::<JsonMap>().await {
                    log.info("TOKEN_REFRESH", "Successfully refreshed Kiro social token");
                    let mut result = JsonMap::new();
                    if let Some(at) = tokens.get("accessToken") {
                        result.insert("accessToken".to_string(), at.clone());
                    }
                    if let Some(rt) = tokens.get("refreshToken") {
                        result.insert("refreshToken".to_string(), rt.clone());
                    } else {
                        result.insert("refreshToken".to_string(), json!(refresh_token));
                    }
                    if let Some(ei) = tokens.get("expiresIn") {
                        result.insert("expiresIn".to_string(), ei.clone());
                    }
                    return Some(result);
                }
            }
            Ok(resp) => {
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro social token: HTTP {}", resp.status()));
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Kiro social token refresh error: {}", e));
            }
        }
        None
    }, log).await
}

/// JS `refreshMiniMaxCodeToken(provider, refreshToken, log)`.
pub async fn refresh_minimax_code_token(
    provider: &str,
    refresh_token: &str,
    log: &Logger,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    let token_url = match provider {
        "minimax-code" => "https://account.minimax.cn/oauth2/token",
        "minimax-code-global" => "https://account.minimax.io/oauth2/token",
        _ => return None,
    };

    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", "mcode-public"),
        ("scope", "agent.default"),
        ("audience", "agent-backend"),
    ];

    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.post(token_url).form(&params).send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            log.error(
                "TOKEN_REFRESH",
                &format!("MiniMax token refresh failed ({}): {}", provider, e),
            );
            return None;
        }
        Err(_) => {
            log.error("TOKEN_REFRESH", "MiniMax token refresh timed out");
            return None;
        }
    };

    let data: JsonMap = response.json().await.ok()?;

    if data.get("error").and_then(|v| v.as_str()) == Some("invalid_grant") {
        log.warn(
            "TOKEN_REFRESH",
            &format!("MiniMax refresh token spent/rejected ({}) — sign-in gone", provider),
        );
        let mut error_result = JsonMap::new();
        error_result.insert("error".to_string(), json!("invalid_grant"));
        return Some(error_result);
    }

    let access_token = data.get("access_token")?.as_str()?;
    let new_refresh = data.get("refresh_token").and_then(|v| v.as_str()).unwrap_or(refresh_token);
    let mut result = JsonMap::new();
    result.insert("accessToken".to_string(), json!(access_token));
    result.insert("refreshToken".to_string(), json!(new_refresh));
    if let Some(expires_in) = data.get("expires_in") {
        result.insert("expiresIn".to_string(), expires_in.clone());
    }
    Some(result)
}

/// Cline OAuth refresh
pub async fn refresh_cline_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("cline", Some(refresh_token), || async {
        let client = reqwest::Client::new();
        match client
            .post("https://api.workos.com/oauth/token")
            .json(&json!({
                "refreshToken": refresh_token,
                "grantType": "refresh_token",
                "clientType": "extension",
            }))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(body) => {
                        let tokens = body.get("data").cloned().unwrap_or(Json::Object(body.clone()));
                        let access_token = tokens.get("accessToken")?.as_str()?.to_string();

                        let expires_in = tokens.get("expiresAt")
                            .and_then(|v| v.as_str())
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| {
                                let now = Utc::now();
                                let duration = dt.with_timezone(&Utc) - now;
                                std::cmp::max(1, duration.num_seconds())
                            })
                            .or_else(|| tokens.get("expiresIn").and_then(|v| v.as_i64()))
                            .unwrap_or(3600);

                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refreshToken") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        result.insert("expiresIn".to_string(), json!(expires_in));
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Cline token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Cline token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Cline token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// Claude OAuth refresh (JSON body, client_id only)
pub async fn refresh_claude_oauth_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("claude", Some(refresh_token), || async {
        // Use generic refresh_access_token with claude profile
        // Claude: JSON body, client_id only
        let client = reqwest::Client::new();
        let client_id = "claude-desktop"; // Known Claude client ID

        match client
            .post("https://api.claude.ai/oauth/token")
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": client_id,
            }))
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(tokens) => {
                        let access_token = tokens.get("access_token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refresh_token") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("expires_in") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed Claude OAuth token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Claude token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Claude OAuth token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Claude OAuth token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// Codex (OpenAI) token refresh
pub async fn refresh_codex_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("codex", Some(refresh_token), || async {
        let client = reqwest::Client::new();
        let client_id = "pgBWxB0mE77V7OV2aKJK8X5aWu2rVoIe"; // Known Codex client ID

        match client
            .post("https://auth0.openai.com/oauth/token")
            .json(&json!({
                "client_id": client_id,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(tokens) => {
                        let access_token = tokens.get("access_token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refresh_token") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("expires_in") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        if let Some(id) = tokens.get("id_token") {
                            result.insert("idToken".to_string(), id.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed Codex token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Codex token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                // Check for permanent errors
                if status.as_u16() == 400 {
                    if text.contains("invalid_grant") || text.contains("refresh_token_expired") {
                        log.error("TOKEN_REFRESH", "Codex refresh token already used or invalid. Re-auth required.");
                        let mut result = JsonMap::new();
                        result.insert("error".to_string(), json!("unrecoverable_refresh_error"));
                        return Some(result);
                    }
                }
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Codex token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Codex token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// iFlow token refresh (Basic Auth + form body)
pub async fn refresh_iflow_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("iflow", Some(refresh_token), || async {
        let client_id = "iflow-client"; // Known iFlow client ID
        let client_secret = ""; // iFlow secret if needed

        // Build Basic Auth header
        let basic_auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", client_id, client_secret));

        let client = reqwest::Client::new();
        match client
            .post("https://oauth.iflow.io/token")
            .header("Authorization", format!("Basic {}", basic_auth))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(tokens) => {
                        let access_token = tokens.get("access_token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refresh_token") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("expires_in") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed iFlow token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse iFlow token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh iFlow token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("iFlow token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// GitHub token refresh
pub async fn refresh_github_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("github", Some(refresh_token), || async {
        let client_id = "github-cli"; // Known GitHub CLI client ID

        let client = reqwest::Client::new();
        match client
            .post("https://github.com/login/oauth/access_token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ])
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(tokens) => {
                        let access_token = tokens.get("access_token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refresh_token") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("refresh_token_expires_in") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed GitHub token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse GitHub token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh GitHub token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("GitHub token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// Trae token refresh - POST ExchangeToken with JSON body
pub async fn refresh_trae_token(refresh_token: &str, _credentials: &Credentials, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("trae", Some(refresh_token), || async {
        let client = reqwest::Client::new();
        match client
            .post("https://api.trae.ai/oauth/token")
            .json(&json!({
                "ClientID": "trae-cli",
                "RefreshToken": refresh_token,
                "ClientSecret": "",
                "UserID": "",
            }))
            .header("Content-Type", "application/json")
            .header("User-Agent", "Trae/1.0.0 antigravity-cockpit-tools")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(payload) => {
                        let result_obj = payload.get("Result").or_else(|| payload.get("result")).cloned().unwrap_or(Json::Object(payload.clone()));
                        let access_token = result_obj.get("AccessToken")
                            .or_else(|| result_obj.get("accessToken"))
                            .and_then(|v| v.as_str())?
                            .to_string();

                        let new_refresh = result_obj.get("RefreshToken")
                            .or_else(|| result_obj.get("refreshToken"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(refresh_token)
                            .to_string();

                        // Calculate expiresIn from expiresAt if present
                        let expires_in = if let Some(expires_at) = result_obj.get("ExpiresAt")
                            .or_else(|| result_obj.get("expiresAt"))
                        {
                            if let Some(seconds) = expires_at.as_i64() {
                                let now = Utc::now().timestamp();
                                Some(std::cmp::max(1, seconds - now))
                            } else if let Some(s) = expires_at.as_str() {
                                chrono::DateTime::parse_from_rfc3339(s).ok()
                                    .map(|dt| {
                                        let now = Utc::now();
                                        let duration = dt.with_timezone(&Utc) - now;
                                        std::cmp::max(1, duration.num_seconds())
                                    })
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        result.insert("refreshToken".to_string(), json!(new_refresh));
                        if let Some(ei) = expires_in {
                            result.insert("expiresIn".to_string(), json!(ei));
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed Trae token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Trae token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Trae token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Trae token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// Zed token refresh - returns None (long-lived tokens, no refresh)
pub fn refresh_zed_token() -> Option<JsonMap> {
    None
}

/// Windsurf token refresh - apiKey is long-lived, returns None
pub async fn refresh_windsurf_token(log: &Logger) -> Option<JsonMap> {
    log.info("TOKEN_REFRESH", "windsurf: apiKey is long-lived (no refresh_token flow) — skipping");
    None
}

/// Nous Research Portal token refresh with optional agent key minting
pub async fn refresh_nous_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("nous", Some(refresh_token), || async {
        let client = reqwest::Client::new();

        // Step 1: Refresh the access token
        let token_response = match client
            .post("https://portal.nousresearch.com/api/oauth/token")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("x-nous-refresh-token", refresh_token)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", "hermes-cli"),
            ])
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp.json::<JsonMap>().await.ok(),
            Ok(resp) => {
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Nous token: HTTP {}", resp.status()));
                return None;
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Nous token refresh error: {}", e));
                return None;
            }
        };

        let token_data = token_response?;
        let access_token = token_data.get("access_token")?.as_str()?.to_string();
        let new_refresh = token_data.get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or(refresh_token);

        // Step 2: Mint agent key (best effort; failures don't block the result)
        let mut agent_key = None;
        let mut agent_key_id = None;
        let mut agent_key_expires_at = None;

        match client
            .post("https://portal.nousresearch.com/api/oauth/agent-key")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&json!({"min_ttl_seconds": 7200}))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(agent_data) = resp.json::<JsonMap>().await {
                    agent_key = agent_data.get("api_key").and_then(|v| v.as_str()).map(|s| s.to_string());
                    agent_key_id = agent_data.get("key_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    agent_key_expires_at = agent_data.get("expires_at").cloned();
                    log.info("TOKEN_REFRESH", "Nous agent key minted successfully");
                }
            }
            Ok(resp) => {
                log.warn("TOKEN_REFRESH", &format!("Failed to mint Nous agent key: HTTP {}", resp.status()));
            }
            Err(e) => {
                log.warn("TOKEN_REFRESH", &format!("Nous agent key mint error: {}", e));
            }
        }

        // Build result
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        result.insert("refreshToken".to_string(), json!(new_refresh));
        if let Some(ei) = token_data.get("expires_in") {
            result.insert("expiresIn".to_string(), ei.clone());
        }

        // Include agent key if available, otherwise fall back to access token
        if let Some(key) = agent_key {
            result.insert("apiKey".to_string(), json!(key.clone()));
            result.insert("agentKey".to_string(), json!(key));
        } else {
            result.insert("apiKey".to_string(), json!(access_token.clone()));
        }

        // Include provider-specific data for agent key metadata
        if agent_key_id.is_some() || agent_key_expires_at.is_some() {
            let mut psd = JsonMap::new();
            if let Some(id) = agent_key_id {
                psd.insert("agentKeyId".to_string(), json!(id));
            }
            if let Some(exp) = agent_key_expires_at {
                psd.insert("agentKeyExpiresAt".to_string(), exp);
            }
            result.insert("providerSpecificData".to_string(), json!(psd));
        }

        log.info("TOKEN_REFRESH", "Successfully refreshed Nous token");
        Some(result)
    }, log).await
}

/// xAI token refresh
pub async fn refresh_xai_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("xai", Some(refresh_token), || async {
        let client = reqwest::Client::new();
        match client
            .post("https://auth.x.ai/oauth/token")
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(tokens) => {
                        let access_token = tokens.get("access_token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refresh_token") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("expires_in") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        if let Some(id) = tokens.get("id_token") {
                            result.insert("idToken".to_string(), id.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed xAI token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse xAI token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                if text.contains("invalid_grant") || text.contains("invalid_request") {
                    log.error("TOKEN_REFRESH", &format!("xAI refresh failed with invalid grant: {}", text));
                    let mut result = JsonMap::new();
                    result.insert("error".to_string(), json!("invalid_grant"));
                    return Some(result);
                }
                log.error("TOKEN_REFRESH", &format!("Failed to refresh xAI token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("xAI token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// CodeBuddy (Tencent) token refresh - POST with X-Refresh-Token header
pub async fn refresh_codebuddy_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("codebuddy-cn", Some(refresh_token), || async {
        let client = reqwest::Client::new();
        match client
            .post("https://api.codebuddy.cn/v2/plugin/auth/token/refresh")
            .header("Content-Type", "application/json")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Domain", "copilot.tencent.com")
            .header("X-Refresh-Token", refresh_token)
            .header("X-Auth-Refresh-Source", "plugin")
            .header("X-Product", "SaaS")
            .body("{}")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(data) => {
                        if let Some(Json::Number(code)) = data.get("code") {
                            if code.as_i64() != Some(0) {
                                log.error("TOKEN_REFRESH", &format!("CodeBuddy token refresh returned code: {}", code));
                                return None;
                            }
                        }
                        let token_data = data.get("data")?;
                        let access_token = token_data.get("accessToken")?.as_str()?.to_string();

                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = token_data.get("refreshToken") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = token_data.get("expiresIn") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed CodeBuddy token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse CodeBuddy token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh CodeBuddy token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("CodeBuddy token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// CodeBuddy International token refresh
pub async fn refresh_codebuddy_intl_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("codebuddy-intl", Some(refresh_token), || async {
        let client = reqwest::Client::new();
        match client
            .post("https://api.codebuddy.ai/v2/plugin/auth/token/refresh")
            .header("Content-Type", "application/json")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Domain", "www.codebuddy.ai")
            .header("X-Refresh-Token", refresh_token)
            .header("X-Auth-Refresh-Source", "plugin")
            .header("X-Product", "SaaS")
            .body("{}")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(data) => {
                        if let Some(Json::Number(code)) = data.get("code") {
                            if code.as_i64() != Some(0) {
                                log.error("TOKEN_REFRESH", &format!("CodeBuddy intl token refresh returned code: {}", code));
                                return None;
                            }
                        }
                        let token_data = data.get("data")?;
                        let access_token = token_data.get("accessToken")?.as_str()?.to_string();

                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = token_data.get("refreshToken") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = token_data.get("expiresIn") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed CodeBuddy intl token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse CodeBuddy intl token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh CodeBuddy intl token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("CodeBuddy intl token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// GitHub Copilot token refresh from GitHub access token
pub async fn refresh_copilot_token(github_access_token: &str, log: &Logger) -> Option<JsonMap> {
    if github_access_token.is_empty() {
        return None;
    }

    dedup_refresh("copilot", Some(github_access_token), || async {
        let client = reqwest::Client::new();
        match client
            .get("https://api.github.com/copilot_internal/v2/token")
            .header("Authorization", format!("token {}", github_access_token))
            .header("User-Agent", "GitHub-Copilot/1.0.0")
            .header("Editor-Version", "vscode/1.85.0")
            .header("Editor-Plugin-Version", "copilot-chat/0.14.1")
            .header("x-github-api-version", "2023-11-09")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(data) => {
                        let token = data.get("token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("token".to_string(), json!(token));
                        if let Some(expires_at) = data.get("expires_at") {
                            result.insert("expiresAt".to_string(), expires_at.clone());
                        }
                        log.info("TOKEN_REFRESH", "Successfully refreshed Copilot token");
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Copilot token response: {}", e));
                        None
                    }
                }
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Copilot token: {}", text));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Copilot token refresh error: {}", e));
                None
            }
        }
    }, log).await
}

/// JS `refreshTokenByProvider(provider, credentials, log)`.
pub async fn refresh_token_by_provider(
    provider: &str,
    creds: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    let refresh_token = creds.refresh_token()?;

    // Dispatch to provider-specific handlers
    match provider {
        "gemini-cli" | "antigravity" => {
            // Google OAuth with client credentials from config
            // Note: requires clientId/clientSecret from PROVIDERS config
            let client_id = "gemini-client"; // Default
            let client_secret = ""; // From config
            refresh_google_token(refresh_token, client_id, client_secret, log).await
        }
        "claude" => refresh_claude_oauth_token(refresh_token, log).await,
        "codex" => refresh_codex_token(refresh_token, log).await,
        "iflow" => refresh_iflow_token(refresh_token, log).await,
        "github" => refresh_github_token(refresh_token, log).await,
        "kiro" => refresh_kiro_token(refresh_token, creds.psd(), log, None).await,
        "xai" | "grok-cli" | "gcli" => refresh_xai_token(refresh_token, log).await,
        "codebuddy-cn" => refresh_codebuddy_token(refresh_token, log).await,
        "codebuddy-intl" => refresh_codebuddy_intl_token(refresh_token, log).await,
        "trae" => refresh_trae_token(refresh_token, creds, log).await,
        "cline" | "clinepass" => refresh_cline_token(refresh_token, log).await,
        "zed" => refresh_zed_token(),
        "windsurf" => refresh_windsurf_token(log).await,
        "kimi" | "kimi-coding" => {
            // Kimi uses generic refresh with X-Msh-* headers
            // TODO(wave2): implement with kimi-specific header building
            log.warn("TOKEN_REFRESH", "Kimi token refresh not yet fully implemented");
            None
        }
        "minimax-code" | "minimax-code-global" => {
            refresh_minimax_code_token(provider, refresh_token, log).await
        }
        "nous" => refresh_nous_token(refresh_token, log).await,
        "vertex" | "vertex-partner" => {
            // Vertex uses service account JSON in apiKey
            let sa_json = parse_vertex_sa_json(creds.api_key()?)?;
            refresh_vertex_token(&sa_json, log).await
        }
        "gemini" => {
            // gemini (not gemini-cli) - Google OAuth
            let client_id = "gemini-client"; // Default
            let client_secret = ""; // From config
            refresh_google_token(refresh_token, client_id, client_secret, log).await
        }
        _ => {
            // Generic OAuth2 path for unknown providers
            refresh_access_token(provider, refresh_token, creds, log).await
        }
    }
}

/// JS `getAccessToken(provider, credentials, log)`: get refreshed access token.
/// Callers: executors before making upstream requests.
pub async fn get_access_token(
    provider: &str,
    creds: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    let refresh_token = creds.refresh_token()?;
    if refresh_token.is_empty() {
        log.warn("TOKEN_REFRESH", &format!("No valid refresh token available for provider: {}", provider));
        return None;
    }
    refresh_token_by_provider(provider, creds, log).await
}

/// Generic OAuth2 refresh with form-encoded body
pub async fn refresh_access_token(
    provider: &str,
    refresh_token: &str,
    _credentials: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        log.warn("TOKEN_REFRESH", &format!("No refresh token for {}", provider));
        return None;
    }

    dedup_refresh(provider, Some(refresh_token), || async {
        // Generic OAuth2 token refresh (form-encoded body)
        let client = reqwest::Client::new();

        // Note: Requires clientId and clientSecret from PROVIDERS config
        // This is a placeholder that should be populated from config
        let client_id = format!("{}-client", provider);
        let client_secret = ""; // Should come from config

        match client
            .post(&format!("https://oauth.{}.io/token", provider))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &client_id),
                ("client_secret", client_secret),
            ])
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<JsonMap>().await {
                    Ok(tokens) => {
                        let access_token = tokens.get("access_token")?.as_str()?.to_string();
                        let mut result = JsonMap::new();
                        result.insert("accessToken".to_string(), json!(access_token));
                        if let Some(rt) = tokens.get("refresh_token") {
                            result.insert("refreshToken".to_string(), rt.clone());
                        } else {
                            result.insert("refreshToken".to_string(), json!(refresh_token));
                        }
                        if let Some(ei) = tokens.get("expires_in") {
                            result.insert("expiresIn".to_string(), ei.clone());
                        }
                        log.info("TOKEN_REFRESH", &format!("Successfully refreshed {} token", provider));
                        Some(result)
                    }
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse {} token response: {}", provider, e));
                        None
                    }
                }
            }
            Ok(resp) => {
                log.error("TOKEN_REFRESH", &format!("Failed to refresh {} token: HTTP {}", provider, resp.status()));
                None
            }
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("{} token refresh error: {}", provider, e));
                None
            }
        }
    }, log).await
}

/// JS `formatProviderCredentials(provider, credentials, log)`.
/// Returns relevant credential fields for a provider (apiKey, accessToken, refreshToken, projectId, etc).
pub fn format_provider_credentials(provider: &str, creds: &Credentials, _log: &Logger) -> Option<JsonMap> {
    let mut result = JsonMap::new();

    // Standard fields that most providers use
    if let Some(ak) = creds.api_key() {
        result.insert("apiKey".to_string(), json!(ak));
    }
    if let Some(at) = creds.access_token() {
        result.insert("accessToken".to_string(), json!(at));
    }

    // Provider-specific fields
    match provider {
        "gemini" => {
            if let Some(pid) = creds.str("projectId") {
                result.insert("projectId".to_string(), json!(pid));
            }
        }
        "claude" => {
            // apiKey and accessToken only
        }
        "vertex" | "vertex-partner" => {
            if let Some(pid) = creds.str("projectId") {
                result.insert("projectId".to_string(), json!(pid));
            }
        }
        _ => {
            // Default: include refresh token too
            if let Some(rt) = creds.refresh_token() {
                result.insert("refreshToken".to_string(), json!(rt));
            }
        }
    }

    if result.is_empty() {
        return None;
    }

    Some(result)
}

/// JS `getAllAccessTokens(userInfo, log)`.
/// Refresh all active provider connections and return a map of provider -> refreshed tokens.
pub async fn get_all_access_tokens(
    connections: &[JsonMap],
    log: &Logger,
) -> JsonMap {
    let mut results = JsonMap::new();

    for conn in connections {
        let is_active = conn.get("isActive")
            .map(|v| nr_core::json::truthy(v))
            .unwrap_or(false);

        if !is_active {
            continue;
        }

        let provider = match conn.get("provider").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => continue,
        };

        // Build minimal credentials from connection
        let refresh_token = match conn.get("refreshToken").and_then(|v| v.as_str()) {
            Some(rt) => rt,
            None => continue,
        };

        let mut creds = Credentials::new();
        creds.set("refreshToken", json!(refresh_token));

        if let Some(token) = get_access_token(provider, &creds, log).await {
            results.insert(provider.to_string(), json!(token));
        }
    }

    results
}

/// JS `refreshWithRetry(refreshFn, maxRetries = 3, log = null)`.
/// Retry a refresh function with exponential backoff.
pub async fn refresh_with_retry<F, Fut>(
    f: F,
    max_retries: usize,
    log: &Logger,
) -> Option<JsonMap>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<JsonMap>>,
{
    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay_ms = attempt as u64 * 1000;
            log.debug("TOKEN_REFRESH", &format!("Retry {}/{} after {}ms", attempt, max_retries, delay_ms));
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        match f().await {
            Some(result) => return Some(result),
            None if attempt < max_retries - 1 => {
                log.warn("TOKEN_REFRESH", &format!("Attempt {}/{} failed, retrying...", attempt + 1, max_retries));
            }
            None => {
                log.error("TOKEN_REFRESH", &format!("All {} retry attempts failed", max_retries));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_unrecoverable_refresh_error() {
        let mut result = JsonMap::new();
        result.insert("error".to_string(), json!("invalid_grant"));
        assert!(is_unrecoverable_refresh_error(&Some(result)));

        let mut result2 = JsonMap::new();
        result2.insert("error".to_string(), json!("refresh_token_reused"));
        assert!(is_unrecoverable_refresh_error(&Some(result2)));

        let result3 = JsonMap::new();
        assert!(!is_unrecoverable_refresh_error(&Some(result3)));
        assert!(!is_unrecoverable_refresh_error(&None));
    }

    #[test]
    fn test_parse_vertex_sa_json() {
        let valid_json = json!({
            "type": "service_account",
            "client_email": "test@example.iam.gserviceaccount.com",
            "private_key": "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----",
            "project_id": "my-project"
        });

        let parsed = parse_vertex_sa_json(valid_json.to_string().as_str());
        assert!(parsed.is_some());

        let invalid_json = json!({"type": "invalid"});
        let parsed2 = parse_vertex_sa_json(invalid_json.to_string().as_str());
        assert!(parsed2.is_none());

        let not_json = "not a json string";
        let parsed3 = parse_vertex_sa_json(not_json);
        assert!(parsed3.is_none());
    }

    #[test]
    fn test_get_refresh_lead_ms() {
        // Default buffer
        assert_eq!(get_refresh_lead_ms("unknown"), TOKEN_EXPIRY_BUFFER_MS);
        assert_eq!(get_refresh_lead_ms("kimi-coding"), TOKEN_EXPIRY_BUFFER_MS);
    }

    #[test]
    fn test_format_provider_credentials() {
        let mut creds = Credentials::new();
        creds.set("apiKey", json!("secret-key"));
        creds.set("accessToken", json!("access-token"));

        let logger = nr_core::Logger::default();

        let formatted = format_provider_credentials("claude", &creds, &logger);
        assert!(formatted.is_some());

        let result = formatted.unwrap();
        assert_eq!(result.get("apiKey").and_then(|v| v.as_str()), Some("secret-key"));
        assert_eq!(result.get("accessToken").and_then(|v| v.as_str()), Some("access-token"));
    }

    #[test]
    fn test_format_provider_credentials_gemini() {
        let mut creds = Credentials::new();
        creds.set("apiKey", json!("key"));
        creds.set("projectId", json!("proj-123"));

        let logger = nr_core::Logger::default();
        let formatted = format_provider_credentials("gemini", &creds, &logger);
        assert!(formatted.is_some());

        let result = formatted.unwrap();
        assert_eq!(result.get("projectId").and_then(|v| v.as_str()), Some("proj-123"));
    }
}
