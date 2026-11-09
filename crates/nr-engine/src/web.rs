//! Web search + web fetch. Port of open-sse/handlers/search/ (index.js,
//! callers.js) and open-sse/handlers/fetch/index.js.
//!
//! Provider request shapes come from `nr_data::provider_media(id)`
//! (`searchConfig` / `fetchConfig`). Per-provider response *normalizers* are
//! a follow-up (see report); search returns the raw provider JSON envelope.

use nr_core::http::UpstreamResponse;
use nr_core::{json, Json, Result};
use nr_db::Db;

use crate::media::rotate_media_accounts;
use crate::ssrf::assert_public_url;

// ─── Web search ───────────────────────────────────────────────────────────

/// Resolve a search provider id from the request (`provider` or `model`).
fn search_provider_id(body: &Json) -> Option<String> {
    body.get("provider")
        .or_else(|| body.get("model"))
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build + send a search request to one provider, rotating its accounts.
pub async fn handle_search(db: &Db, body: &Json) -> Result<UpstreamResponse> {
    let Some(provider) = search_provider_id(body) else {
        return Err(nr_core::Error::BadRequest("Missing required field: provider (or model)".into()));
    };
    let query = body.get("query").and_then(Json::as_str).map(str::trim).unwrap_or("");
    if query.is_empty() {
        return Err(nr_core::Error::BadRequest("Missing required field: query".into()));
    }

    let media = nr_data::provider_media(&provider);
    let config = media
        .as_ref()
        .and_then(|m| m.get("searchConfig"))
        .ok_or_else(|| nr_core::Error::NotFound(format!("Provider '{provider}' does not support search.")))?;
    let base_url = config
        .get("baseUrl")
        .and_then(Json::as_str)
        .ok_or_else(|| nr_core::Error::BadRequest(format!("Provider '{provider}' has no search endpoint.")))?;

    let max_results = body.get("maxResults").and_then(Json::as_i64).unwrap_or(
        config.get("defaultMaxResults").and_then(Json::as_i64).unwrap_or(5),
    );
    let search_type = body
        .get("searchType")
        .and_then(Json::as_str)
        .unwrap_or("web")
        .to_string();

    // GET-style providers put the query in the URL; POST-style in the body.
    let method = config.get("method").and_then(Json::as_str).unwrap_or("POST");
    let is_get = method.eq_ignore_ascii_case("GET");

    if is_get {
        // Validate the base URL: search endpoints are provider-configured, not
        // user-supplied, but the guard catches a misconfigured internal endpoint.
        assert_public_url(base_url).map_err(nr_core::Error::BadRequest)?;
        let mut url = base_url.to_string();
        url.push_str(if url.contains('?') { "&" } else { "?" });
        url.push_str(&format!(
            "q={}&count={}&search_type={}",
            urlencoding_encode(query),
            max_results,
            urlencoding_encode(&search_type)
        ));
        attempt_search(db, &provider, &url, Json::Null, "GET").await
    } else {
        assert_public_url(base_url).map_err(nr_core::Error::BadRequest)?;
        let mut payload = json!({ "query": query, "maxResults": max_results });
        // Providers using snake_case for the count field.
        payload["num_results"] = json!(max_results);
        attempt_search(db, &provider, base_url, payload, "POST").await
    }
}

async fn attempt_search(
    db: &Db,
    provider: &str,
    url: &str,
    payload: Json,
    method: &'static str,
) -> Result<UpstreamResponse> {
    let url = url.to_string();
    let attempt: crate::media::MediaAttemptFn<'_> = Box::new(move |creds| {
        let url = url.clone();
        let payload = payload.clone();
        Box::pin(async move { run_media_request(creds, url, payload, method).await })
    });
    rotate_media_accounts(db, provider, None, attempt).await
}

/// Web fetch (URL → markdown/text). Port of open-sse/handlers/fetch/index.js.
pub async fn handle_fetch(db: &Db, body: &Json) -> Result<UpstreamResponse> {
    let Some(provider) = search_provider_id(body) else {
        return Err(nr_core::Error::BadRequest("Missing required field: provider (or model)".into()));
    };
    let Some(target) = body.get("url").and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(nr_core::Error::BadRequest("Missing required field: url".into()));
    };
    // User-supplied URL: SSRF guard is mandatory (block private/metadata targets).
    crate::ssrf::assert_public_url_resolved(target)
        .await
        .map_err(nr_core::Error::BadRequest)?;

    let config = nr_data::provider_media(&provider)
        .and_then(|m| m.get("fetchConfig"))
        .ok_or_else(|| nr_core::Error::NotFound(format!("Provider '{provider}' does not support web fetch.")))?;
    let base_url = config
        .get("baseUrl")
        .and_then(Json::as_str)
        .ok_or_else(|| nr_core::Error::BadRequest(format!("Provider '{provider}' has no fetch endpoint.")))?;
    let format = body
        .get("format")
        .and_then(Json::as_str)
        .unwrap_or("markdown")
        .to_string();

    let payload = json!({ "url": target, "format": format });
    assert_public_url(base_url).map_err(nr_core::Error::BadRequest)?;
    let url = base_url.to_string();
    let attempt: crate::media::MediaAttemptFn<'_> = Box::new(move |creds| {
        let url = url.clone();
        let payload = payload.clone();
        Box::pin(async move { run_media_request(creds, url, payload, "POST").await })
    });
    rotate_media_accounts(db, &provider, None, attempt).await
}

/// Issue one provider media request with the credential applied.
async fn run_media_request(
    creds: nr_core::Credentials,
    url: String,
    payload: Json,
    method: &str,
) -> Result<UpstreamResponse> {
    let mut headers = nr_core::http::HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    // `bearer` config = Authorization header; otherwise the config names a custom header.
    if let Some(key) = creds.api_key().or_else(|| creds.access_token()) {
        if creds.psd().and_then(|p| p.get("authHeader")).and_then(Json::as_str) != Some("none") {
            headers.insert("Authorization".into(), format!("Bearer {key}"));
        }
    }
    let is_get = method.eq_ignore_ascii_case("GET");
    let method = if is_get {
        http::Method::GET
    } else {
        http::Method::POST
    };
    let mut req = nr_core::http::FetchRequest::new(method, url);
    req.headers = headers;
    if !is_get {
        req.body = Some(payload.to_string().into_bytes().into());
    }
    let proxy = nr_core::http::ProxyOptions::from_credentials(&creds);
    nr_core::http::proxy_fetch(req, Some(&proxy)).await
}

fn urlencoding_encode(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_requires_provider_and_query() {
        assert_eq!(search_provider_id(&json!({})), None);
        assert_eq!(
            search_provider_id(&json!({"provider": "tavily"})).as_deref(),
            Some("tavily")
        );
        assert_eq!(
            search_provider_id(&json!({"model": "brave-search"})).as_deref(),
            Some("brave-search")
        );
    }

    #[test]
    fn query_encoding() {
        assert_eq!(urlencoding_encode("rust async"), "rust%20async");
    }
}
