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

use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, UpstreamResponse};
use nr_core::http::ProxyOptions;
use nr_core::json::now_ms;
use nr_core::{Credentials, Error, Json, JsonMap, Logger, Result};

const TOKEN_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000; // 5 minutes
const REFRESH_RESULT_TTL_MS: i64 = 10_000; // 10 seconds (dedup cache TTL)

// xAI (Grok) OAuth issuer — CLIProxyAPI parity (src/lib/oauth/constants/xai.js).
const XAI_ISSUER: &str = "https://auth.x.ai";

// ── Refresh dedup (port of tokenRefresh/dedup.js) ────────────────────────────
// Every entry is either an in-flight refresh or a finished result kept for
// REFRESH_RESULT_TTL_MS. Concurrent callers for the same `provider:token` key
// await one shared refresh instead of issuing duplicate HTTP calls — required
// because several providers (MiniMax, Codex, Google) rotate single-use refresh
// tokens: a second send of the same token answers invalid_grant.
struct DedupEntry {
    /// Serializes the refresh for this key (the JS "in-flight promise").
    in_flight: tokio::sync::Mutex<()>,
    /// Finished result + expiry, set under `in_flight`.
    finished: parking_lot::Mutex<Option<(Option<JsonMap>, i64)>>,
}

lazy_static::lazy_static! {
    /// Cached xAI token endpoint from OIDC discovery.
    static ref XAI_DISCOVERY: RwLock<Option<String>> = RwLock::new(None);
}

lazy_static::lazy_static! {
    static ref REFRESH_DEDUP_CACHE: RwLock<HashMap<String, Arc<DedupEntry>>> =
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
/// Recent results (including `None` — a failed refresh is cached too, matching
/// JS) are reused within the TTL window.
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
    let Some(old_token) = old_token.filter(|t| !t.is_empty()) else {
        // No token to dedup on; call directly
        return f().await;
    };

    let key = format!("{provider}:{old_token}");
    let entry = {
        let mut cache = REFRESH_DEDUP_CACHE.write();
        cache
            .entry(key)
            .or_insert_with(|| {
                Arc::new(DedupEntry {
                    in_flight: tokio::sync::Mutex::new(()),
                    finished: parking_lot::Mutex::new(None),
                })
            })
            .clone()
    };

    // Only one task runs the refresh per key; the rest wait here and then read
    // the finished result — this is JS's shared in-flight promise.
    let _guard = entry.in_flight.lock().await;

    {
        let finished = entry.finished.lock();
        if let Some((result, expires_at)) = finished.as_ref() {
            if *expires_at > now_ms() {
                log.info(
                    "TOKEN_REFRESH",
                    &format!("Reusing recent refresh result for {provider}"),
                );
                return result.clone();
            }
        }
    }

    let result = f().await;
    *entry.finished.lock() = Some((result.clone(), now_ms() + REFRESH_RESULT_TTL_MS));
    result
}

/// JS `getRefreshLeadMs(provider)`: per-provider lead from `PROVIDER_OAUTH[provider].refreshLeadMs`.
pub fn get_refresh_lead_ms(provider: &str) -> i64 {
    let lead = |id: &str| {
        nr_data::provider_oauth(id)
            .and_then(|o| o.get("refreshLeadMs"))
            .and_then(Json::as_i64)
    };
    if let Some(ms) = lead(provider) {
        return ms;
    }
    // Legacy id after kimi-coding → kimi merge
    if provider == "kimi-coding" {
        if let Some(ms) = lead("kimi") {
            return ms;
        }
    }
    TOKEN_EXPIRY_BUFFER_MS
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────
// All refresh calls go through `proxy_fetch` (port of proxyAwareFetch), which
// honors per-connection and env proxies; refreshes are never connection-bound
// so `ProxyOptions` is only threaded where the JS call passes `proxyOptions`
// (Kiro). Bodies are pre-encoded to exact JS wire bytes.

// Endpoint overrides for tests: point a provider's refresh URL at a local axum
// mock. Production code never sets these, so `endpoint_for` returns the
// configured value unchanged.
#[cfg(test)]
pub(crate) static TEST_ENDPOINTS: parking_lot::Mutex<Option<HashMap<String, String>>> =
    parking_lot::Mutex::new(None);

/// A registered test override for `provider:key`; always `None` in production.
/// Every refresh path that can reach the network resolves its URL through this
/// (directly or via `endpoint_for`) so no test ever talks to a live provider.
fn endpoint_override(_provider: &str, _key: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(map) = TEST_ENDPOINTS.lock().as_ref() {
            return map.get(&format!("{_provider}:{_key}")).cloned();
        }
    }
    None
}

/// Resolve a provider endpoint, honoring a test override when one is registered.
fn endpoint_for(provider: &str, key: &str, fallback: &str) -> String {
    endpoint_override(provider, key).unwrap_or_else(|| {
        config_str(provider, key).unwrap_or_else(|| fallback.to_string())
    })
}

/// `Some(s)` unless the resolved endpoint is empty (JS `url || null`).
fn some_non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// `fetch(url, { method: "POST", headers, body })` with a caller-built body.
async fn post_raw(
    url: &str,
    headers: HeaderList,
    body: String,
    proxy: Option<&ProxyOptions>,
) -> Result<UpstreamResponse> {
    let mut req = FetchRequest::new(http::Method::POST, url);
    req.headers = headers;
    req.body = Some(body.into_bytes().into());
    proxy_fetch(req, proxy).await
}

/// `fetch(url, { method: "POST", headers, body: new URLSearchParams(pairs) })`.
async fn post_form(
    url: &str,
    headers: HeaderList,
    pairs: &[(&str, &str)],
    proxy: Option<&ProxyOptions>,
) -> Result<UpstreamResponse> {
    let body = serde_urlencoded::to_string(pairs).unwrap_or_default();
    let mut headers = headers;
    if !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
        headers.insert("Content-Type".into(), "application/x-www-form-urlencoded".into());
    }
    post_raw(url, headers, body, proxy).await
}

/// `fetch(url, { method: "POST", headers, body: JSON.stringify(json) })`.
async fn post_json(
    url: &str,
    headers: HeaderList,
    body: &Json,
    proxy: Option<&ProxyOptions>,
) -> Result<UpstreamResponse> {
    let mut headers = headers;
    if !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
        headers.insert("Content-Type".into(), "application/json".into());
    }
    post_raw(url, headers, body.to_string(), proxy).await
}

/// `fetch(url, { method: "GET", headers })`.
async fn get(url: &str, headers: HeaderList) -> Result<UpstreamResponse> {
    let mut req = FetchRequest::get(url);
    req.headers = headers;
    proxy_fetch(req, None).await
}

/// The `Accept: application/json` header present on every JS refresh call.
fn accept_json(mut headers: HeaderList) -> HeaderList {
    if !headers.keys().any(|k| k.eq_ignore_ascii_case("accept")) {
        headers.insert("Accept".into(), "application/json".into());
    }
    headers
}

/// `PROVIDER_OAUTH[provider]?.[key] || PROVIDERS[provider]?.[key]`, non-empty string.
fn config_str(provider: &str, key: &str) -> Option<String> {
    fn field<'a>(root: Option<&'a Json>, key: &str) -> Option<&'a Json> {
        root.and_then(|o| o.get(key))
    }
    fn non_empty(v: &Json) -> Option<String> {
        v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
    }
    field(nr_data::provider_oauth(provider), key)
        .and_then(non_empty)
        .or_else(|| field(nr_data::provider(provider), key).and_then(non_empty))
}

/// `OAUTH_ENDPOINTS.google.token` — read from the nr-data appConstants dump so the
/// URL stays data-driven; falls back to the hardcoded literal on a stale dump.
fn google_token_endpoint() -> String {
    if let Some(url) = endpoint_override("google", "token") {
        return url;
    }
    nr_data::export("open-sse/config/appConstants", "OAUTH_ENDPOINTS")
        .and_then(|e| e.get("google"))
        .and_then(|g| g.get("token"))
        .and_then(Json::as_str)
        .unwrap_or("https://oauth2.googleapis.com/token")
        .to_string()
}

/// JS `PROVIDER_OAUTH[provider]` as an owned map (empty when unset).
fn provider_oauth_map(provider: &str) -> JsonMap {
    nr_data::provider_oauth(provider)
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default()
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
            if *expires_at - now_ms() > 5 * 60 * 1000 {
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
    let token_url = google_token_endpoint();
    let response = match post_form(
        &token_url,
        HeaderList::new(),
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ],
        None,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Vertex token mint network error: {}", e));
            return None;
        }
    };

    if !response.ok() {
        let err_text = response.text().await.unwrap_or_default();
        log.error("TOKEN_REFRESH", &format!("Vertex token mint failed: {}", err_text));
        return None;
    }

    match response.json_body().await {
        Ok(tokens) => {
            let access_token = tokens.get("access_token")?.as_str()?.to_string();
            let expires_in = tokens.get("expires_in")
                .and_then(|v| v.as_i64())
                .unwrap_or(3600);

            let expires_at = now_ms() + expires_in * 1000;

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

    dedup_refresh(&format!("google:{client_id}"), Some(refresh_token), || async {
        let url = google_token_endpoint();
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ];

        let response = match post_form(&url, accept_json(HeaderList::new()), &params, None).await {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Network error refreshing Google token: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            log.error("TOKEN_REFRESH", &format!("Failed to refresh Google token: {error_text}"));
            return None;
        }

        match response.json_body().await {
            Ok(tokens) => {
                log.info("TOKEN_REFRESH", "Successfully refreshed Google token");
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
                log.error("TOKEN_REFRESH", &format!("Failed to parse Google token response: {e}"));
                None
            }
        }
    }, log).await
}

