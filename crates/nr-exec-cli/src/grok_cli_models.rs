//! Port of open-sse/services/grokCliModels.js
//!
//! Live model discovery for the Grok CLI provider: GET `{base}/models` with the
//! xAI CLI headers, refresh once on 401/403, then normalize whatever shape the
//! endpoint returns.

use nr_core::http::{proxy_fetch, FetchRequest, HeaderList};
use nr_core::{Credentials, Json};
use serde_json::json;

use crate::grok_cli::grok_cli_const;

fn models_url() -> String {
    let base = grok_cli_const("GROK_CLI_BASE_URL")
        .unwrap_or_else(|| "https://cli-chat-proxy.grok.com/v1".to_string());
    format!("{}/models", base.trim_end_matches('/'))
}

fn config_str(key: &str, fallback: &str) -> String {
    grok_cli_const(key).unwrap_or_else(|| fallback.to_string())
}

/// `modelEntries(data)`: iterate `[key, value]` pairs for array or object payloads.
fn model_entries(data: &Json) -> Vec<(Option<String>, Json)> {
    let value = if data.is_array() {
        data
    } else {
        data.get("data")
            .or_else(|| data.get("models"))
            .or_else(|| data.get("results"))
            .unwrap_or(&Json::Null)
    };
    match value {
        Json::Array(items) => items.iter().map(|item| (None, item.clone())).collect(),
        Json::Object(map) => map.iter().map(|(k, v)| (Some(k.clone()), v.clone())).collect(),
        _ => Vec::new(),
    }
}

/// Coerce a JSON value the way JS `Number()` does for the fields we read.
fn number_of(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => {
            let t = s.trim();
            if t.is_empty() { None } else { t.parse::<f64>().ok() }
        }
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Build a `Json::Number` for a positive finite value.
fn positive_number(value: f64) -> Option<Json> {
    if !(value.is_finite() && value > 0.0) {
        return None;
    }
    // Keep integral values as integers so the JSON reads `500000`, not `500000.0`
    // (JS numbers serialize without a fractional part).
    if value.fract() == 0.0 && value <= i64::MAX as f64 {
        return Some(Json::Number(serde_json::Number::from(value as i64)));
    }
    serde_json::Number::from_f64(value).map(Json::Number)
}

/// `parseGrokCliModels(data)` — deduped model objects, original fields preserved.
pub fn parse_grok_cli_models(data: &Json) -> Vec<Json> {
    static GROK_CLI_MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let cli_model = GROK_CLI_MODEL
        .get_or_init(|| config_str("GROK_CLI_MODEL", "grok-build"))
        .clone();

    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();

    for (key, raw) in model_entries(data) {
        let item: Json = match raw {
            Json::String(s) => json!({ "id": s }),
            other @ Json::Object(_) => other,
            _ => continue,
        };

        // `item.id ?? item.model_id ?? ... ?? key ?? item.name ?? ""`
        let id_raw = [
            "id", "model_id", "modelId", "model", "slug",
        ]
        .iter()
        .find_map(|k| item.get(*k).map(|v| js_string(v)))
        .or_else(|| key.clone())
        .or_else(|| item.get("name").map(js_string))
        .unwrap_or_default();
        let id = id_raw.trim().to_string();
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }

        let mut model = match item.clone() {
            Json::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        model.insert("id".into(), Json::String(id.clone()));

        let name = ["display_name", "displayName", "name"]
            .iter()
            .find_map(|k| model.get(*k).map(|v| js_string(v)))
            .unwrap_or_else(|| id.clone());
        model.insert("name".into(), Json::String(name));

        let context_length = [
            "context_length", "contextLength", "context_window", "contextWindow",
        ]
        .iter()
        .find_map(|k| number_of(model.get(*k)));
        let max_output = ["max_output_tokens", "maxOutputTokens"]
            .iter()
            .find_map(|k| number_of(model.get(*k)));

        if let Some(v) = context_length.and_then(positive_number) {
            model.insert("contextLength".into(), v);
        }
        if let Some(v) = max_output.and_then(positive_number) {
            model.insert("maxOutputTokens".into(), v);
        }
        if id == cli_model {
            // `model.contextLength ||= 500000`
            let needs_ctx = model
                .get("contextLength")
                .and_then(|v| number_of(Some(v)))
                .map(|n| !(n > 0.0))
                .unwrap_or(true);
            if needs_ctx {
                model.insert("contextLength".into(), json!(500000));
            }
            let needs_out = model
                .get("maxOutputTokens")
                .and_then(|v| number_of(Some(v)))
                .map(|n| !(n > 0.0))
                .unwrap_or(true);
            if needs_out {
                model.insert("maxOutputTokens".into(), json!(64000));
            }
        }
        models.push(Json::Object(model));
    }

    models
}

/// JS `String(value)` for the id/name fields.
fn js_string(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => b.to_string(),
        Json::Null => "null".into(),
        _ => String::new(),
    }
}

