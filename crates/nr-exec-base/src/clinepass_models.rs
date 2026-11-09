//! Port of open-sse/services/clinepassModels.js — ClinePass model discovery.

use nr_core::http::{proxy_fetch, FetchRequest, HeaderList};
use nr_core::{Credentials, Json};
use serde_json::json;

use nr_core::http::ProxyOptions;

use crate::cline_auth::build_cline_headers;

const CLINEPASS_MODELS_ENDPOINT: &str = "https://api.cline.bot/api/v1/models";
// Cline's free tier is published here, not in /api/v1/models: the catalog
// endpoint carries no `cline-free/*` ids at all. Cline's own SDK calls this
// feed unauthenticated (sdk/packages/core/src/services/llms/cline-recommended-models.ts),
// so no Authorization header is sent — adding one would only make the request
// fail on a header the endpoint ignores.
const CLINE_RECOMMENDED_MODELS_ENDPOINT: &str =
    "https://api.cline.bot/api/v1/ai/cline/recommended-models";
const FETCH_TIMEOUT_MS: std::time::Duration = std::time::Duration::from_millis(5000);

/// Build request headers for the ClinePass /models endpoint (Cline's upstream API).
/// - API keys are sent as plain Bearer tokens.
/// - OAuth access tokens must carry the WorkOS `workos:` prefix (build_cline_headers).
fn build_model_list_headers(token: &str, is_api_key: bool) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.insert("Accept".into(), "application/json".into());
    if is_api_key {
        headers.insert("Authorization".into(), format!("Bearer {token}"));
        return headers;
    }
    for (k, v) in build_cline_headers(token) {
        headers.insert(k, v);
    }
    headers
}

/// Credentials for the Cline endpoints: the router stores `{ accessToken, apiKey }`.
pub struct ClineCredentials<'a> {
    pub access_token: Option<&'a str>,
    pub api_key: Option<&'a str>,
}

impl<'a> ClineCredentials<'a> {
    pub fn from_credentials(credentials: &'a Credentials) -> Self {
        Self {
            access_token: credentials.get("accessToken").and_then(Json::as_str),
            api_key: credentials.get("apiKey").and_then(Json::as_str),
        }
    }
}

/// Extract a `name`/`id` pair from one raw entry, or `None` when the id is not a
/// non-empty string.
fn id_name(m: &Json) -> Option<(String, String)> {
    let id = m.get("id").and_then(Json::as_str)?.to_string();
    let name = m.get("name").and_then(Json::as_str).unwrap_or(&id).to_string();
    Some((id, name))
}

/// Internal: fetch the raw model list from Cline's /models endpoint.
/// Returns the parsed array or `None` on any failure.
async fn fetch_cline_raw_models(credentials: &ClineCredentials<'_>, proxy_url: Option<&str>) -> Option<Vec<Json>> {
    let is_api_key = credentials.api_key.is_some();
    let token = credentials.api_key.or(credentials.access_token)?;
    let mut req = FetchRequest::get(CLINEPASS_MODELS_ENDPOINT);
    req.headers = build_model_list_headers(token, is_api_key);
    req.header_timeout = Some(FETCH_TIMEOUT_MS);

    let proxy = proxy_url.map(|p| ProxyOptions {
        connection_proxy_enabled: true,
        connection_proxy_url: p.to_string(),
        ..ProxyOptions::default()
    });
    let response = proxy_fetch(req, proxy.as_ref()).await.ok()?;

    if !response.ok() {
        return None;
    }
    let json = response.json_body().await.ok()?;
    let raw_list = if json.is_array() { json } else { json.get("data")?.clone() };
    raw_list.as_array().cloned()
}

fn id_names(raw_list: &[Json]) -> Vec<(String, String)> {
    raw_list.iter().filter_map(id_name).collect()
}

/// Fetch ClinePass live model catalog from Cline's /models endpoint.
/// Returns only models with the `cline-pass/` prefix.
pub async fn resolve_clinepass_models(
    credentials: &ClineCredentials<'_>,
    proxy_url: Option<&str>,
) -> Option<Vec<(String, String)>> {
    let raw_list = fetch_cline_raw_models(credentials, proxy_url).await?;
    let models: Vec<(String, String)> = id_names(&raw_list)
        .into_iter()
        .filter(|(id, _)| id.starts_with("cline-pass/"))
        .collect();
    (!models.is_empty()).then_some(models)
}

