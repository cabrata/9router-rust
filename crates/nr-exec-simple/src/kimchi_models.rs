//! Kimchi model catalog discovery. Port of `open-sse/services/kimchiModels.js`.
//!
//! The `/v1/models/metadata?include_in_cli=true` endpoint carries per-model
//! metadata (upstream provider, modalities, limits, reasoning) that the plain
//! `/v1/models` list does not, so the executor needs this cache to decide
//! whether a model is Anthropic-backed (tool/reasoning field shaping).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, ProxyOptions};
use nr_core::{Credentials, Json};
use serde_json::{json, Value};

use nr_core::json::{get_path, truthy};

/// `KIMCHI_API`.
pub const KIMCHI_API: &str = "https://llm.kimchi.dev";
/// `KIMCHI_USER_AGENT`.
pub const KIMCHI_USER_AGENT: &str = "kimchi/0.1.40";

const FETCH_TIMEOUT_MS: u64 = 20_000;
const CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const RETRYABLE_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `normalizeKimchiEndpoint`.
fn normalize_kimchi_endpoint(endpoint: Option<&str>) -> String {
    let raw = endpoint.map(str::trim).unwrap_or("");
    let base = if raw.is_empty() { KIMCHI_API } else { raw };
    base.trim_end_matches('/').to_string()
}

/// `buildKimchiModelsUrl(endpoint)`.
pub fn build_kimchi_models_url(endpoint: Option<&str>) -> String {
    format!(
        "{}/v1/models/metadata?include_in_cli=true",
        normalize_kimchi_endpoint(endpoint)
    )
}

/// `readToken(credentials)`: accessToken, then apiKey, then psd.apiKey.
fn read_token(credentials: &Credentials) -> Option<String> {
    credentials
        .access_token()
        .or_else(|| credentials.api_key())
        .or_else(|| credentials.psd_str("apiKey"))
        .map(str::to_string)
}

/// `cacheKey(credentials, endpoint)`: stable per-account key, never the raw token.
fn cache_key(credentials: &Credentials, endpoint: Option<&str>) -> String {
    // The JS hashes; we only need a stable map key, and a hash of a secret in a
    // HashMap key is not a security boundary here. Still, keep the seed shape.
    let token = read_token(credentials);
    let seed = credentials
        .psd_str("userId")
        .or_else(|| credentials.psd_str("username"))
        .or_else(|| credentials.refresh_token())
        .or_else(|| token.as_deref())
        .unwrap_or("anonymous");
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("kimchi:{}:{}", normalize_kimchi_endpoint(endpoint), seed).as_bytes());
    hex::encode(hasher.finalize())
}

/// `toModelKind`: image-capable inputs → `imageToText`, else `llm`.
fn to_model_kind(input_modalities: &[Value]) -> &'static str {
    if input_modalities.iter().any(|m| m.as_str() == Some("image")) {
        "imageToText"
    } else {
        "llm"
    }
}