/// JS `refreshKiroToken(refreshToken, providerSpecificData, log, proxyOptions)`.
pub async fn refresh_kiro_token(
    refresh_token: &str,
    provider_specific_data: Option<&JsonMap>,
    log: &Logger,
    proxy: Option<&ProxyOptions>,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("kiro", Some(refresh_token), || async {
        let psd = provider_specific_data?;
        let auth_method = psd.get("authMethod").and_then(|v| v.as_str());
        let psd_str = |k: &str| psd.get(k).and_then(Json::as_str);

        let (mut result, refreshed_arn) = if auth_method == Some("external_idp") {
            // CLIProxyAPI import: Microsoft OAuth2 form-encoded refresh against the
            // tenant's token endpoint. buildExternalIdpRefreshParams rejects any
            // endpoint off login.microsoftonline.com / login.microsoft.com /
            // login.windows.net, so a hostile saved endpoint can't be refreshed.
            let params = match build_external_idp_refresh_params(refresh_token, psd) {
                Ok(p) => p,
                Err(e) => {
                    log.warn("TOKEN_REFRESH", &format!("Invalid Kiro external_idp refresh config: {e}"));
                    return None;
                }
            };
            let body: Vec<(&str, &str)> = params.body.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let response = match post_form(
                &params.token_endpoint,
                accept_json(HeaderList::new()),
                &body,
                proxy,
            ).await {
                Ok(r) => r,
                Err(e) => {
                    log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro external_idp token: {e}"));
                    return None;
                }
            };
            if !response.ok() {
                let error_text = response.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro external_idp token: {error_text}"));
                return None;
            }
            let tokens = match response.json_body().await {
                Ok(Json::Object(m)) => m,
                Ok(_) => return None,
                Err(e) => {
                    log.error("TOKEN_REFRESH", &format!("Failed to parse Kiro external_idp token response: {e}"));
                    return None;
                }
            };
            log.info(
                "TOKEN_REFRESH",
                &format!(
                    "Successfully refreshed Kiro external_idp token hasNewAccessToken={} hasNewRefreshToken={} expiresIn={:?}",
                    tokens.get("access_token").and_then(Json::as_str).is_some(),
                    tokens.get("refresh_token").and_then(Json::as_str).is_some(),
                    tokens.get("expires_in").and_then(Json::as_i64),
                ),
            );
            let access_token = tokens.get("access_token").and_then(Json::as_str)?.to_string();
            let mut out = map_oauth_tokens(&tokens, &access_token, refresh_token, "refresh_token", "expires_in");
            // JS spreads the whole providerSpecificData (authMethod, clientId,
            // tokenEndpoint, scope, region, profileArn, …) into the result so the
            // importer's config survives the refresh.
            let mut next_psd = psd.clone();
            next_psd.insert("authMethod".to_string(), json!("external_idp"));
            next_psd.insert("clientId".to_string(), json!(params.client_id));
            next_psd.insert("tokenEndpoint".to_string(), json!(params.token_endpoint));
            next_psd.insert("scope".to_string(), json!(params.scope));
            out.insert("providerSpecificData".to_string(), Json::Object(next_psd));
            (out, None)
        } else {
            let client_id = psd_str("clientId");
            let client_secret = psd_str("clientSecret");
            let region = psd_str("region");

            if let (Some(cid), Some(csec)) = (client_id, client_secret) {
                // AWS Builder ID / IDC path: JSON body, camelCase keys.
                let is_idc = auth_method == Some("idc");
                let endpoint = match (is_idc, region) {
                    (true, Some(r)) => format!("https://oidc.{r}.amazonaws.com/token"),
                    _ => "https://oidc.us-east-1.amazonaws.com/token".to_string(),
                };
                let response = match post_json(
                    &endpoint,
                    accept_json(HeaderList::new()),
                    &json!({
                        "clientId": cid,
                        "clientSecret": csec,
                        "refreshToken": refresh_token,
                        "grantType": "refresh_token",
                    }),
                    proxy,
                ).await {
                    Ok(r) => r,
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro AWS token: {e}"));
                        return None;
                    }
                };
                if !response.ok() {
                    let error_text = response.text().await.unwrap_or_default();
                    log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro AWS token: {error_text}"));
                    return None;
                }
                let tokens = match response.json_body().await {
                    Ok(Json::Object(m)) => m,
                    Ok(_) => return None,
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Kiro AWS token response: {e}"));
                        return None;
                    }
                };
                let access_token = tokens.get("accessToken").and_then(Json::as_str)?;
                log.info(
                    "TOKEN_REFRESH",
                    &format!(
                        "Successfully refreshed Kiro AWS token hasNewAccessToken={} expiresIn={:?}",
                        !access_token.is_empty(),
                        tokens.get("expiresIn").and_then(Json::as_i64),
                    ),
                );
                let out = map_oauth_tokens(&tokens, access_token, refresh_token, "refreshToken", "expiresIn");
                (out, tokens.get("profileArn").and_then(Json::as_str).map(str::to_string))
            } else {
                // Social (email) login: Kiro's own refresh endpoint, kiro-cli UA.
                let url = endpoint_for("kiro", "tokenUrl", "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken");
                let mut headers = accept_json(HeaderList::new());
                headers.insert("User-Agent".into(), "kiro-cli/1.0.0".into());
                let response = match post_json(
                    &url,
                    headers,
                    &json!({ "refreshToken": refresh_token }),
                    proxy,
                ).await {
                    Ok(r) => r,
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro social token: {e}"));
                        return None;
                    }
                };
                if !response.ok() {
                    let error_text = response.text().await.unwrap_or_default();
                    log.error("TOKEN_REFRESH", &format!("Failed to refresh Kiro social token: {error_text}"));
                    return None;
                }
                let tokens = match response.json_body().await {
                    Ok(Json::Object(m)) => m,
                    Ok(_) => return None,
                    Err(e) => {
                        log.error("TOKEN_REFRESH", &format!("Failed to parse Kiro social token response: {e}"));
                        return None;
                    }
                };
                let access_token = tokens.get("accessToken").and_then(Json::as_str)?;
                log.info(
                    "TOKEN_REFRESH",
                    &format!(
                        "Successfully refreshed Kiro social token hasNewAccessToken={} expiresIn={:?}",
                        !access_token.is_empty(),
                        tokens.get("expiresIn").and_then(Json::as_i64),
                    ),
                );
                let out = map_oauth_tokens(&tokens, access_token, refresh_token, "refreshToken", "expiresIn");
                (out, tokens.get("profileArn").and_then(Json::as_str).map(str::to_string))
            }
        };

        // resolveKiroProfileArnPatch: keep the saved ARN; else discover one for the
        // freshly minted token (CodeWhisperer ListAvailableProfiles) so executors
        // keep targeting the right profile. Best-effort — never fails the refresh.
        // external_idp returns its own providerSpecificData (JS returns early there).
        let has_arn = psd.get("profileArn").and_then(Json::as_str).is_some_and(|s| !s.trim().is_empty());
        if auth_method != Some("external_idp") && !has_arn {
            let mut arn = refreshed_arn.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
            if arn.is_none() {
                let token = result.get("accessToken").and_then(Json::as_str).unwrap_or("");
                arn = fetch_kiro_profile_arn(token, log).await;
            }
            if let Some(arn) = arn {
                let mut next = result
                    .get("providerSpecificData")
                    .and_then(Json::as_object)
                    .cloned()
                    .unwrap_or_default();
                next.insert("profileArn".to_string(), json!(arn));
                result.insert("providerSpecificData".to_string(), Json::Object(next));
            }
        }

        Some(result)
    }, log).await
}

/// JS `mapOAuthTokens`-equivalent: copy access/refresh/expires from a provider
/// response into the refreshed-fields map. `refresh_key` falls back to the token
/// we sent when the answer omits it (JS `tokens.refreshToken || refreshToken`).
fn map_oauth_tokens(
    tokens: &JsonMap,
    access_token: &str,
    refresh_token: &str,
    refresh_key: &str,
    expires_key: &str,
) -> JsonMap {
    let mut result = JsonMap::new();
    result.insert("accessToken".to_string(), json!(access_token));
    let rt = tokens.get(refresh_key).and_then(Json::as_str).unwrap_or(refresh_token);
    result.insert("refreshToken".to_string(), json!(rt));
    if let Some(ei) = tokens.get(expires_key) {
        if !ei.is_null() {
            result.insert("expiresIn".to_string(), ei.clone());
        }
    }
    result
}

