//! Port of open-sse/services/qoderModels.js — Qoder model catalog fetcher.
//!
//! Calls `/algo/api/v2/model/list` (COSY-signed) on the inference host to get
//! the live catalog for an authenticated Qoder account, then caches the
//! per-model `model_config` blocks by key. Chat requests later look up the
//! exact server-published metadata for the model they want — Qoder's chat
//! endpoint silently downgrades to a different model when the wrong
//! `model_config` is sent.
//!
//! PAT (`pt-...`) connections: a PAT cannot sign COSY requests directly, so we
//! exchange it for a short-lived job token (`jt-...`) via the region's
//! jobToken/exchange endpoint (plain JSON POST), then use that job token for
//! signing. On intl, job-token traffic must hit api2.qoder.sh — api3 rejects
//! `jt-` with "Login expired" (403); CN serves it from the same gateway host.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, ProxyOptions};
use nr_core::{Credentials, Error, Json, JsonMap, Logger, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::constants::{
    qoder_inference_base, qoder_job_token_exchange_url, qoder_region_of, qoder_user_info_url,
    QODER_CLIENT_TYPE, QODER_IDE_VERSION,
};
use super::cosy::{build_cosy_headers, derive_machine_id, CosyCreds};

const FETCH_TIMEOUT_MS: u64 = 15_000;
const CACHE_TTL_MS: u64 = 60 * 60 * 1000; // 1h, same as the Kiro catalog
const PAT_PREFIX: &str = "pt-";
// A job token is short-lived (24h), so we keep it per PAT and re-exchange once
// it is within 5 minutes of expiry.
const PAT_REFRESH_BUFFER_MS: u64 = 5 * 60 * 1000;
const PAT_DEFAULT_TTL_MS: u64 = 24 * 60 * 60 * 1000;

struct PatEntry {
    access_token: String,
    user_id: String,
    expires_at_ms: u64,
}

struct CatalogEntry {
    expires_at_ms: u64,
    models: Vec<Json>,
    raw_configs: JsonMap,
}

static PAT_CACHE: LazyLock<Mutex<HashMap<String, PatEntry>>> = LazyLock::new(Default::default);
static CATALOG_CACHE: LazyLock<Mutex<HashMap<String, CatalogEntry>>> = LazyLock::new(Default::default);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `isQoderPat(token)` — `pt-` prefixed personal access token.
pub fn is_qoder_pat(token: Option<&str>) -> bool {
    token.map(|t| t.starts_with(PAT_PREFIX)).unwrap_or(false)
}

fn parse_expiry(value: Option<&Json>) -> Option<u64> {
    match value? {
        Json::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp_millis().max(0) as u64)
            .or_else(|| {
                // `Date.parse` also accepts `YYYY-MM-DD HH:MM:SS` shapes.
                chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|d| d.and_utc().timestamp_millis().max(0) as u64)
            }),
        Json::Number(n) => n.as_u64().map(|ms| if ms < 10_000_000_000 { ms * 1000 } else { ms }),
        _ => None,
    }
}

/// Exchange a Qoder PAT for a short-lived job token (plain JSON POST, not
/// COSY-signed).
async fn exchange_job_token(
    pat: &str,
    proxy: Option<&ProxyOptions>,
    region: &str,
) -> Result<(String, u64)> {
    let mut headers = HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Accept".into(), "application/json".into());
    headers.insert("User-Agent".into(), "qodercli/1.0.0".into());
    headers.insert("Cosy-Version".into(), QODER_IDE_VERSION.into());
    headers.insert("Cosy-ClientType".into(), QODER_CLIENT_TYPE.into());

    let body = json!({ "personal_token": pat });
    let mut req = FetchRequest::post_json(qoder_job_token_exchange_url(region), headers, &body);
    req.header_timeout = Some(Duration::from_millis(FETCH_TIMEOUT_MS));

    let response = proxy_fetch(req, proxy).await?;
    if !response.ok() {
        let status = response.status;
        let text = response.text().await.unwrap_or_default();
        let trimmed: String = text.chars().take(200).collect();
        return Err(Error::Provider(format!("qoder PAT exchange failed: {status} {trimmed}")));
    }
    let data = response.json_body().await.unwrap_or(Json::Null);
    let Some(token) = data.get("token").and_then(Json::as_str).filter(|s| !s.is_empty()) else {
        return Err(Error::Provider("qoder PAT exchange returned no job token".into()));
    };

    let mut expires_at = now_ms() + PAT_DEFAULT_TTL_MS;
    if let Some(parsed) = parse_expiry(data.get("expires_at")) {
        expires_at = parsed;
    } else if let Some(secs) = data.get("expires_in").and_then(Json::as_u64).filter(|v| *v > 0) {
        expires_at = now_ms() + secs * 1000;
    }
    Ok((token.to_string(), expires_at))
}