/// `normalizeKimchiModel`: one raw catalog entry → the model shape the router
/// stores. Returns `None` when the entry has no usable id.
pub fn normalize_kimchi_model(item: &Json) -> Option<Json> {
    let obj = item.as_object()?;
    let id_value = obj
        .get("slug")
        .or_else(|| obj.get("id"))
        .or_else(|| obj.get("model"))
        .or_else(|| obj.get("name"))?;
    let id = id_value.as_str()?.trim();
    if id.is_empty() {
        return None;
    }

    let input_modalities: Vec<Value> = obj
        .get("input_modalities")
        .and_then(Json::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|v| matches!(v.as_str(), Some("text") | Some("image")))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    let limits = obj.get("limits").filter(|v| v.is_object());
    let number = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|n| *n != 0.0);
    let limit = |key: &str| limits.and_then(|l| l.get(key));
    let context_length = number(limit("context_window"))
        .or_else(|| number(obj.get("contextLength")))
        .or_else(|| number(obj.get("context_length")));
    let max_output_tokens = number(limit("max_output_tokens"))
        .or_else(|| number(obj.get("maxOutputTokens")))
        .or_else(|| number(obj.get("max_output_tokens")));

    let upstream_provider = obj.get("provider").and_then(Json::as_str).unwrap_or("").to_string();
    let reasoning = obj.get("reasoning").and_then(Json::as_bool) == Some(true);
    let kind = to_model_kind(&input_modalities);

    let name = ["display_name", "displayName", "name"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Json::as_str).map(str::trim))
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_string();

    let vision = input_modalities.iter().any(|m| m.as_str() == Some("image"));
    let mut capabilities = serde_json::Map::new();
    capabilities.insert("vision".into(), json!(vision));
    capabilities.insert("reasoning".into(), json!(reasoning));
    if let Some(ctx) = context_length {
        capabilities.insert("contextWindow".into(), json!(ctx));
    }
    if let Some(max) = max_output_tokens {
        capabilities.insert("maxOutput".into(), json!(max));
    }
    if !upstream_provider.is_empty() {
        capabilities.insert("upstreamProvider".into(), json!(upstream_provider));
    }

    // The raw entry is spread first, then the normalized fields overwrite it.
    let mut model = obj.clone();
    model.insert("id".into(), json!(id));
    model.insert("name".into(), json!(name));
    model.insert("provider".into(), json!(upstream_provider));
    model.insert("upstreamProvider".into(), json!(upstream_provider));
    model.insert("reasoning".into(), json!(reasoning));
    model.insert("inputModalities".into(), Json::Array(input_modalities));
    model.insert("kind".into(), json!(kind));
    model.insert("type".into(), json!(kind));
    model.insert("capabilities".into(), Json::Object(capabilities));
    if let Some(ctx) = context_length {
        model.insert("contextLength".into(), json!(ctx));
    }
    if let Some(max) = max_output_tokens {
        model.insert("maxOutputTokens".into(), json!(max));
    }
    if upstream_provider == "anthropic" {
        model.insert(
            "compat".into(),
            json!({ "supportsReasoningEffort": false, "cacheControlFormat": "anthropic" }),
        );
    }
    Some(Json::Object(model))
}

/// One cached catalog per account/endpoint.
#[derive(Clone, Debug)]
pub struct KimchiCatalog {
    pub expires_at: u64,
    pub models: Vec<Json>,
    pub raw_models: Vec<Json>,
}

static CATALOG_CACHE: LazyLock<std::sync::Mutex<HashMap<String, KimchiCatalog>>> =
    LazyLock::new(Default::default);
static METADATA_BY_MODEL_ID: LazyLock<std::sync::Mutex<HashMap<String, Json>>> =
    LazyLock::new(Default::default);

/// `rememberModels`: index by id and lowercased id.
fn remember_models(models: &[Json]) {
    let mut index = METADATA_BY_MODEL_ID.lock().expect("metadata index poisoned");
    for model in models {
        let Some(id) = model.get("id").and_then(Json::as_str) else { continue };
        if id.is_empty() {
            continue;
        }
        index.insert(id.to_string(), model.clone());
        index.insert(id.to_lowercase(), model.clone());
    }
}

/// `getCachedKimchiModelMetadata(modelId)`: metadata for a bare or
/// `provider/model` id, case-insensitively.
pub fn get_cached_kimchi_model_metadata(model_id: &str) -> Option<Json> {
    let trimmed = model_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let index = METADATA_BY_MODEL_ID.lock().expect("metadata index poisoned");
    index
        .get(raw)
        .or_else(|| index.get(&raw.to_lowercase()))
        .cloned()
}