/// JS `fetchKiroProfileArn(accessToken)` — best-effort CodeWhisperer profile lookup.
async fn fetch_kiro_profile_arn(access_token: &str, log: &Logger) -> Option<String> {
    if access_token.is_empty() {
        return None;
    }
    let mut headers = accept_json(HeaderList::new());
    headers.insert("Authorization".into(), format!("Bearer {access_token}"));
    let response = match post_json(
        "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableProfiles",
        headers,
        &json!({ "maxResults": 10 }),
        None,
    ).await {
        Ok(r) if r.ok() => r,
        Ok(_) => return None,
        Err(e) => {
            log.debug("TOKEN_REFRESH", &format!("Kiro profile lookup failed: {e}"));
            return None;
        }
    };
    let data = response.json_body().await.ok()?;
    data.get("profiles")
        .and_then(Json::as_array)?
        .iter()
        .find_map(|p| p.get("arn").and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
}

/// JS `buildExternalIdpRefreshParams(refreshToken, providerSpecificData)`.
struct ExternalIdpRefresh {
    token_endpoint: String,
    client_id: String,
    scope: String,
    body: Vec<(&'static str, String)>,
}

fn build_external_idp_refresh_params(
    refresh_token: &str,
    psd: &JsonMap,
) -> Result<ExternalIdpRefresh> {
    // Accepts both the importer's camelCase keys and CLIProxyAPI's snake_case.
    let psd_str = |k: &str| {
        psd.get(k).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let camel_or_snake = |camel: &str, snake: &str| psd_str(camel).or_else(|| psd_str(snake));

    let client_id = camel_or_snake("clientId", "client_id");
    let token_endpoint = validate_microsoft_token_endpoint(
        &camel_or_snake("tokenEndpoint", "token_endpoint").unwrap_or_default(),
    )?;
    // `scopes` may be an array (CLIProxyAPI) or a space-joined string.
    let scope = match psd.get("scope").or_else(|| psd.get("scopes")) {
        Some(Json::Array(items)) => items
            .iter()
            .filter_map(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Some(v) => v.as_str().map(|s| s.trim().to_string()).unwrap_or_default(),
        None => String::new(),
    };

    if refresh_token.is_empty() {
        return Err(Error::new("refresh token is required"));
    }
    let client_id = client_id.ok_or_else(|| Error::new("clientId is required for external_idp refresh"))?;
    if scope.is_empty() {
        return Err(Error::new("scope is required for external_idp refresh"));
    }

    Ok(ExternalIdpRefresh {
        body: vec![
            ("grant_type", "refresh_token".to_string()),
            ("client_id", client_id.clone()),
            ("refresh_token", refresh_token.to_string()),
            ("scope", scope.clone()),
        ],
        client_id,
        scope,
        token_endpoint,
    })
}

/// JS `validateMicrosoftTokenEndpoint`: https + a Microsoft login host only.
fn validate_microsoft_token_endpoint(raw: &str) -> Result<String> {
    let endpoint = raw.trim();
    if endpoint.is_empty() {
        return Err(Error::new("token_endpoint is required"));
    }
    let parsed = url::Url::parse(endpoint).map_err(|_| Error::new("token_endpoint must be a valid URL"))?;
    if parsed.scheme() != "https" {
        return Err(Error::new("token_endpoint must use https"));
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    const HOSTS: [&str; 3] = ["login.microsoftonline.com", "login.microsoft.com", "login.windows.net"];
    if !HOSTS.contains(&host.as_str()) {
        return Err(Error::new("token_endpoint must be a Microsoft login endpoint"));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

/// JS `refreshMiniMaxCodeToken(provider, refreshToken, log)`.
///
/// MiniMax refresh tokens are SINGLE-USE: sending the same one twice answers
/// invalid_grant (HTTP 400) and signs the account out. The background scheduler
/// and the executor's on-401 retry may hold different snapshots of one
/// connection, so `dedup_refresh` keys on the spent token value — a token
/// already sent resolves to the cached result instead of a second send.
/// invalid_grant is the ONLY answer that means the sign-in is gone; anything
/// else (5xx, timeout) returns None and the current access token stays usable.
pub async fn refresh_minimax_code_token(
    provider: &str,
    refresh_token: &str,
    log: &Logger,
) -> Option<JsonMap> {
    let fallback = match provider {
        "minimax-code" => "https://account.minimax.cn/oauth2/token",
        "minimax-code-global" => "https://account.minimax.io/oauth2/token",
        _ => return None,
    };
    let token_url = endpoint_for(provider, "tokenUrl", fallback);
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh(provider, Some(refresh_token), || async {
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", "mcode-public"),
            ("scope", "agent.default"),
            ("audience", "agent-backend"),
        ];

        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            post_form(&token_url, accept_json(HeaderList::new()), &params, None),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing MiniMax token ({provider}): {e}"));
                return None;
            }
            Err(_) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing MiniMax token ({provider}): timed out"));
                return None;
            }
        };

        let status = response.status;
        // JS reads the body even on a non-2xx so it can classify invalid_grant.
        let data = response.json_body().await.ok().and_then(|v| v.as_object().cloned());
        let has_error = data.as_ref().is_some_and(|d| d.get("error").and_then(Json::as_str).is_some_and(|s| !s.is_empty()));

        if !(200..300).contains(&status) || has_error {
            let data = data.unwrap_or_default();
            if status == 400 && data.get("error").and_then(Json::as_str) == Some("invalid_grant") {
                log.warn("TOKEN_REFRESH", &format!("MiniMax refresh token spent/rejected ({provider}) — sign-in gone"));
                let mut error_result = JsonMap::new();
                error_result.insert("error".to_string(), json!("invalid_grant"));
                return Some(error_result);
            }
            let code = data
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}"));
            log.error("TOKEN_REFRESH", &format!("MiniMax token refresh failed ({provider}) status={status} error={code}"));
            return None;
        }

        let data = data?;
        let access_token = match data.get("access_token").and_then(Json::as_str) {
            Some(t) => t,
            None => {
                log.error("TOKEN_REFRESH", &format!("MiniMax refresh returned no access_token ({provider})"));
                return None;
            }
        };
        // MiniMax rotates the refresh token on every refresh; keep the old one
        // only if the answer oddly omits it.
        let new_refresh = data.get("refresh_token").and_then(Json::as_str).unwrap_or(refresh_token);
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        result.insert("refreshToken".to_string(), json!(new_refresh));
        if let Some(expires_in) = data.get("expires_in") {
            if !expires_in.is_null() {
                result.insert("expiresIn".to_string(), expires_in.clone());
            }
        }
        Some(result)
    }, log).await
}

/// JS `refreshClineToken(refreshToken, log)`.
pub async fn refresh_cline_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("cline", Some(refresh_token), || async {
        let url = endpoint_for("cline", "refreshUrl", "https://api.cline.bot/api/v1/auth/refresh");
        let response = match post_json(
            &url,
            accept_json(HeaderList::new()),
            &json!({
                "refreshToken": refresh_token,
                "grantType": "refresh_token",
                "clientType": "extension",
            }),
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing Cline token: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            log.error("TOKEN_REFRESH", &format!("Failed to refresh Cline token: {error_text}"));
            return None;
        }

        let body = response.json_body().await.ok()?;
        // `body?.data || body` — the extension wraps the token in `data`.
        let tokens = body
            .get("data")
            .filter(|d| nr_core::json::truthy(d))
            .cloned()
            .unwrap_or(body);
        let access_token = tokens.get("accessToken").and_then(Json::as_str)?;
        if access_token.is_empty() {
            return None;
        }

        // expiresAt is an ISO timestamp; fall back to expiresIn/expires_in/3600.
        let expires_in = tokens
            .get("expiresAt")
            .and_then(Json::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| std::cmp::max(1, (dt.with_timezone(&Utc) - Utc::now()).num_seconds()))
            .or_else(|| tokens.get("expiresIn").and_then(Json::as_i64))
            .or_else(|| tokens.get("expires_in").and_then(Json::as_i64))
            .unwrap_or(3600);

        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        let rt = tokens.get("refreshToken").and_then(Json::as_str).unwrap_or(refresh_token);
        result.insert("refreshToken".to_string(), json!(rt));
        result.insert("expiresIn".to_string(), json!(expires_in));
        Some(result)
    }, log).await
}

/// Per-provider refresh variants for the generic path (JS `REFRESH_PROFILES`).
/// Keys not listed fall back to the default form-encoded OAuth2 refresh with
/// client_id + client_secret read from config.
struct RefreshProfile {
    /// JSON body instead of the default form-encoded one.
    body_format: BodyFormat,
    include_client_secret: bool,
}

enum BodyFormat {
    Json,
    Form,
}

fn refresh_profile(provider: &str) -> RefreshProfile {
    match provider {
        // JSON body, client_id only, anthropic endpoint (≠ claude.ai login): the
        // profile carries no URL — `PROVIDER_OAUTH.claude.tokenUrl` is that endpoint.
        "claude" => RefreshProfile { body_format: BodyFormat::Json, include_client_secret: false },
        // Basic auth header; form body keeps client_id + client_secret.
        "iflow" => RefreshProfile { body_format: BodyFormat::Form, include_client_secret: true },
        // Public GitHub CLI client — no secret on file.
        "github" => RefreshProfile { body_format: BodyFormat::Form, include_client_secret: false },
        // JSON body, client_id only (PKCE public client), no secret.
        "perchai" => RefreshProfile { body_format: BodyFormat::Json, include_client_secret: false },
        _ => RefreshProfile { body_format: BodyFormat::Form, include_client_secret: true },
    }
}

/// JS `refreshAccessToken(provider, refreshToken, credentials, log)`: the generic
/// config-driven refresh, specialised per provider by `REFRESH_PROFILES`.
pub async fn refresh_access_token(
    provider: &str,
    refresh_token: &str,
    credentials: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    let profile = refresh_profile(provider);
    // `extraHeaders` equivalent: Kimi's X-Msh-* device headers.
    let kimi_headers: Vec<(String, String)> = if provider == "kimi" {
        nr_data::build_kimi_headers(credentials.psd_str("deviceId"))
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect()
    } else {
        Vec::new()
    };
    let url = endpoint_override(provider, "refreshUrl")
        .or_else(|| endpoint_override(provider, "tokenUrl"))
        // JS `resolveRefreshUrl`: config.refreshUrl, then config.tokenUrl.
        .or_else(|| some_non_empty(endpoint_for(provider, "refreshUrl", "")))
        .or_else(|| some_non_empty(endpoint_for(provider, "tokenUrl", "")))?;
    let client_id = config_str(provider, "clientId")?;

    if refresh_token.is_empty() {
        log.warn("TOKEN_REFRESH", &format!("No refresh token available for provider: {provider}"));
        return None;
    }

    dedup_refresh(provider, Some(refresh_token), || async {
        let mut payload = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
            ("client_id", client_id.clone()),
        ];
        if profile.include_client_secret {
            if let Some(secret) = config_str(provider, "clientSecret") {
                payload.push(("client_secret", secret));
            }
        }

        let mut headers = accept_json(HeaderList::new());
        let response = match profile.body_format {
            BodyFormat::Json => {
                let body: JsonMap = payload
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), Json::String(v)))
                    .collect();
                post_json(&url, headers, &Json::Object(body), None).await
            }
            BodyFormat::Form => {
                // Kimi Code OAuth: CLIProxyAPI `X-Msh-*` device headers.
                for (k, v) in &kimi_headers {
                    headers.insert(k.clone(), v.clone());
                }
                // iFlow authenticates with a Basic header on client_id:client_secret.
                if provider == "iflow" {
                    if let Some(secret) = config_str("iflow", "clientSecret") {
                        let raw = format!("{client_id}:{secret}");
                        let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                        headers.insert("Authorization".into(), format!("Basic {encoded}"));
                    }
                }
                let pairs: Vec<(&str, &str)> = payload.iter().map(|(k, v)| (*k, v.as_str())).collect();
                post_form(&url, headers, &pairs, None).await
            }
        };

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing token for {provider}: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            log.error("TOKEN_REFRESH", &format!("Failed to refresh token for {provider}: {error_text}"));
            return None;
        }

        let tokens = match response.json_body().await {
            Ok(t) => t,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Failed to parse {provider} token response: {e}"));
                return None;
            }
        };

        log.info("TOKEN_REFRESH", &format!(
            "Successfully refreshed token for {provider} hasNewAccessToken={} hasNewRefreshToken={} expiresIn={:?}",
            tokens.get("access_token").and_then(Json::as_str).is_some(),
            tokens.get("refresh_token").and_then(Json::as_str).is_some(),
            tokens.get("expires_in").and_then(Json::as_i64),
        ));

        let access_token = tokens.get("access_token").and_then(Json::as_str).filter(|s| !s.is_empty())?;
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        let sent = refresh_token;
        result.insert("refreshToken".to_string(), json!(tokens.get("refresh_token").and_then(Json::as_str).unwrap_or(sent)));
        if let Some(expires_in) = tokens
            .get("expires_in")
            // GitHub's device-flow refresh answers `refresh_token_expires_in`
            // (the refresh token's lifetime) and may omit `expires_in`.
            .or_else(|| tokens.get("refresh_token_expires_in"))
        {
            result.insert("expiresIn".to_string(), expires_in.clone());
        }
        result.insert("_client_id".to_string(), json!(client_id));
        Some(result)
    }, log).await
}