/// Resolve the Qoder userId for a job token (needed for COSY signing).
/// Returns `""` on any failure — callers fall back to the stored userId.
async fn fetch_user_id_for_job_token(
    job_token: &str,
    proxy: Option<&ProxyOptions>,
    region: &str,
) -> String {
    let mut headers = HeaderList::new();
    headers.insert("Authorization".into(), format!("Bearer {job_token}"));
    headers.insert("Accept".into(), "application/json".into());
    headers.insert("User-Agent".into(), "qodercli/1.0.0".into());

    let mut req = FetchRequest::get(qoder_user_info_url(region));
    req.headers = headers;
    req.header_timeout = Some(Duration::from_millis(FETCH_TIMEOUT_MS));

    let Ok(response) = proxy_fetch(req, proxy).await else { return String::new() };
    if !response.ok() {
        return String::new();
    }
    let Ok(data) = response.json_body().await else { return String::new() };
    for key in ["id", "userId", "user_id"] {
        if let Some(v) = data.get(key).and_then(Json::as_str).filter(|s| !s.is_empty()) {
            return v.to_string();
        }
    }
    String::new()
}

/// Resolve a PAT to a job-token credential, cached per-PAT-per-region.
async fn resolve_pat_credential(
    pat: &str,
    proxy: Option<&ProxyOptions>,
    region: &str,
) -> Result<PatEntry> {
    let cache_key = format!("{region}:{pat}");
    if let Some(cached) = PAT_CACHE.lock().expect("pat cache").get(&cache_key) {
        if cached.expires_at_ms > now_ms() + PAT_REFRESH_BUFFER_MS {
            return Ok(PatEntry {
                access_token: cached.access_token.clone(),
                user_id: cached.user_id.clone(),
                expires_at_ms: cached.expires_at_ms,
            });
        }
    }

    let (job_token, expires_at) = exchange_job_token(pat, proxy, region).await?;
    let user_id = fetch_user_id_for_job_token(&job_token, proxy, region).await;
    let entry = PatEntry { access_token: job_token, user_id, expires_at_ms: expires_at };
    PAT_CACHE
        .lock()
        .expect("pat cache")
        .insert(cache_key, PatEntry { access_token: entry.access_token.clone(), user_id: entry.user_id.clone(), expires_at_ms: entry.expires_at_ms });
    Ok(entry)
}

/// `resolveQoderCredentials` — resolve connection credentials to COSY-signable
/// form: PATs are exchanged for a job token + userId, everything else passes
/// through unchanged.
pub async fn resolve_qoder_credentials(
    credentials: &Credentials,
    proxy: Option<&ProxyOptions>,
    region: Option<&str>,
) -> Result<Credentials> {
    let raw = credentials.api_key().or_else(|| credentials.access_token());
    if !is_qoder_pat(raw) {
        return Ok(credentials.clone());
    }
    let raw = raw.unwrap_or("");
    let eff_region = region
        .map(String::from)
        .unwrap_or_else(|| qoder_region_of(credentials.str("provider").unwrap_or("qoder")).to_string());
    let resolved = resolve_pat_credential(raw, proxy, &eff_region).await?;

    let mut out = credentials.clone();
    out.set("accessToken", Json::String(resolved.access_token));
    out.remove("apiKey");
    let fallback_user = credentials.psd_str("userId").unwrap_or("").to_string();
    let fallback_machine = credentials.psd_str("machineId").unwrap_or("").to_string();
    {
        let psd = out.psd_mut();
        psd.insert("authMethod".into(), Json::String("pat".into()));
        psd.insert(
            "userId".into(),
            Json::String(if resolved.user_id.is_empty() { fallback_user } else { resolved.user_id }),
        );
        psd.insert("machineId".into(), Json::String(fallback_machine));
    }
    Ok(out)
}