/// Fetch the raw catalog. Errors carry `(message, status, retryable)` like the
/// JS error object.
async fn fetch_kimchi_catalog_raw(
    token: &str,
    endpoint: Option<&str>,
    proxy_options: Option<&ProxyOptions>,
) -> Result<Vec<Json>, (String, u16, bool)> {
    let url = build_kimchi_models_url(endpoint);
    let mut headers = HeaderList::new();
    headers.insert("Accept".into(), "application/json".into());
    headers.insert("Authorization".into(), format!("Bearer {token}"));
    headers.insert("User-Agent".into(), KIMCHI_USER_AGENT.into());

    let mut req = FetchRequest::get(&url);
    req.headers = headers;
    req.header_timeout = Some(Duration::from_millis(FETCH_TIMEOUT_MS));

    let response = proxy_fetch(req, proxy_options).await.map_err(|e| (e.to_string(), 0, false))?;
    if !response.ok() {
        let status = response.status;
        return Err((
            format!("Kimchi models {status}: {}", http_status_text(status)),
            status,
            RETRYABLE_STATUSES.contains(&status),
        ));
    }
    let body = response.json_body().await.map_err(|e| (e.to_string(), 0, false))?;
    Ok(body.get("models").and_then(Json::as_array).cloned().unwrap_or_default())
}