/// Claude OAuth: JSON body, client_id only. Delegate to refreshAccessToken("claude", …).
pub async fn refresh_claude_oauth_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    refresh_access_token("claude", refresh_token, &Credentials::default(), log).await
}

/// iFlow: Basic Auth + client_id/client_secret in the form body.
pub async fn refresh_iflow_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    refresh_access_token("iflow", refresh_token, &Credentials::default(), log).await
}

/// GitHub: optional client_secret (omitted — the CLI client is public).
pub async fn refresh_github_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    refresh_access_token("github", refresh_token, &Credentials::default(), log).await
}

/// Codex (OpenAI) token refresh
pub async fn refresh_codex_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    dedup_refresh("codex", Some(refresh_token), || async {
        let client_id = config_str("codex", "clientId")
            .unwrap_or_else(|| "app_EMoamEEZ73f0CkXaXp7hrann".to_string());
        let url = endpoint_for("codex", "tokenUrl", "https://auth.openai.com/oauth/token");

        let response = match post_json(
            &url,
            accept_json(HeaderList::new()),
            &json!({
                "client_id": client_id,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }),
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Network error refreshing Codex token: {e}"));
                return None;
            }
        };

        let status = response.status;
        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            let failure = classify_oauth_refresh_error(&error_text, status);
            if failure.permanent {
                log.error("TOKEN_REFRESH", "Codex refresh token already used or invalid. Re-auth required.");
                let mut result = JsonMap::new();
                result.insert("error".to_string(), json!("unrecoverable_refresh_error"));
                result.insert("code".to_string(), json!(failure.code));
                return Some(result);
            }
            log.error("TOKEN_REFRESH", &format!("Failed to refresh Codex token: {error_text}"));
            return None;
        }

        let tokens = match response.json_body().await {
            Ok(Json::Object(m)) => m,
            Ok(_) => return None,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Failed to parse Codex token response: {e}"));
                return None;
            }
        };
        log.info("TOKEN_REFRESH", &format!(
            "Successfully refreshed Codex token hasNewAccessToken={} hasNewRefreshToken={} hasIdToken={} expiresIn={:?}",
            tokens.get("access_token").and_then(Json::as_str).is_some(),
            tokens.get("refresh_token").and_then(Json::as_str).is_some(),
            tokens.get("id_token").and_then(Json::as_str).is_some(),
            tokens.get("expires_in").and_then(Json::as_i64),
        ));
        let access_token = tokens.get("access_token").and_then(Json::as_str)?.to_string();
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        let rt = tokens.get("refresh_token").and_then(Json::as_str).unwrap_or(refresh_token);
        result.insert("refreshToken".to_string(), json!(rt));
        if let Some(id) = tokens.get("id_token") {
            if nr_core::json::truthy(id) {
                result.insert("idToken".to_string(), id.clone());
            }
        }
        if let Some(ei) = tokens.get("expires_in") {
            result.insert("expiresIn".to_string(), ei.clone());
        }
        Some(result)
    }, log).await
}
/// Trae token refresh - POST ExchangeToken with JSON body
pub async fn refresh_trae_token(refresh_token: &str, _credentials: &Credentials, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // No Trae entry in the nr-data dumps (registry snapshot predates it), so the
    // exchange endpoint falls back to the registry value from trae.js.
    let oauth = provider_oauth_map("trae");
    let url = endpoint_for("trae", "exchangeTokenUrl", "https://api.marscode.com/cloudide/api/v3/trae/oauth/ExchangeToken");
    let client_id = oauth.get("clientId").and_then(Json::as_str).filter(|s| !s.is_empty()).unwrap_or("ono9krqynydwx5");
    let client_secret = oauth.get("clientSecret").and_then(Json::as_str).filter(|s| !s.is_empty()).unwrap_or("-");

    dedup_refresh("trae", Some(refresh_token), || async {
        let mut headers = accept_json(HeaderList::new());
        headers.insert("User-Agent".into(), "Trae/1.0.0 antigravity-cockpit-tools".into());

        let response = match post_json(
            &url,
            headers,
            &json!({
                "ClientID": client_id,
                "RefreshToken": refresh_token,
                "ClientSecret": client_secret,
                "UserID": "",
            }),
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing Trae token: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            log.error("TOKEN_REFRESH", &format!("Failed to refresh Trae token: {error_text}"));
            return None;
        }

        let payload = match response.json_body().await {
            Ok(Json::Object(m)) => m,
            Ok(_) => return None,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing Trae token: {e}"));
                return None;
            }
        };
        // `payload?.Result || payload?.result || payload`
        let result_obj = payload
            .get("Result")
            .or_else(|| payload.get("result"))
            .filter(|v| nr_core::json::truthy(v))
            .cloned()
            .unwrap_or(Json::Object(payload));
        let access_token = match result_obj
            .get("AccessToken")
            .or_else(|| result_obj.get("accessToken"))
            .and_then(Json::as_str)
        {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                log.error("TOKEN_REFRESH", "Trae refresh returned no AccessToken");
                return None;
            }
        };

        let new_refresh = result_obj
            .get("RefreshToken")
            .or_else(|| result_obj.get("refreshToken"))
            .and_then(Json::as_str)
            .unwrap_or(refresh_token)
            .to_string();

        // expiresAt is either epoch seconds or an ISO timestamp; JS leaves
        // expiresIn undefined when the string is in the past.
        let expires_at = result_obj.get("ExpiresAt").or_else(|| result_obj.get("expiresAt"));
        let expires_in = match expires_at {
            Some(Json::Number(n)) => n.as_i64().map(|s| std::cmp::max(1, s - Utc::now().timestamp())),
            Some(Json::String(s)) => chrono::DateTime::parse_from_rfc3339(s).ok().and_then(|dt| {
                let ms = (dt.with_timezone(&Utc) - Utc::now()).num_milliseconds();
                (ms > 0).then(|| ms / 1000)
            }),
            _ => None,
        };

        log.info("TOKEN_REFRESH", &format!(
            "Successfully refreshed Trae token hasNewAccessToken=true hasNewRefreshToken={} expiresIn={expires_in:?}",
            new_refresh != refresh_token,
        ));
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        result.insert("refreshToken".to_string(), json!(new_refresh));
        if let Some(ei) = expires_in {
            result.insert("expiresIn".to_string(), json!(ei));
        }
        Some(result)
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

/// JS `refreshNousToken(refreshToken, log)`.
///
/// Flow: refresh_token → new access_token → mint a fresh single-purpose agent
/// key (best effort; a failed mint falls back to the access token so the
/// connection keeps working).
pub async fn refresh_nous_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    let token_url = endpoint_for("nous", "tokenUrl", "https://portal.nousresearch.com/api/oauth/token");
    let agent_key_url = endpoint_for("nous", "agentKeyUrl", "https://portal.nousresearch.com/api/oauth/agent-key");
    let client_id = config_str("nous", "clientId").unwrap_or_else(|| "hermes-cli".to_string());

    dedup_refresh("nous", Some(refresh_token), || async {
        // Step 1: Refresh the access token
        let mut headers = accept_json(HeaderList::new());
        headers.insert("x-nous-refresh-token".into(), refresh_token.to_string());
        let token_response = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            post_form(
                &token_url,
                headers,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", &client_id),
                ],
                None,
            ),
        )
        .await
        {
            Ok(Ok(r)) if r.ok() => r.json_body().await.ok(),
            Ok(Ok(r)) => {
                let error = r.text().await.unwrap_or_default();
                log.error("TOKEN_REFRESH", &format!("Failed to refresh Nous token: {error}"));
                return None;
            }
            Ok(Err(e)) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing Nous token: {e}"));
                return None;
            }
            Err(_) => {
                log.error("TOKEN_REFRESH", "Error refreshing Nous token: timed out");
                return None;
            }
        };

        let token_data = token_response?;
        let access_token = match token_data.get("access_token").and_then(Json::as_str) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                log.error("TOKEN_REFRESH", "Nous token refresh returned no access_token");
                return None;
            }
        };
        let new_refresh = token_data
            .get("refresh_token")
            .and_then(Json::as_str)
            .unwrap_or(refresh_token)
            .to_string();

        // Step 2: Mint agent key (best effort; failures don't block the result)
        let mut agent_key = None;
        let mut agent_key_id = None;
        let mut agent_key_expires_at = None;

        let mut mint_headers = accept_json(HeaderList::new());
        mint_headers.insert("Authorization".into(), format!("Bearer {access_token}"));
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            post_json(&agent_key_url, mint_headers, &json!({ "min_ttl_seconds": 7200 }), None),
        )
        .await
        {
            Ok(Ok(r)) if r.ok() => {
                if let Ok(agent_data) = r.json_body().await {
                    agent_key = agent_data.get("api_key").and_then(Json::as_str).filter(|s| !s.is_empty()).map(str::to_string);
                    agent_key_id = agent_data.get("key_id").and_then(Json::as_str).filter(|s| !s.is_empty()).map(str::to_string);
                    agent_key_expires_at = agent_data.get("expires_at").filter(|v| nr_core::json::truthy(v)).cloned();
                    log.info("TOKEN_REFRESH", "Nous agent key minted successfully");
                }
            }
            Ok(Ok(r)) => {
                let error = r.text().await.unwrap_or_default();
                log.warn("TOKEN_REFRESH", &format!("Failed to mint Nous agent key, falling back to access_token: {error}"));
            }
            Ok(Err(e)) => {
                log.warn("TOKEN_REFRESH", &format!("Nous agent key mint error: {e}"));
            }
            Err(_) => {
                log.warn("TOKEN_REFRESH", "Nous agent key mint error: timed out");
            }
        }

        // Build result
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        result.insert("refreshToken".to_string(), json!(new_refresh));
        if let Some(ei) = token_data.get("expires_in") {
            result.insert("expiresIn".to_string(), ei.clone());
        }

        // JS always emits both `apiKey` and `agentKey`; agentKey is null when the
        // mint failed and apiKey falls back to the access token.
        result.insert("apiKey".to_string(), json!(agent_key.clone().unwrap_or_else(|| access_token.clone())));
        result.insert("agentKey".to_string(), json!(agent_key));

        // Include provider-specific data for agent key metadata
        let mut psd = JsonMap::new();
        if let Some(id) = agent_key_id {
            psd.insert("agentKeyId".to_string(), json!(id));
        }
        if let Some(exp) = agent_key_expires_at {
            psd.insert("agentKeyExpiresAt".to_string(), exp);
        }
        result.insert("providerSpecificData".to_string(), Json::Object(psd));

        log.info("TOKEN_REFRESH", "Successfully refreshed Nous token");
        Some(result)
    }, log).await
}