/// Stable cache key per credential+region.
fn cache_key(credentials: &Credentials) -> String {
    let seed = credentials
        .psd_str("userId")
        .or_else(|| credentials.refresh_token())
        .or_else(|| credentials.access_token())
        .unwrap_or("anonymous");
    let region = qoder_region_of(credentials.str("provider").unwrap_or("qoder"));
    format!("{:x}", Sha256::digest(format!("qoder:{region}:{seed}").as_bytes()))
}

fn cosy_creds(credentials: &Credentials) -> (String, String, String, String, Option<String>) {
    (
        credentials.psd_str("userId").unwrap_or("").to_string(),
        credentials.access_token().unwrap_or("").to_string(),
        credentials.str("displayName").unwrap_or("").to_string(),
        credentials.str("email").unwrap_or("").to_string(),
        credentials.psd_str("machineId").map(String::from),
    )
}

/// Sign a Qoder request with the connection's COSY credentials.
pub fn cosy_headers_for(body: &[u8], url: &str, credentials: &Credentials) -> Option<Vec<(String, String)>> {
    let (user_id, auth_token, name, email, machine_id) = cosy_creds(credentials);
    let derived = machine_id.filter(|m| !m.is_empty()).unwrap_or_else(|| derive_machine_id(&user_id));
    build_cosy_headers(
        body,
        url,
        &CosyCreds {
            user_id: &user_id,
            auth_token: &auth_token,
            name: &name,
            email: &email,
            machine_id: Some(&derived),
        },
    )
}