/// `buildHeaders(accessToken, providerSpecificData)` for the models endpoint.
pub fn build_model_list_headers(access_token: &str, provider_specific_data: Option<&Json>) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.insert("Authorization".into(), format!("Bearer {access_token}"));
    headers.insert("Accept".into(), "application/json".into());
    headers.insert(
        "User-Agent".into(),
        config_str("GROK_CLI_USER_AGENT", "grok-shell/1.0.44 (linux; x86_64)"),
    );
    headers.insert("x-xai-token-auth".into(), "xai-grok-cli".into());
    headers.insert(
        "x-grok-client-version".into(),
        config_str("GROK_CLI_VERSION", "1.0.44"),
    );
    headers.insert(
        "x-grok-client-identifier".into(),
        config_str("GROK_CLI_CLIENT_IDENTIFIER", "grok-shell"),
    );
    headers.insert("x-grok-client-mode".into(), "headless".into());
    if let Some(psd) = provider_specific_data {
        if let Some(email) = psd.get("email").and_then(Json::as_str).filter(|e| !e.is_empty()) {
            headers.insert("x-email".into(), email.to_string());
        }
        let user_id = psd
            .get("userId")
            .and_then(Json::as_str)
            .filter(|v| !v.is_empty())
            .or_else(|| psd.get("principalId").and_then(Json::as_str).filter(|v| !v.is_empty()));
        if let Some(user_id) = user_id {
            headers.insert("x-userid".into(), user_id.to_string());
        }
    }
    headers
}

/// What `resolveGrokCliModels` reports back: models plus an optional warning.
#[derive(Debug, Clone, Default)]
pub struct GrokCliModels {
    pub models: Vec<Json>,
    pub warning: Option<String>,
}

/// `resolveGrokCliModels(credentials, options)` — token refresh on 401/403 is
/// delegated to `nr_oauth::credential_manager::refresh_provider_credentials` by
/// the caller (the refresh hook also persists the rotated credentials).
pub async fn resolve_grok_cli_models(credentials: &Credentials, proxy_url: Option<&str>) -> GrokCliModels {
    let access_token = match credentials.get("accessToken").and_then(Json::as_str) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return GrokCliModels {
                models: Vec::new(),
                warning: Some("Grok CLI access token is missing.".into()),
            }
        }
    };

    let psd = credentials.psd();
    let response = request_models(&access_token, psd, proxy_url).await;

    let response = match response {
        Ok(r) => r,
        Err(e) => {
            return GrokCliModels {
                models: Vec::new(),
                warning: Some(format!("Grok CLI model discovery failed: {e}")),
            }
        }
    };

    if !response.ok() {
        let status = response.status;
        let detail = response.text().await.unwrap_or_default();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {}", detail.chars().take(160).collect::<String>())
        };
        return GrokCliModels {
            models: Vec::new(),
            warning: Some(format!("Grok CLI model discovery failed ({status}){suffix}")),
        };
    }

    let payload = response.json_body().await.unwrap_or(Json::Null);
    let models = parse_grok_cli_models(&payload);
    if models.is_empty() {
        return GrokCliModels {
            models: Vec::new(),
            warning: Some("Grok CLI returned no selectable models.".into()),
        };
    }
    GrokCliModels { models, warning: None }
}