/// xAI issuer OIDC discovery (cached process-wide, like `discoverEndpoints()`).
async fn discover_xai_token_url(log: &Logger) -> String {
    const DISCOVERY: &str = "https://auth.x.ai/.well-known/openid-configuration";
    let fallback = format!("{XAI_ISSUER}/oauth2/token");
    // A registered endpoint (test override) wins over discovery, so a test
    // refresh never depends on a live GET to auth.x.ai.
    if let Some(url) = endpoint_override("xai", "refreshUrl")
        .or_else(|| endpoint_override("xai", "tokenUrl"))
    {
        return url;
    }
    if let Some(cached) = XAI_DISCOVERY.read().clone() {
        return cached;
    }
    let url = match get(DISCOVERY, accept_json(HeaderList::new())).await {
        Ok(r) if r.ok() => match r.json_body().await {
            Ok(data) => data
                .get("token_endpoint")
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|u| validate_xai_endpoint(u).is_some())
                .unwrap_or(&fallback)
                .to_string(),
            Err(_) => fallback.clone(),
        },
        _ => fallback.clone(),
    };
    if url != fallback {
        log.debug("TOKEN_REFRESH", &format!("xAI discovery: token_endpoint={url}"));
    }
    *XAI_DISCOVERY.write() = Some(url.clone());
    url
}

/// JS `validateOAuthEndpoint`: https + a host under x.ai (discovery is untrusted).
fn validate_xai_endpoint(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw.trim()).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    (host == "x.ai" || host.ends_with(".x.ai")).then(|| raw.trim().to_string())
}

/// JS `refreshXaiToken(refreshToken, log)` — xAI is a public PKCE client, so the
/// refresh body carries `client_id` and no secret.
pub async fn refresh_xai_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    let token_url = discover_xai_token_url(log).await;
    let client_id = config_str("xai", "clientId")
        .or_else(|| config_str("grok-cli", "clientId"))
        .unwrap_or_else(|| "b1a00492-073a-47ea-816f-4c329264a828".to_string());

    dedup_refresh("xai", Some(refresh_token), || async {
        let response = match post_form(
            &token_url,
            accept_json(HeaderList::new()),
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", refresh_token),
            ],
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log.warn("TOKEN_REFRESH", &format!("xai refresh failed: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let text = response.text().await.unwrap_or_default();
            log.warn("TOKEN_REFRESH", &format!("xai refresh failed: {text}"));
            if text.contains("invalid_grant") || text.contains("invalid_request") {
                let mut result = JsonMap::new();
                result.insert("error".to_string(), json!("invalid_grant"));
                return Some(result);
            }
            return None;
        }

        let tokens = match response.json_body().await {
            Ok(Json::Object(m)) => m,
            Ok(_) => return None,
            Err(e) => {
                log.warn("TOKEN_REFRESH", &format!("xai refresh failed: {e}"));
                return None;
            }
        };
        let access_token = tokens.get("access_token").and_then(Json::as_str)?;
        // JS emits idToken unconditionally (undefined dropped on the wire, null kept
        // here to match "not provided" vs "cleared" only when truly absent).
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        let rt = tokens.get("refresh_token").and_then(Json::as_str).unwrap_or(refresh_token);
        result.insert("refreshToken".to_string(), json!(rt));
        if let Some(ei) = tokens.get("expires_in") {
            result.insert("expiresIn".to_string(), ei.clone());
        }
        if let Some(id) = tokens.get("id_token") {
            result.insert("idToken".to_string(), id.clone());
        }
        Some(result)
    }, log).await
}

/// CodeBuddy (Tencent) refresh — POST /v2/plugin/auth/token/refresh with the
/// refresh token carried in the X-Refresh-Token header (not a form body),
/// matching the official CodeBuddy CLI. Response: `{ code: 0, data: <token> }`.
/// `provider` is `codebuddy-cn` or `codebuddy-intl`; both read their endpoint,
/// user agent and X-Domain from `PROVIDER_OAUTH`.
async fn refresh_codebuddy_variant(provider: &str, refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }
    let oauth = provider_oauth_map(provider);
    let url = endpoint_for(provider, "refreshUrl", if provider == "codebuddy-intl" {
        "https://www.codebuddy.ai/v2/plugin/auth/token/refresh"
    } else {
        "https://copilot.tencent.com/v2/plugin/auth/token/refresh"
    });
    let user_agent = oauth.get("userAgent").and_then(Json::as_str).unwrap_or("").to_string();
    let domain = if provider == "codebuddy-intl" { "www.codebuddy.ai" } else { "copilot.tencent.com" };
    let label = if provider == "codebuddy-intl" { "CodeBuddy intl" } else { "CodeBuddy" };

    dedup_refresh(provider, Some(refresh_token), || async {
        let mut headers = accept_json(HeaderList::new());
        headers.insert("X-Requested-With".into(), "XMLHttpRequest".into());
        headers.insert("X-Domain".into(), domain.into());
        headers.insert("X-Refresh-Token".into(), refresh_token.into());
        headers.insert("X-Auth-Refresh-Source".into(), "plugin".into());
        headers.insert("X-Product".into(), "SaaS".into());
        if !user_agent.is_empty() {
            headers.insert("User-Agent".into(), user_agent.clone());
        }

        let response = match post_raw(&url, headers, "{}".to_string(), None).await {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Failed to refresh {label} token: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            log.error("TOKEN_REFRESH", &format!("Failed to refresh {label} token: {error_text}"));
            return None;
        }

        let data = match response.json_body().await {
            Ok(Json::Object(m)) => m,
            Ok(_) => return None,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Failed to parse {label} token response: {e}"));
                return None;
            }
        };
        // `data.code !== 0 || !data.data?.accessToken` → the sign-in is dead.
        let code_ok = data.get("code").map_or(true, |c| c.as_i64() == Some(0));
        let token_data = data.get("data").and_then(Json::as_object);
        let access_token = token_data
            .and_then(|d| d.get("accessToken"))
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty());
        let (token_data, access_token) = match (code_ok, token_data, access_token) {
            (true, Some(d), Some(t)) => (d, t),
            _ => {
                log.error("TOKEN_REFRESH", &format!(
                    "{label} token refresh returned no token code={:?} msg={:?}",
                    data.get("code"),
                    data.get("msg").and_then(Json::as_str),
                ));
                return None;
            }
        };

        log.info("TOKEN_REFRESH", &format!(
            "Successfully refreshed {label} token hasNewAccessToken=true hasNewRefreshToken={} expiresIn={:?}",
            token_data.get("refreshToken").and_then(Json::as_str).is_some(),
            token_data.get("expiresIn").and_then(Json::as_i64),
        ));
        let mut result = JsonMap::new();
        result.insert("accessToken".to_string(), json!(access_token));
        let rt = token_data.get("refreshToken").and_then(Json::as_str).unwrap_or(refresh_token);
        result.insert("refreshToken".to_string(), json!(rt));
        if let Some(ei) = token_data.get("expiresIn") {
            result.insert("expiresIn".to_string(), ei.clone());
        }
        Some(result)
    }, log).await
}

/// JS `refreshCodebuddyToken(refreshToken, log)`.
pub async fn refresh_codebuddy_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    refresh_codebuddy_variant("codebuddy-cn", refresh_token, log).await
}

/// JS `refreshCodebuddyIntlToken(refreshToken, log)`.
pub async fn refresh_codebuddy_intl_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    refresh_codebuddy_variant("codebuddy-intl", refresh_token, log).await
}