/// Raw catalog fetch: models (IDE-visible) plus every `model_config` block.
async fn fetch_catalog_raw(
    credentials: &Credentials,
    proxy: Option<&ProxyOptions>,
    region: &str,
) -> Result<Option<(Vec<Json>, JsonMap)>> {
    let (user_id, auth_token, _, _, _) = cosy_creds(credentials);
    if user_id.is_empty() || auth_token.is_empty() {
        return Ok(None);
    }

    // Intl job-token traffic is rejected by api3 ("Login expired" 403) — the
    // official qodercli serves it from api2 instead; CN uses the single gateway.
    let base = qoder_inference_base(&credentials.clone().into_value(), region);
    let model_list_url = format!("{base}/algo/api/v2/model/list");

    let Some(cosy) = cosy_headers_for(&[], &model_list_url, credentials) else {
        return Ok(None);
    };
    let mut headers = HeaderList::new();
    headers.insert("Accept".into(), "application/json".into());
    headers.insert("Accept-Encoding".into(), "identity".into());
    for (k, v) in cosy {
        headers.insert(k, v);
    }

    let mut req = FetchRequest::get(model_list_url);
    req.headers = headers;
    req.header_timeout = Some(Duration::from_millis(FETCH_TIMEOUT_MS));

    let response = match proxy_fetch(req, proxy).await {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    if !response.ok() {
        return Ok(None);
    }
    let body = match response.json_body().await {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let Some(entries) = body.get("chat").and_then(Json::as_array) else {
        return Ok(None);
    };

    let mut models = Vec::new();
    let mut raw_configs = JsonMap::new();
    for entry in entries {
        if !entry.is_object() {
            continue;
        }
        let Some(key) = entry.get("key").and_then(Json::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        // Always cache the config — chat needs model_config even for UI-hidden
        // models (enable:false). Upstream still accepts chat for these keys.
        raw_configs.insert(key.to_string(), entry.clone());
        if entry.get("enable") == Some(&Json::Bool(false)) {
            continue;
        }
        let display = entry.get("display_name").and_then(Json::as_str).unwrap_or(key);
        let ctx = entry.get("max_input_tokens").and_then(Json::as_u64).unwrap_or(131_072);
        models.push(json!({
            "id": key,
            "name": display,
            "contextLength": ctx,
            "isVL": entry.get("is_vl") == Some(&Json::Bool(true)),
            "isReasoning": entry.get("is_reasoning") == Some(&Json::Bool(true)),
            "maxOutputTokens": entry.get("max_output_tokens").and_then(Json::as_u64).unwrap_or(0),
            "description": entry.get("description").and_then(Json::as_str).unwrap_or(""),
        }));
    }
    Ok(Some((models, raw_configs)))
}

/// Live catalog for a credential (cached, deduped upstream).
pub struct Catalog {
    pub models: Vec<Json>,
    pub raw_configs: JsonMap,
}

/// `resolveQoderModels` — cached catalog, refreshing on a miss.
pub async fn resolve_qoder_models(
    credentials: &Credentials,
    proxy: Option<&ProxyOptions>,
    region: &str,
    force_refresh: bool,
    log: &Logger,
) -> Option<Catalog> {
    let resolved = match resolve_qoder_credentials(credentials, proxy, Some(region)).await {
        Ok(c) => c,
        Err(err) => {
            log.warn("QODER", format!("PAT exchange failed: {err}"));
            return None;
        }
    };
    if resolved.access_token().is_none() || resolved.psd_str("userId").is_none() {
        return None;
    }

    let key = cache_key(&resolved);
    if !force_refresh {
        if let Some(cached) = CATALOG_CACHE.lock().expect("catalog cache").get(&key) {
            if cached.expires_at_ms > now_ms() {
                return Some(Catalog { models: cached.models.clone(), raw_configs: cached.raw_configs.clone() });
            }
        }
    }

    let fetched = fetch_catalog_raw(&resolved, proxy, region).await.ok().flatten()?;
    let entry = CatalogEntry {
        expires_at_ms: now_ms() + CACHE_TTL_MS,
        models: fetched.0.clone(),
        raw_configs: fetched.1.clone(),
    };
    CATALOG_CACHE.lock().expect("catalog cache").insert(key, entry);
    Some(Catalog { models: fetched.0, raw_configs: fetched.1 })
}

/// `getQoderModelConfig` — the model_config block for one key, or `None` when
/// the catalog cannot be fetched (callers fall back to the static registry).
pub async fn get_qoder_model_config(
    credentials: &Credentials,
    model_key: &str,
    proxy: Option<&ProxyOptions>,
    region: &str,
    log: &Logger,
) -> Option<Json> {
    let catalog = resolve_qoder_models(credentials, proxy, region, false, log).await?;
    catalog.raw_configs.get(model_key).map(|cfg| {
        let mut cfg = cfg.clone();
        cfg["key"] = Json::String(model_key.to_string());
        cfg
    })
}

/// `routableQoderModels` — every model key the chat endpoint accepts: IDE-visible
/// models first, then catalog entries flagged `enable:false`, then the static
/// fallbacks (Sonus/Cantus) that the live catalog usually omits.
pub fn routable_qoder_models(catalog: Option<&Catalog>) -> Vec<Json> {
    let Some(catalog) = catalog else { return Vec::new() };
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    let mut catalog_keys: Vec<String> = catalog
        .models
        .iter()
        .filter_map(|m| m.get("id").and_then(Json::as_str).map(String::from))
        .collect();
    catalog_keys.extend(catalog.raw_configs.keys().cloned());

    for key in ["smodel", "cmodel"] {
        if catalog_keys.iter().any(|k| k == key) || seen.iter().any(|k| k == key) {
            continue;
        }
        if let Some(cfg) = super::constants::qoder_static_model_config(key) {
            seen.push(key.to_string());
            out.push(json!({
                "id": key,
                "name": cfg.get("display_name").and_then(Json::as_str).unwrap_or(key),
                "hidden": false,
            }));
        }
    }

    for m in &catalog.models {
        let Some(id) = m.get("id").and_then(Json::as_str) else { continue };
        if seen.iter().any(|k| k == id) {
            continue;
        }
        seen.push(id.to_string());
        out.push(json!({
            "id": id,
            "name": m.get("name").and_then(Json::as_str).unwrap_or(id),
            "hidden": false,
        }));
    }
    for (key, cfg) in &catalog.raw_configs {
        if seen.iter().any(|k| k == key) {
            continue;
        }
        seen.push(key.clone());
        out.push(json!({
            "id": key,
            "name": cfg.get("display_name").and_then(Json::as_str).unwrap_or(key),
            "hidden": true,
        }));
    }
    out
}

/// `invalidateQoderCatalog(credentials)` — drop the cached catalog for one
/// credential so the next call re-fetches.
pub fn invalidate_qoder_catalog(credentials: &Credentials) {
    let key = cache_key(credentials);
    CATALOG_CACHE.lock().expect("catalog cache").remove(&key);
}

/// Test hook: clear every cache.
pub fn clear_caches() {
    CATALOG_CACHE.lock().expect("catalog cache").clear();
    PAT_CACHE.lock().expect("pat cache").clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_detection() {
        assert!(is_qoder_pat(Some("pt-abc")));
        assert!(!is_qoder_pat(Some("dt-abc")));
        assert!(!is_qoder_pat(Some("jt-abc")));
        assert!(!is_qoder_pat(None));
    }

    #[test]
    fn expiry_parsing_accepts_iso_and_epochs() {
        let iso = parse_expiry(Some(&json!("2030-01-01T00:00:00Z")));
        assert!(iso.unwrap() > 1_800_000_000_000);
        // Seconds are widened to ms.
        let secs = parse_expiry(Some(&json!(1_800_000_000u64))).unwrap();
        assert_eq!(secs, 1_800_000_000_000);
        let ms = parse_expiry(Some(&json!(1_800_000_000_000u64))).unwrap();
        assert_eq!(ms, 1_800_000_000_000);
        assert_eq!(parse_expiry(None), None);
    }

    #[test]
    fn cache_key_is_stable_and_region_scoped() {
        let mut a = Credentials::from_value(json!({ "accessToken": "t", "providerSpecificData": { "userId": "u1" } }));
        a.set("provider", Json::String("qoder".into()));
        let mut b = a.clone();
        b.set("provider", Json::String("qoder-cn".into()));
        assert_eq!(cache_key(&a), cache_key(&a));
        assert_ne!(cache_key(&a), cache_key(&b));

        let mut c = a.clone();
        c.set("accessToken", Json::String("other".into()));
        assert_eq!(cache_key(&a), cache_key(&c), "userId seeds the key, not the token");
    }

    #[test]
    fn routable_models_include_hidden_and_static_fallbacks() {
        let mut raw = JsonMap::new();
        raw.insert("auto".into(), json!({ "key": "auto", "display_name": "Auto" }));
        raw.insert("secret".into(), json!({ "key": "secret", "display_name": "Secret", "enable": false }));
        let catalog = Catalog {
            models: vec![json!({ "id": "auto", "name": "Auto" })],
            raw_configs: raw,
        };
        let out = routable_qoder_models(Some(&catalog));
        let ids: Vec<&str> = out.iter().filter_map(|m| m.get("id").and_then(Json::as_str)).collect();
        assert_eq!(ids, vec!["smodel", "cmodel", "auto", "secret"]);
        // Catalog entries win over the static fallback for the same key.
        let hidden = out.iter().find(|m| m["id"] == json!("secret")).unwrap();
        assert_eq!(hidden["hidden"], json!(true));
        assert!(routable_qoder_models(None).is_empty());
    }

    #[test]
    fn cosy_headers_require_user_and_token() {
        let missing_user = Credentials::from_value(json!({ "accessToken": "t" }));
        assert!(cosy_headers_for(&[], "https://api3.qoder.sh/algo/api/v2/model/list", &missing_user).is_none());

        let ok = Credentials::from_value(json!({
            "accessToken": "t",
            "providerSpecificData": { "userId": "u1", "machineId": "m1" }
        }));
        let headers = cosy_headers_for(&[], "https://api3.qoder.sh/algo/api/v2/model/list", &ok).unwrap();
        let get = |k: &str| headers.iter().find(|(name, _)| name == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Cosy-User"), Some("u1"));
        assert_eq!(get("Cosy-Machineid"), Some("m1"));
        assert_eq!(get("Cosy-Sigpath"), Some("/api/v2/model/list"));
        assert!(get("Authorization").unwrap().starts_with("Bearer COSY."));
    }
}