/// HTTP reason phrase for the status codes this module can surface.
fn http_status_text(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// `resolveKimchiModels(credentials, options)`: cached catalog for a connection.
///
/// Returns `None` when there is no token, the fetch fails, or the catalog is
/// empty — callers fall back to the static model list.
pub async fn resolve_kimchi_models(
    credentials: &Credentials,
    endpoint: Option<&str>,
    force_refresh: bool,
    proxy_options: Option<&ProxyOptions>,
) -> Option<KimchiCatalog> {
    let token = read_token(credentials)?;
    let endpoint = credentials.psd_str("kimchiEndpoint").or(endpoint);
    let key = cache_key(credentials, endpoint);

    if !force_refresh {
        let cached = CATALOG_CACHE.lock().expect("catalog cache poisoned").get(&key).cloned();
        if let Some(entry) = cached.filter(|e| e.expires_at > now_ms()) {
            return Some(entry);
        }
    }

    let raw_models = match fetch_kimchi_catalog_raw(&token, endpoint, proxy_options).await {
        Ok(models) => models,
        Err((message, _, _)) => {
            tracing::warn!("[KIMCHI_MODELS] {message}");
            return None;
        }
    };

    let models: Vec<Json> = raw_models.iter().filter_map(normalize_kimchi_model).collect();
    if models.is_empty() {
        return None;
    }

    remember_models(&models);
    let entry = KimchiCatalog { expires_at: now_ms() + CACHE_TTL_MS, models, raw_models };
    CATALOG_CACHE.lock().expect("catalog cache poisoned").insert(key, entry.clone());
    Some(entry)
}

/// `clearKimchiCatalog()`.
pub fn clear_kimchi_catalog() {
    CATALOG_CACHE.lock().expect("catalog cache poisoned").clear();
    METADATA_BY_MODEL_ID.lock().expect("metadata index poisoned").clear();
}

/// `isAnthropicBackedKimchiModel(model)`: metadata first, then the id shape.
pub fn is_anthropic_backed_kimchi_model(model: &str) -> bool {
    if let Some(meta) = get_cached_kimchi_model_metadata(model) {
        let provider = meta.get("provider").and_then(Json::as_str);
        let upstream = meta.get("upstreamProvider").and_then(Json::as_str);
        if provider == Some("anthropic") || upstream == Some("anthropic") {
            return true;
        }
    }
    // `/(^|[-_/])(?:claude|anthropic)(?:[-_/]|$)/i`
    let lower = model.to_lowercase();
    for token in ["claude", "anthropic"] {
        let mut search = lower.as_str();
        while let Some(pos) = search.find(token) {
            let before_ok = pos == 0
                || matches!(lower.as_bytes()[pos - 1], b'-' | b'_' | b'/');
            let after = pos + token.len();
            let after_ok = after == lower.len()
                || matches!(lower.as_bytes()[after], b'-' | b'_' | b'/');
            if before_ok && after_ok {
                return true;
            }
            search = &search[pos + token.len()..];
        }
    }
    false
}

/// Test/`/api` helper: the cached catalog shape as JSON.
pub fn catalog_payload(entry: &KimchiCatalog) -> Value {
    json!({
        "expiresAt": entry.expires_at,
        "models": entry.models,
        "rawModels": entry.raw_models,
    })
}

/// `truthy` re-export guard: keeps the JSON-truthiness helper honest for callers
/// that build catalog entries from JS-shaped values.
pub fn has_entries(value: &Json) -> bool {
    truthy(get_path(value, &["models"]).unwrap_or(&Json::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn endpoint_normalization_and_url() {
        assert_eq!(build_kimchi_models_url(None), "https://llm.kimchi.dev/v1/models/metadata?include_in_cli=true");
        assert_eq!(
            build_kimchi_models_url(Some("https://alt.example.com///")),
            "https://alt.example.com/v1/models/metadata?include_in_cli=true"
        );
        assert_eq!(
            build_kimchi_models_url(Some("   ")),
            "https://llm.kimchi.dev/v1/models/metadata?include_in_cli=true"
        );
    }

    #[test]
    fn normalize_uses_slug_and_display_name() {
        let raw = json!({
            "slug": "claude-sonnet-4",
            "display_name": "Claude Sonnet 4",
            "provider": "anthropic",
            "reasoning": true,
            "input_modalities": ["text", "image", "audio"],
            "limits": { "context_window": 200000, "max_output_tokens": 64000 }
        });
        let model = normalize_kimchi_model(&raw).expect("model");
        assert_eq!(model["id"], "claude-sonnet-4");
        assert_eq!(model["name"], "Claude Sonnet 4");
        assert_eq!(model["provider"], "anthropic");
        assert_eq!(model["upstreamProvider"], "anthropic");
        assert_eq!(model["reasoning"], true);
        assert_eq!(model["kind"], "imageToText");
        assert_eq!(model["type"], "imageToText");
        // Only text/image survive the filter.
        assert_eq!(model["inputModalities"], json!(["text", "image"]));
        assert_eq!(model["capabilities"]["vision"], true);
        assert_eq!(model["capabilities"]["reasoning"], true);
        assert_eq!(model["capabilities"]["contextWindow"], 200000);
        assert_eq!(model["capabilities"]["maxOutput"], 64000);
        assert_eq!(model["contextLength"], 200000);
        assert_eq!(model["maxOutputTokens"], 64000);
        assert_eq!(model["compat"]["cacheControlFormat"], "anthropic");
    }

    #[test]
    fn normalize_falls_back_to_id_and_name() {
        let model = normalize_kimchi_model(&json!({"id": "glm-4", "name": "GLM 4"})).expect("model");
        assert_eq!(model["id"], "glm-4");
        assert_eq!(model["name"], "GLM 4");
        assert_eq!(model["kind"], "llm");
        assert!(model.get("compat").is_none(), "non-anthropic has no compat block");

        let slug = normalize_kimchi_model(&json!({"slug": "s"})).expect("slug wins");
        assert_eq!(slug["id"], "s");
    }

    #[test]
    fn normalize_rejects_missing_or_blank_id() {
        assert!(normalize_kimchi_model(&json!({})).is_none());
        assert!(normalize_kimchi_model(&json!({"id": "   "})).is_none());
        assert!(normalize_kimchi_model(&json!({"id": 42})).is_none());
        assert!(normalize_kimchi_model(&Json::Null).is_none());
    }

    #[test]
    fn metadata_lookup_strips_provider_prefix() {
        let _guard = serial();
        clear_kimchi_catalog();
        let models = vec![normalize_kimchi_model(&json!({"id": "Kimi-K2", "provider": "moonshot"})).unwrap()];
        remember_models(&models);

        assert_eq!(
            get_cached_kimchi_model_metadata("Kimi-K2").unwrap()["provider"],
            "moonshot"
        );
        assert_eq!(
            get_cached_kimchi_model_metadata("kimchi/Kimi-K2").unwrap()["provider"],
            "moonshot"
        );
        // Case-insensitive.
        assert!(get_cached_kimchi_model_metadata("kimi-k2").is_some());
        assert!(get_cached_kimchi_model_metadata("nope").is_none());
        assert!(get_cached_kimchi_model_metadata("").is_none());
        clear_kimchi_catalog();
    }

    #[test]
    fn anthropic_backed_detection() {
        let _guard = serial();
        clear_kimchi_catalog();
        assert!(is_anthropic_backed_kimchi_model("claude-sonnet-4"));
        assert!(is_anthropic_backed_kimchi_model("kimchi/claude-3-5"));
        assert!(is_anthropic_backed_kimchi_model("some_anthropic_model"));
        assert!(!is_anthropic_backed_kimchi_model("glm-4"));
        // Substring but not a whole token.
        assert!(!is_anthropic_backed_kimchi_model("claudette"));
        assert!(!is_anthropic_backed_kimchi_model("anthropicx"));
    }

    #[test]
    fn anthropic_backed_uses_cached_metadata() {
        let _guard = serial();
        clear_kimchi_catalog();
        let models = vec![normalize_kimchi_model(&json!({"id": "mystery-1", "provider": "anthropic"})).unwrap()];
        remember_models(&models);
        assert!(is_anthropic_backed_kimchi_model("mystery-1"));
        clear_kimchi_catalog();
        assert!(!is_anthropic_backed_kimchi_model("mystery-1"));
    }

    #[test]
    fn status_text_matches_common_codes() {
        assert_eq!(http_status_text(429), "Too Many Requests");
        assert_eq!(http_status_text(503), "Service Unavailable");
        assert_eq!(http_status_text(418), "");
        assert!(RETRYABLE_STATUSES.contains(&429));
        assert!(!RETRYABLE_STATUSES.contains(&401));
    }

    #[test]
    fn cache_key_is_per_endpoint_and_account() {
        let creds = Credentials::from_value(json!({"accessToken": "tok-a"}));
        let other = Credentials::from_value(json!({"accessToken": "tok-b"}));
        let k1 = cache_key(&creds, None);
        assert_eq!(k1, cache_key(&creds, None), "stable for the same inputs");
        assert_ne!(k1, cache_key(&creds, Some("https://alt.example.com")));
        assert_ne!(k1, cache_key(&other, None), "different tokens differ");
        assert_eq!(k1.len(), 64, "sha256 hex");

        // A userId takes precedence over the token.
        let named = Credentials::from_value(json!({"accessToken": "tok-a", "providerSpecificData": {"userId": "u1"}}));
        assert_ne!(k1, cache_key(&named, None));
    }

    #[test]
    fn token_precedence() {
        assert_eq!(
            read_token(&Credentials::from_value(json!({"accessToken": "a", "apiKey": "k"}))),
            Some("a".into())
        );
        assert_eq!(
            read_token(&Credentials::from_value(json!({"apiKey": "k"}))),
            Some("k".into())
        );
        assert_eq!(
            read_token(&Credentials::from_value(json!({"providerSpecificData": {"apiKey": "p"}}))),
            Some("p".into())
        );
        assert_eq!(read_token(&Credentials::from_value(json!({}))), None);
    }

    #[test]
    fn catalog_payload_shape() {
        let entry = KimchiCatalog { expires_at: 123, models: vec![json!({"id": "a"})], raw_models: vec![] };
        let payload = catalog_payload(&entry);
        assert_eq!(payload["expiresAt"], 123);
        assert_eq!(payload["models"][0]["id"], "a");
        assert!(payload["rawModels"].as_array().unwrap().is_empty());
    }

    #[test]
    fn has_entries_uses_js_truthiness() {
        assert!(has_entries(&json!({"models": [{"id": "a"}]})));
        assert!(!has_entries(&json!({"models": []})));
        assert!(!has_entries(&json!({})));
        assert!(!has_entries(&Json::Null));
    }
}