/// Fetch Cline's recommended-models feed and return only its `free[]` tier.
/// Returns `None` on any failure — the free tier is additive, so a dead feed must
/// never take the /api/v1/models catalog down with it.
async fn fetch_cline_free_tier_models(proxy_url: Option<&str>) -> Vec<(String, String)> {
    let mut req = FetchRequest::get(CLINE_RECOMMENDED_MODELS_ENDPOINT);
    req.headers.insert("Accept".into(), "application/json".into());
    req.header_timeout = Some(FETCH_TIMEOUT_MS);
    let proxy = proxy_url.map(|p| ProxyOptions {
        connection_proxy_enabled: true,
        connection_proxy_url: p.to_string(),
        ..ProxyOptions::default()
    });
    let response = match proxy_fetch(req, proxy.as_ref()).await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if !response.ok() {
        return Vec::new();
    }
    let json = match response.json_body().await {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    let free = match json.get("free").and_then(Json::as_array) {
        Some(f) if !f.is_empty() => f,
        _ => return Vec::new(),
    };
    id_names(free)
}

/// Fetch Cline live model catalog from Cline's /models endpoint.
/// Unlike `resolve_clinepass_models`, this returns ALL models (including
/// free-tier models like `z-ai/glm-5.3-flash`) without the `cline-pass/` filter.
///
/// Free tier: /api/v1/models lists no `cline-free/*` ids, so the feed's `free[]`
/// is merged in. First writer wins on a shared id, keeping the catalog's entry
/// for anything the two sources agree on.
pub async fn resolve_cline_models(
    credentials: &ClineCredentials<'_>,
    proxy_url: Option<&str>,
) -> Option<Vec<(String, String)>> {
    let raw_list = fetch_cline_raw_models(credentials, proxy_url).await?;
    let models: Vec<(String, String)> = id_names(&raw_list)
        .into_iter()
        .filter(|(id, _)| !id.trim().is_empty())
        .collect();

    let mut by_id: Vec<(String, String)> = models;
    let free_tier = fetch_cline_free_tier_models(proxy_url).await;
    for (id, name) in free_tier {
        if !by_id.iter().any(|(existing, _)| existing == &id) {
            by_id.push((id, name));
        }
    }

    (!by_id.is_empty()).then_some(by_id)
}

/// The catalog shapes callers expect: `null` maps to `None`.
pub fn models_payload(models: &[(String, String)]) -> Json {
    json!({
        "models": models.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_key_headers_use_bearer_and_accept() {
        let headers = build_model_list_headers("clp_123", true);
        assert_eq!(headers.get("Authorization"), Some(&"Bearer clp_123".to_string()));
        assert_eq!(headers.get("Accept"), Some(&"application/json".to_string()));
    }

    #[test]
    fn oauth_headers_come_from_build_cline_headers() {
        // A WorkOS JWT gets the `workos:` prefix applied by build_cline_headers.
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let headers = build_model_list_headers(jwt, false);
        assert_eq!(
            headers.get("Authorization"),
            Some(&format!("Bearer workos:{jwt}"))
        );
        // Cline identifies the client via these headers (not on the api-key branch).
        assert_eq!(headers.get("X-Title"), Some(&"Cline".to_string()));
        assert_eq!(headers.get("Accept"), Some(&"application/json".to_string()));
    }

    #[test]
    fn id_name_reads_name_or_falls_back_to_id() {
        assert_eq!(
            id_name(&json!({ "id": "cline-pass/x", "name": "X" })),
            Some(("cline-pass/x".into(), "X".into()))
        );
        assert_eq!(id_name(&json!({ "id": "z" })), Some(("z".into(), "z".into())));
        assert_eq!(id_name(&json!({ "name": "no id" })), None);
        assert_eq!(id_name(&Json::Null), None);
    }

    #[test]
    fn id_names_keeps_strings_and_skips_non_strings() {
        // `typeof m?.id === "string"` — an empty id is still a string here; both
        // resolve_* callers apply their own emptiness filter (JS does the same).
        let list = json!([
            { "id": "a" },
            { "id": 42 },
            { "id": "" },
            null,
        ]);
        assert_eq!(
            id_names(list.as_array().unwrap()),
            vec![("a".to_string(), "a".to_string()), (String::new(), String::new())]
        );
    }

    #[test]
    fn credentials_read_both_keys() {
        let creds = Credentials::from_value(json!({ "accessToken": "tok", "apiKey": "clp_1" }));
        let c = ClineCredentials::from_credentials(&creds);
        assert_eq!(c.access_token, Some("tok"));
        assert_eq!(c.api_key, Some("clp_1"));

        let empty_creds = Credentials::from_value(json!({}));
        let empty = ClineCredentials::from_credentials(&empty_creds);
        assert_eq!(empty.access_token, None);
        assert_eq!(empty.api_key, None);
    }

    #[tokio::test]
    async fn missing_token_returns_none_without_network() {
        let creds = ClineCredentials { access_token: None, api_key: None };
        assert!(resolve_clinepass_models(&creds, None).await.is_none());
        assert!(resolve_cline_models(&creds, None).await.is_none());
    }

    #[test]
    fn models_payload_shape() {
        let payload = models_payload(&[("a".into(), "A".into())]);
        assert_eq!(payload["models"][0]["id"], Json::String("a".into()));
        assert_eq!(payload["models"][0]["name"], Json::String("A".into()));
    }
}