async fn request_models(
    access_token: &str,
    psd: Option<&serde_json::Map<String, Json>>,
    proxy_url: Option<&str>,
) -> nr_core::Result<nr_core::http::UpstreamResponse> {
    let psd_value = psd.map(|m| Json::Object(m.clone()));
    let mut req = FetchRequest::get(models_url());
    req.headers = build_model_list_headers(access_token, psd_value.as_ref());
    let proxy = proxy_url.map(|p| nr_core::http::ProxyOptions {
        connection_proxy_enabled: true,
        connection_proxy_url: p.to_string(),
        ..nr_core::http::ProxyOptions::default()
    });
    proxy_fetch(req, proxy.as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_openai_style_list() {
        let payload = json!({
            "data": [
                { "id": "grok-4.5", "display_name": "Grok 4.5", "context_length": 256000 },
                { "id": "grok-4.5", "display_name": "dup" },
                { "id": "grok-build" },
            ]
        });
        let models = parse_grok_cli_models(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], Json::String("grok-4.5".into()));
        assert_eq!(models[0]["name"], Json::String("Grok 4.5".into()));
        assert_eq!(models[0]["contextLength"], json!(256000));
        // grok-build gets the CLI defaults.
        assert_eq!(models[1]["contextLength"], json!(500000));
        assert_eq!(models[1]["maxOutputTokens"], json!(64000));
        assert_eq!(models[1]["name"], Json::String("grok-build".into()));
    }

    #[test]
    fn parses_bare_array_and_strings() {
        let models = parse_grok_cli_models(&json!(["a", "b"]));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], Json::String("a".into()));
        assert_eq!(models[1]["name"], Json::String("b".into()));
    }

    #[test]
    fn parses_object_map_with_key_fallback() {
        let payload = json!({
            "models": {
                "key-1": { "name": "Model One" },
                "key-2": "string-model",
            }
        });
        let models = parse_grok_cli_models(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], Json::String("key-1".into()));
        assert_eq!(models[0]["name"], Json::String("Model One".into()));
        assert_eq!(models[1]["id"], Json::String("string-model".into()));
    }

    #[test]
    fn top_level_payload_without_known_envelope_yields_no_models() {
        // No `data`/`models`/`results` key: JS `[]`, so an unrelated key holding a
        // model-shaped object is not a model.
        assert!(parse_grok_cli_models(&json!({ "m": { "model_id": "from-model-id" } })).is_empty());
    }

    #[test]
    fn id_prefers_richer_fields_over_key() {
        let payload = json!({ "data": { "m": { "model_id": "from-model-id" } } });
        let models = parse_grok_cli_models(&payload);
        assert_eq!(models[0]["id"], Json::String("from-model-id".into()));
    }

    #[test]
    fn context_fields_accept_aliases_and_numeric_strings() {
        let payload = json!({
            "data": [
                { "id": "x", "contextWindow": "128000", "maxOutputTokens": "8192" },
                { "id": "y", "contextLength": 0, "context_window": 32000 },
            ]
        });
        let models = parse_grok_cli_models(&payload);
        assert_eq!(models[0]["contextLength"], json!(128000));
        assert_eq!(models[0]["maxOutputTokens"], json!(8192));
        // JS `??` only skips null/undefined, so `contextLength: 0` wins the lookup;
        // `0 > 0` is false, so the normalized field is NOT written and the raw
        // `...item` spread value (0) survives.
        assert_eq!(models[1]["contextLength"], json!(0));
        assert!(models[1].get("maxOutputTokens").is_none());
    }

    #[test]
    fn skips_entries_without_usable_id() {
        let models = parse_grok_cli_models(&json!({
            "data": [
                { "id": "   " },
                { "name": "" },
                [1, 2, 3],
                42,
                null,
            ]
        }));
        assert!(models.is_empty());
    }

    #[test]
    fn empty_and_unknown_payloads_yield_no_models() {
        assert!(parse_grok_cli_models(&Json::Null).is_empty());
        assert!(parse_grok_cli_models(&json!({})).is_empty());
        assert!(parse_grok_cli_models(&json!({ "data": "nope" })).is_empty());
    }

    #[test]
    fn preserves_extra_upstream_fields() {
        let models = parse_grok_cli_models(&json!({
            "data": [{ "id": "grok-4.5", "owned_by": "xai", "temperature": 1 }]
        }));
        assert_eq!(models[0]["owned_by"], Json::String("xai".into()));
        assert_eq!(models[0]["temperature"], json!(1));
    }

    #[test]
    fn headers_include_cli_identity_and_optional_user() {
        let psd = json!({ "email": "a@b.c", "principalId": "u-1" });
        let headers = build_model_list_headers("tok", Some(&psd));
        assert_eq!(headers.get("Authorization"), Some(&"Bearer tok".to_string()));
        assert_eq!(headers.get("x-xai-token-auth"), Some(&"xai-grok-cli".to_string()));
        assert_eq!(
            headers.get("x-grok-client-identifier"),
            Some(&"grok-shell".to_string())
        );
        assert_eq!(headers.get("x-grok-client-mode"), Some(&"headless".to_string()));
        assert_eq!(headers.get("x-email"), Some(&"a@b.c".to_string()));
        assert_eq!(headers.get("x-userid"), Some(&"u-1".to_string()));
    }

    #[test]
    fn headers_omit_user_fields_when_missing() {
        let headers = build_model_list_headers("tok", None);
        assert!(headers.get("x-email").is_none());
        assert!(headers.get("x-userid").is_none());
        assert!(headers.get("x-grok-client-version").is_some());
    }

    #[test]
    fn userId_wins_over_principalId() {
        let psd = json!({ "userId": "primary", "principalId": "fallback" });
        let headers = build_model_list_headers("tok", Some(&psd));
        assert_eq!(headers.get("x-userid"), Some(&"primary".to_string()));
    }

    #[tokio::test]
    async fn missing_token_reports_warning() {
        let creds = Credentials::from_value(json!({}));
        let result = resolve_grok_cli_models(&creds, None).await;
        assert!(result.models.is_empty());
        assert_eq!(result.warning.as_deref(), Some("Grok CLI access token is missing."));
    }

    #[test]
    fn models_url_uses_config_base() {
        assert!(models_url().ends_with("/models"));
        assert!(models_url().starts_with("https://"));
    }

    #[test]
    fn js_string_covers_scalar_shapes() {
        assert_eq!(js_string(&json!("x")), "x");
        assert_eq!(js_string(&json!(5)), "5");
        assert_eq!(js_string(&json!(true)), "true");
        assert_eq!(js_string(&Json::Null), "null");
    }
}