/// `GITHUB_COPILOT.<key>` from the appConstants dump.
fn github_copilot_const(key: &str) -> String {
    nr_data::export("open-sse/config/appConstants", "GITHUB_COPILOT")
        .and_then(|c| c.get(key))
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// JS `refreshCopilotToken(githubAccessToken, log)`: swap a GitHub OAuth token for
/// a short-lived Copilot bearer token.
pub async fn refresh_copilot_token(github_access_token: &str, log: &Logger) -> Option<JsonMap> {
    if github_access_token.is_empty() {
        return None;
    }

    let url = endpoint_for("github", "copilotTokenUrl", "https://api.github.com/copilot_internal/v2/token");

    dedup_refresh("copilot", Some(github_access_token), || async {
        let mut headers = accept_json(HeaderList::new());
        headers.insert("Authorization".into(), format!("token {github_access_token}"));
        headers.insert("User-Agent".into(), github_copilot_const("USER_AGENT"));
        headers.insert(
            "Editor-Version".into(),
            format!("vscode/{}", github_copilot_const("VSCODE_VERSION")),
        );
        headers.insert(
            "Editor-Plugin-Version".into(),
            format!("copilot-chat/{}", github_copilot_const("COPILOT_CHAT_VERSION")),
        );
        headers.insert("x-github-api-version".into(), github_copilot_const("API_VERSION"));

        let response = match get(&url, headers).await {
            Ok(r) => r,
            Err(e) => {
                log.error("TOKEN_REFRESH", &format!("Error refreshing Copilot token: {e}"));
                return None;
            }
        };

        if !response.ok() {
            let error_text = response.text().await.unwrap_or_default();
            log.error("TOKEN_REFRESH", &format!("Failed to refresh Copilot token: {error_text}"));
            return None;
        }

        let data = response.json_body().await.ok()?;
        let token = data.get("token").and_then(Json::as_str)?;
        log.info("TOKEN_REFRESH", &format!(
            "Successfully refreshed Copilot token hasToken={} expiresAt={:?}",
            !token.is_empty(),
            data.get("expires_at").and_then(Json::as_i64),
        ));
        let mut result = JsonMap::new();
        result.insert("token".to_string(), json!(token));
        if let Some(expires_at) = data.get("expires_at") {
            result.insert("expiresAt".to_string(), expires_at.clone());
        }
        Some(result)
    }, log).await
}

/// JS `classifyOAuthRefreshError(errorText, status)`: decide whether a failed
/// refresh means the sign-in is permanently gone (re-auth required).
pub struct OAuthRefreshFailure {
    pub status: u16,
    pub code: String,
    pub description: String,
    pub permanent: bool,
}

pub fn classify_oauth_refresh_error(error_text: &str, status: u16) -> OAuthRefreshFailure {
    let parsed: Option<Json> = if error_text.is_empty() {
        None
    } else {
        serde_json::from_str(error_text).ok()
    };
    let pick = |key: &str| parsed.as_ref().and_then(|p| p.get(key));
    let code = pick("error")
        .and_then(|e| e.get("code").or(Some(e)))
        .or_else(|| pick("error_code"))
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let description = pick("error_description")
        .or_else(|| pick("message"))
        .and_then(Json::as_str)
        .unwrap_or(error_text)
        .to_string();
    let combined = format!("{code} {description}").to_lowercase();
    let permanent = [
        "refresh_token_expired",
        "refresh_token_reused",
        "refresh_token_invalidated",
        "invalid_grant",
    ]
    .iter()
    .any(|marker| combined.contains(marker));

    OAuthRefreshFailure { status, code, description, permanent }
}

/// JS `refreshKimiToken(refreshToken, credentials, log)` — delegate to
/// refreshAccessToken("kimi", …); the kimi profile carries the X-Msh-* headers
/// and omits client_secret (CLIProxyAPI DeviceFlowClient parity).
pub async fn refresh_kimi_token(
    refresh_token: &str,
    credentials: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    refresh_access_token("kimi", refresh_token, credentials, log).await
}

/// JS `refreshTokenByProvider(provider, credentials, log)` — REFRESH_HANDLERS dispatch.
pub async fn refresh_token_by_provider(
    provider: &str,
    creds: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    let refresh_token = creds.refresh_token()?;

    match provider {
        // Google OAuth: shared client credentials per provider from config.
        "gemini-cli" | "antigravity" => {
            let client_id = config_str(provider, "clientId")?;
            let client_secret = config_str(provider, "clientSecret").unwrap_or_default();
            refresh_google_token(refresh_token, &client_id, &client_secret, log).await
        }
        "claude" => refresh_claude_oauth_token(refresh_token, log).await,
        "codex" => refresh_codex_token(refresh_token, log).await,
        "iflow" => refresh_iflow_token(refresh_token, log).await,
        "github" => refresh_github_token(refresh_token, log).await,
        "kiro" => refresh_kiro_token(refresh_token, creds.psd(), log, None).await,
        // Grok CLI shares the xAI OAuth client + token endpoint; device-code
        // tokens refresh the same way. `gcli` is a legacy alias.
        "xai" | "grok-cli" | "gcli" => refresh_xai_token(refresh_token, log).await,
        "codebuddy-cn" => refresh_codebuddy_token(refresh_token, log).await,
        "codebuddy-intl" => refresh_codebuddy_intl_token(refresh_token, log).await,
        "trae" => refresh_trae_token(refresh_token, creds, log).await,
        // ClinePass shares Cline's WorkOS auth endpoints, so the same refresh works.
        "cline" | "clinepass" => refresh_cline_token(refresh_token, log).await,
        // Zed/Windsurf hold long-lived tokens with no refresh_token flow.
        "zed" => refresh_zed_token(),
        "windsurf" => refresh_windsurf_token(log).await,
        // Kimi Code OAuth (merged into id `kimi`); legacy id still routes here.
        "kimi" | "kimi-coding" => refresh_kimi_token(refresh_token, creds, log).await,
        // MiniMax Code — single-use refresh tokens; the handler dedupes by token
        // value and classifies invalid_grant (docs/minimax-code-proxy-plan.md §2.3).
        "minimax-code" | "minimax-code-global" => {
            refresh_minimax_code_token(provider, refresh_token, log).await
        }
        "nous" => refresh_nous_token(refresh_token, log).await,
        // Vertex signs a JWT from the service-account JSON stored in apiKey.
        "vertex" | "vertex-partner" => {
            let sa_json = parse_vertex_sa_json(creds.api_key()?)?;
            refresh_vertex_token(&sa_json, log).await
        }
        _ => refresh_access_token(provider, refresh_token, creds, log).await,
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
        log.warn("TOKEN_REFRESH", &format!("No valid refresh token available for provider: {provider}"));
        return None;
    }
    refresh_token_by_provider(provider, creds, log).await
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


// ─── Provider refresh tests ─────────────────────────────────────────────────
// Every provider refresher is driven against a local axum mock: the test
// registers the mock URL in `TEST_ENDPOINTS`, calls the refresher, and asserts
// both the parsed result and the exact request the provider would have seen
// (method, path, headers, body). No live provider calls.
#[cfg(test)]
mod provider_tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use base64::Engine;
    use nr_core::{Credentials, Logger};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// What a mock endpoint received.
    #[derive(Clone, Debug, Default)]
    struct Seen {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: String,
    }

    impl Seen {
        fn json(&self) -> Json {
            serde_json::from_str(&self.body).unwrap_or(Json::Null)
        }
        fn form(&self) -> HashMap<String, String> {
            url::form_urlencoded::parse(self.body.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        }
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// Start a mock that replies with `status` + `body` to every request.
    async fn mock(status: u16, body: &'static str) -> (String, Log) {
        async fn handler(
            State((status, body, log)): State<(u16, String, Log)>,
            req: axum::extract::Request,
        ) -> impl axum::response::IntoResponse {
            use axum::body::to_bytes;
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let bytes = to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            log.lock().unwrap().push(Seen {
                method,
                path,
                headers,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
            (
                StatusCode::from_u16(status).unwrap(),
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
        }

        use axum::{routing::any, Router};
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().fallback(any(handler)).with_state((status, body.to_string(), log.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), log)
    }

    /// Register endpoint overrides for the duration of one test.
    /// Callers must hold `TEST_LOCK` (see `run`): the table is process-global and
    /// `clear()` would otherwise drop another test's overrides mid-request.
    fn endpoints(pairs: &[(&str, &str)]) {
        let mut map = TEST_ENDPOINTS.lock();
        let entry = map.get_or_insert_with(HashMap::new);
        entry.clear();
        for (key, url) in pairs {
            entry.insert((*key).to_string(), (*url).to_string());
        }
    }

    /// Serializes the endpoint-override table: it is process-global, so two
    /// tests registering different mocks at once would send each other's
    /// requests to a dead/wrong server (or to a real provider).
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Run an async test body on a fresh runtime, clearing overrides afterwards.
    fn run<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _serial = TEST_LOCK.lock();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(f());
        *TEST_ENDPOINTS.lock() = None;
        // Dedup cache is process-global; each test uses unique tokens, but clear
        // it so a retried test never reuses a stale result.
        REFRESH_DEDUP_CACHE.write().clear();
    }

    fn log() -> Logger {
        Logger::default()
    }

    fn only(log: &Log) -> Seen {
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "expected exactly one request, got {:?}", *entries);
        entries[0].clone()
    }

    // ── claude ──────────────────────────────────────────────────────────────
    #[test]
    fn claude_posts_json_with_client_id_only() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"cl-acc","refresh_token":"cl-rot","expires_in":3600}"#).await;
            endpoints(&[("claude:refreshUrl", &url), ("claude:tokenUrl", &url)]);

            let out = refresh_claude_oauth_token("cl-old", &log()).await.expect("claude refresh");
            assert_eq!(out["accessToken"], json!("cl-acc"));
            assert_eq!(out["refreshToken"], json!("cl-rot"));
            assert_eq!(out["expiresIn"], json!(3600));

            let req = only(&seen);
            assert_eq!(req.method, "POST");
            assert_eq!(req.headers["content-type"], "application/json");
            assert_eq!(req.headers["accept"], "application/json");
            let body = req.json();
            assert_eq!(body["grant_type"], json!("refresh_token"));
            assert_eq!(body["refresh_token"], json!("cl-old"));
            assert_eq!(body["client_id"], json!("9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
            assert!(body.get("client_secret").is_none(), "claude is a public client");
        });
    }

    // ── codex ───────────────────────────────────────────────────────────────
    #[test]
    fn codex_rotates_refresh_and_id_token() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"cx-acc","refresh_token":"cx-rot","id_token":"cx-id","expires_in":3600}"#).await;
            endpoints(&[("codex:tokenUrl", &url)]);

            let out = refresh_codex_token("cx-old", &log()).await.expect("codex refresh");
            assert_eq!(out["accessToken"], json!("cx-acc"));
            assert_eq!(out["refreshToken"], json!("cx-rot"));
            assert_eq!(out["idToken"], json!("cx-id"));
            assert_eq!(out["expiresIn"], json!(3600));

            let req = only(&seen);
            assert_eq!(req.headers["content-type"], "application/json");
            let body = req.json();
            assert_eq!(body["client_id"], json!("app_EMoamEEZ73f0CkXaXp7hrann"));
            assert_eq!(body["grant_type"], json!("refresh_token"));
            assert_eq!(body["refresh_token"], json!("cx-old"));
        });
    }

    #[test]
    fn codex_keeps_old_refresh_token_when_omitted() {
        run(|| async {
            let (url, _) = mock(200, r#"{"access_token":"cx-acc","expires_in":60}"#).await;
            endpoints(&[("codex:tokenUrl", &url)]);
            let out = refresh_codex_token("keep-me", &log()).await.unwrap();
            assert_eq!(out["refreshToken"], json!("keep-me"));
        });
    }

    #[test]
    fn codex_classifies_permanent_failure() {
        run(|| async {
            let (url, _) = mock(400, r#"{"error":{"code":"refresh_token_expired"}}"#).await;
            endpoints(&[("codex:tokenUrl", &url)]);
            let out = refresh_codex_token("dead", &log()).await.expect("returns an error marker");
            assert_eq!(out["error"], json!("unrecoverable_refresh_error"));
            assert_eq!(out["code"], json!("refresh_token_expired"));
            assert!(is_unrecoverable_refresh_error(&Some(out)));
        });
    }

    #[test]
    fn codex_returns_none_on_transient_failure() {
        run(|| async {
            let (url, _) = mock(503, "upstream unavailable").await;
            endpoints(&[("codex:tokenUrl", &url)]);
            assert!(refresh_codex_token("ok-token", &log()).await.is_none());
        });
    }

    // ── iflow ───────────────────────────────────────────────────────────────
    #[test]
    fn iflow_sends_basic_auth_and_form_body() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"if-acc","refresh_token":"if-rot","expires_in":3600}"#).await;
            endpoints(&[("iflow:refreshUrl", &url), ("iflow:tokenUrl", &url)]);

            let out = refresh_iflow_token("if-old", &log()).await.expect("iflow refresh");
            assert_eq!(out["accessToken"], json!("if-acc"));

            let req = only(&seen);
            assert_eq!(req.headers["content-type"], "application/x-www-form-urlencoded");
            let auth = req.headers["authorization"].clone();
            assert!(auth.starts_with("Basic "), "iflow uses Basic auth: {auth}");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth.trim_start_matches("Basic "))
                .unwrap();
            assert_eq!(
                String::from_utf8(decoded).unwrap(),
                "10009311001:4Z3YjXycVsQvyGF1etiNlIBB4RsqSDtW"
            );
            let form = req.form();
            assert_eq!(form["client_id"], "10009311001");
            assert_eq!(form["client_secret"], "4Z3YjXycVsQvyGF1etiNlIBB4RsqSDtW");
            assert_eq!(form["refresh_token"], "if-old");
        });
    }

    // ── github ──────────────────────────────────────────────────────────────
    #[test]
    fn github_omits_client_secret_and_uses_refresh_token_expiry() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"gh-acc","refresh_token":"gh-rot","refresh_token_expires_in":28800}"#).await;
            endpoints(&[("github:refreshUrl", &url), ("github:tokenUrl", &url)]);

            let out = refresh_github_token("gh-old", &log()).await.expect("github refresh");
            assert_eq!(out["accessToken"], json!("gh-acc"));
            assert_eq!(out["refreshToken"], json!("gh-rot"));
            assert_eq!(out["expiresIn"], json!(28800));

            let req = only(&seen);
            let form = req.form();
            assert_eq!(form["client_id"], "Iv1.b507a08c87ecfe98");
            assert!(!form.contains_key("client_secret"), "github-cli has no secret");
        });
    }

    // ── google / gemini / gemini-cli / antigravity ──────────────────────────
    #[test]
    fn google_refresh_posts_client_credentials() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"g-acc","refresh_token":"g-rot","expires_in":3599}"#).await;
            endpoints(&[("google:token", &url)]);

            let out = refresh_google_token("g-old", "cid", "csecret", &log()).await.unwrap();
            assert_eq!(out["accessToken"], json!("g-acc"));
            assert_eq!(out["refreshToken"], json!("g-rot"));
            assert_eq!(out["expiresIn"], json!(3599));

            let req = only(&seen);
            assert_eq!(req.headers["content-type"], "application/x-www-form-urlencoded");
            let form = req.form();
            assert_eq!(form["grant_type"], "refresh_token");
            assert_eq!(form["refresh_token"], "g-old");
            assert_eq!(form["client_id"], "cid");
            assert_eq!(form["client_secret"], "csecret");
        });
    }

    #[test]
    fn gemini_cli_uses_provider_client_credentials() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"gc-acc","expires_in":3599}"#).await;
            endpoints(&[("google:token", &url)]);

            let mut creds = Credentials::new();
            creds.set("refreshToken", json!("gc-old"));
            let out = get_access_token("gemini-cli", &creds, &log()).await.unwrap();
            assert_eq!(out["accessToken"], json!("gc-acc"));

            let form = only(&seen).form();
            assert_eq!(form["client_id"], "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com");
            assert_eq!(form["client_secret"], "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");
        });
    }

    #[test]
    fn antigravity_uses_its_own_client_id() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"ag-acc","expires_in":3599}"#).await;
            endpoints(&[("google:token", &url)]);

            let mut creds = Credentials::new();
            creds.set("refreshToken", json!("ag-old"));
            get_access_token("antigravity", &creds, &log()).await.unwrap();

            let form = only(&seen).form();
            assert_eq!(form["client_id"], "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com");
        });
    }

    // ── kimi ────────────────────────────────────────────────────────────────
    #[test]
    fn kimi_sends_msh_device_headers() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"km-acc","expires_in":86400}"#).await;
            endpoints(&[("kimi:refreshUrl", &url), ("kimi:tokenUrl", &url)]);

            let mut creds = Credentials::new();
            creds.set("refreshToken", json!("km-old"));
            creds.set("providerSpecificData", json!({ "deviceId": "dev-xyz" }));

            let out = refresh_kimi_token("km-old", &creds, &log()).await.expect("kimi refresh");
            assert_eq!(out["accessToken"], json!("km-acc"));
            assert_eq!(out["refreshToken"], json!("km-old"), "keep the sent token");

            let req = only(&seen);
            assert_eq!(req.headers["x-msh-platform"], "9router");
            assert_eq!(req.headers["x-msh-device-id"], "dev-xyz");
            let form = req.form();
            assert_eq!(form["client_id"], "17e5f671-d194-4dfb-9706-5516cb48c098");
            assert!(!form.contains_key("client_secret"), "kimi is a public PKCE client");
        });
    }

    // ── cline / clinepass ───────────────────────────────────────────────────
    #[test]
    fn cline_uses_extension_json_contract() {
        run(|| async {
            let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(3600))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let body: &'static str = Box::leak(
                json!({ "data": { "accessToken": "cline-acc", "refreshToken": "cline-rot", "expiresAt": expires_at } })
                    .to_string()
                    .into_boxed_str(),
            );
            let (url, seen) = mock(200, body).await;
            endpoints(&[("cline:refreshUrl", &url)]);

            let out = refresh_cline_token("cline-old", &log()).await.expect("cline refresh");
            assert_eq!(out["accessToken"], json!("cline-acc"));
            assert_eq!(out["refreshToken"], json!("cline-rot"));
            let exp = out["expiresIn"].as_i64().unwrap();
            assert!((3500..=3600).contains(&exp), "expiresIn from expiresAt: {exp}");

            let req = only(&seen);
            assert_eq!(req.headers["content-type"], "application/json");
            assert_eq!(
                req.json(),
                json!({"refreshToken": "cline-old", "grantType": "refresh_token", "clientType": "extension"})
            );
        });
    }

    #[test]
    fn cline_defaults_to_one_hour_without_expiry_fields() {
        run(|| async {
            let (url, _) = mock(200, r#"{"accessToken":"c2"}"#).await;
            endpoints(&[("cline:refreshUrl", &url)]);
            let out = refresh_cline_token("c2-old", &log()).await.unwrap();
            assert_eq!(out["expiresIn"], json!(3600));
            assert_eq!(out["refreshToken"], json!("c2-old"));
        });
    }

    // ── copilot ─────────────────────────────────────────────────────────────
    #[test]
    fn copilot_swaps_github_token_for_bearer() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"token":"copilot-tok","expires_at":1790000000}"#).await;
            endpoints(&[("github:copilotTokenUrl", &url)]);

            let out = refresh_copilot_token("gho_user", &log()).await.expect("copilot refresh");
            assert_eq!(out["token"], json!("copilot-tok"));
            assert_eq!(out["expiresAt"], json!(1790000000));

            let req = only(&seen);
            assert_eq!(req.method, "GET");
            assert_eq!(req.headers["authorization"], "token gho_user");
            assert_eq!(req.headers["user-agent"], "GitHubCopilotChat/0.38.0");
            assert_eq!(req.headers["editor-version"], "vscode/1.110.0");
            assert_eq!(req.headers["editor-plugin-version"], "copilot-chat/0.38.0");
            assert_eq!(req.headers["x-github-api-version"], "2025-04-01");
        });
    }

    // ── codebuddy cn / intl ─────────────────────────────────────────────────
    #[test]
    fn codebuddy_cn_uses_refresh_token_header() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"code":0,"data":{"accessToken":"cb-acc","refreshToken":"cb-rot","expiresIn":7200}}"#).await;
            endpoints(&[("codebuddy-cn:refreshUrl", &url)]);

            let out = refresh_codebuddy_token("cb-old", &log()).await.expect("codebuddy refresh");
            assert_eq!(out["accessToken"], json!("cb-acc"));
            assert_eq!(out["refreshToken"], json!("cb-rot"));
            assert_eq!(out["expiresIn"], json!(7200));

            let req = only(&seen);
            assert_eq!(req.headers["x-refresh-token"], "cb-old");
            assert_eq!(req.headers["x-domain"], "copilot.tencent.com");
            assert_eq!(req.headers["x-product"], "SaaS");
            assert_eq!(req.headers["x-auth-refresh-source"], "plugin");
            assert_eq!(req.body, "{}");
        });
    }

    #[test]
    fn codebuddy_intl_targets_its_own_domain() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"code":0,"data":{"accessToken":"cbi-acc"}}"#).await;
            endpoints(&[("codebuddy-intl:refreshUrl", &url)]);
            let out = refresh_codebuddy_intl_token("cbi-old", &log()).await.unwrap();
            assert_eq!(out["accessToken"], json!("cbi-acc"));
            assert_eq!(out["refreshToken"], json!("cbi-old"));
            let req = only(&seen);
            assert_eq!(req.headers["x-domain"], "www.codebuddy.ai");
        });
    }

    #[test]
    fn codebuddy_rejects_nonzero_code() {
        run(|| async {
            let (url, _) = mock(200, r#"{"code":10004,"msg":"token expired"}"#).await;
            endpoints(&[("codebuddy-cn:refreshUrl", &url)]);
            assert!(refresh_codebuddy_token("dead", &log()).await.is_none());
        });
    }

    // ── trae ────────────────────────────────────────────────────────────────
    #[test]
    fn trae_unwraps_result_and_computes_expiry() {
        run(|| async {
            let expires_at = chrono::Utc::now().timestamp() + 7200;
            let body: &'static str = Box::leak(
                json!({ "Result": {
                    "AccessToken": "tr-acc",
                    "RefreshToken": "tr-rot",
                    "TokenType": "Bearer",
                    "ExpiresAt": expires_at,
                }})
                .to_string()
                .into_boxed_str(),
            );
            let (url, seen) = mock(200, body).await;
            endpoints(&[("trae:exchangeTokenUrl", &url)]);

            let out = refresh_trae_token("tr-old", &Credentials::new(), &log()).await.expect("trae refresh");
            assert_eq!(out["accessToken"], json!("tr-acc"));
            assert_eq!(out["refreshToken"], json!("tr-rot"));
            let exp = out["expiresIn"].as_i64().unwrap();
            assert!((7100..=7200).contains(&exp), "expiresIn from epoch ExpiresAt: {exp}");

            let req = only(&seen);
            assert_eq!(req.headers["user-agent"], "Trae/1.0.0 antigravity-cockpit-tools");
            assert_eq!(
                req.json(),
                json!({
                    "ClientID": "ono9krqynydwx5",
                    "RefreshToken": "tr-old",
                    "ClientSecret": "-",
                    "UserID": "",
                })
            );
        });
    }

    #[test]
    fn trae_returns_none_without_access_token() {
        run(|| async {
            let (url, _) = mock(200, r#"{"Result":{"RefreshToken":"x"}}"#).await;
            endpoints(&[("trae:exchangeTokenUrl", &url)]);
            assert!(refresh_trae_token("old", &Credentials::new(), &log()).await.is_none());
        });
    }

    // ── xai ─────────────────────────────────────────────────────────────────
    #[test]
    fn xai_posts_form_with_client_id() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"xa-acc","refresh_token":"xa-rot","expires_in":900,"id_token":"xa-id"}"#).await;
            endpoints(&[("xai:refreshUrl", &url)]);

            let out = refresh_xai_token("xa-old", &log()).await.expect("xai refresh");
            assert_eq!(
                out,
                JsonMap::from_iter([
                    ("accessToken".to_string(), json!("xa-acc")),
                    ("refreshToken".to_string(), json!("xa-rot")),
                    ("expiresIn".to_string(), json!(900)),
                    ("idToken".to_string(), json!("xa-id")),
                ])
            );
            assert!(out.get("expiresAt").is_none(), "JS never sets expiresAt here");

            let req = only(&seen);
            assert_eq!(req.headers["content-type"], "application/x-www-form-urlencoded");
            let form = req.form();
            assert_eq!(form["grant_type"], "refresh_token");
            assert_eq!(form["client_id"], "b1a00492-073a-47ea-816f-4c329264a828");
            assert_eq!(form["refresh_token"], "xa-old");
        });
    }

    #[test]
    fn xai_reports_invalid_grant() {
        run(|| async {
            let (url, _) = mock(400, r#"{"error":"invalid_grant"}"#).await;
            endpoints(&[("xai:refreshUrl", &url)]);
            let out = refresh_xai_token("spent", &log()).await.expect("error marker");
            assert_eq!(out["error"], json!("invalid_grant"));
        });
    }

    // ── nous ────────────────────────────────────────────────────────────────
    #[test]
    fn nous_mints_agent_key_after_refresh() {
        run(|| async {
            // One server, two paths: token then agent-key.
            use axum::{routing::any, Router};
            let seen: Log = Arc::new(Mutex::new(Vec::new()));
            let log2 = seen.clone();
            async fn handler(State(log): State<Log>, req: axum::extract::Request) -> impl axum::response::IntoResponse {
                use axum::body::to_bytes;
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let headers = req
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let bytes = to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                log.lock().unwrap().push(Seen {
                    method,
                    path: path.clone(),
                    headers,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                });
                let body = if path.ends_with("agent-key") {
                    r#"{"api_key":"agent-key-1","key_id":"kid-1","expires_at":"2030-01-01T00:00:00Z"}"#
                } else {
                    r#"{"access_token":"nous-acc","refresh_token":"nous-rot","expires_in":3600}"#
                };
                ([(axum::http::header::CONTENT_TYPE, "application/json")], body)
            }
            let app = Router::new().fallback(any(handler)).with_state(log2);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let base = format!("http://{addr}");

            endpoints(&[
                ("nous:tokenUrl", &format!("{base}/api/oauth/token")),
                ("nous:agentKeyUrl", &format!("{base}/api/oauth/agent-key")),
            ]);

            let out = refresh_nous_token("nous-old", &log()).await.expect("nous refresh");
            assert_eq!(out["accessToken"], json!("nous-acc"));
            assert_eq!(out["apiKey"], json!("agent-key-1"));
            assert_eq!(out["agentKey"], json!("agent-key-1"));
            assert_eq!(out["providerSpecificData"]["agentKeyId"], json!("kid-1"));
            assert_eq!(out["providerSpecificData"]["agentKeyExpiresAt"], json!("2030-01-01T00:00:00Z"));

            let entries = seen.lock().unwrap().clone();
            assert_eq!(entries.len(), 2, "token + agent key");
            assert_eq!(entries[0].headers["x-nous-refresh-token"], "nous-old");
            assert_eq!(entries[0].form()["client_id"], "hermes-cli");
            assert_eq!(entries[1].headers["authorization"], "Bearer nous-acc");
            assert_eq!(entries[1].json(), json!({"min_ttl_seconds": 7200}));
        });
    }

    #[test]
    fn nous_falls_back_to_access_token_when_mint_fails() {
        run(|| async {
            use axum::{routing::any, Router};
            async fn handler(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
                if uri.path().ends_with("agent-key") {
                    (StatusCode::INTERNAL_SERVER_ERROR, "nope")
                } else {
                    (StatusCode::OK, r#"{"access_token":"nous-acc","refresh_token":"nous-rot"}"#)
                }
            }
            let app = Router::new().fallback(any(handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let base = format!("http://{addr}");
            endpoints(&[
                ("nous:tokenUrl", &format!("{base}/api/oauth/token")),
                ("nous:agentKeyUrl", &format!("{base}/api/oauth/agent-key")),
            ]);

            let out = refresh_nous_token("nous-old", &log()).await.expect("nous refresh");
            assert_eq!(out["apiKey"], json!("nous-acc"), "falls back to the access token");
            assert_eq!(out["agentKey"], Json::Null, "JS emits null agentKey");
        });
    }

    // ── minimax-code ────────────────────────────────────────────────────────
    #[test]
    fn minimax_rotates_and_reports_invalid_grant() {
        run(|| async {
            let (url, seen) = mock(200, r#"{"access_token":"mm-acc","refresh_token":"mm-rot","expires_in":1800}"#).await;
            endpoints(&[("minimax-code:tokenUrl", &url)]);

            let out = refresh_minimax_code_token("minimax-code", "mm-old", &log()).await.unwrap();
            assert_eq!(out["accessToken"], json!("mm-acc"));
            assert_eq!(out["refreshToken"], json!("mm-rot"));
            assert_eq!(out["expiresIn"], json!(1800));

            let form = only(&seen).form();
            assert_eq!(form["client_id"], "mcode-public");
            assert_eq!(form["scope"], "agent.default");
            assert_eq!(form["audience"], "agent-backend");
            assert_eq!(form["refresh_token"], "mm-old");
        });
    }

    #[test]
    fn minimax_rejects_single_use_spent_token() {
        run(|| async {
            let (url, _) = mock(400, r#"{"error":"invalid_grant"}"#).await;
            endpoints(&[("minimax-code:tokenUrl", &url)]);
            let out = refresh_minimax_code_token("minimax-code", "spent", &log()).await.unwrap();
            assert_eq!(out["error"], json!("invalid_grant"));
        });
    }

    #[test]
    fn minimax_unknown_variant_is_none() {
        run(|| async {
            assert!(refresh_minimax_code_token("minimax-code-eu", "x", &log()).await.is_none());
        });
    }


}
