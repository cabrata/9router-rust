//! Port of open-sse/executors/base.js.
//!
//! A JS executor subclasses `BaseExecutor` and overrides `buildUrl`,
//! `buildHeaders`, `transformRequest`, …; here an executor implements
//! [`BaseHooks`] (overriding only what differs) and forwards
//! `Executor::execute` to [`base_execute`].

use std::time::Duration;

use async_trait::async_trait;
use http::HeaderMap;
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Json, JsonMap, Result};

/// Answer of [`BaseHooks::compute_retry_delay`] (JS returns a number, `null` or `false`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDelay {
    /// Use the configured delay (JS `null`/`undefined`).
    Default,
    /// Wait this long instead.
    Ms(u64),
    /// Don't retry; fall through to URL fallback (JS `false`).
    Veto,
}

fn shared_const(name: &str) -> &'static str {
    nr_data::export("open-sse/providers/shared", name).and_then(Json::as_str).unwrap_or_default()
}

/// `resolveOpenAICompatibleApiType` (open-sse/services/provider.js).
pub fn openai_compatible_api_type(provider: &str, creds: &Credentials) -> &'static str {
    match creds.psd_str("apiType") {
        Some("chat") => "chat",
        Some("responses") => "responses",
        _ if provider.contains("responses") => "responses",
        _ => "chat",
    }
}

/// The overridable surface of JS `BaseExecutor`.
#[async_trait]
pub trait BaseHooks: Send + Sync {
    /// JS `this.provider`.
    fn provider(&self) -> &str;

    /// JS `this.config`: the registry transport, `PROVIDERS[id]`.
    fn config(&self) -> &Json;

    /// JS `this.noAuth`.
    fn no_auth(&self) -> bool {
        self.config().get("noAuth").and_then(Json::as_bool).unwrap_or(false)
    }

    /// JS `getBaseUrls()`.
    fn base_urls(&self) -> Vec<String> {
        let c = self.config();
        if let Some(urls) = c.get("baseUrls").and_then(Json::as_array) {
            return urls.iter().filter_map(|u| u.as_str().map(String::from)).collect();
        }
        c.get("baseUrl").and_then(Json::as_str).map(|u| vec![u.to_string()]).unwrap_or_default()
    }

    /// JS `getFallbackCount()`.
    fn fallback_count(&self) -> usize {
        self.base_urls().len().max(1)
    }

    /// JS `buildUrl(model, stream, urlIndex, credentials)`.
    fn build_url(&self, _model: &str, _stream: bool, url_index: usize, creds: &Credentials) -> Result<String> {
        Ok(default_build_url(self, url_index, creds))
    }

    /// JS `buildHeaders(credentials, stream, url, model, body)`.
    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, _body: &Json) -> Result<HeaderList> {
        Ok(default_build_headers(self, creds, stream))
    }

    /// JS `transformRequest(model, body, stream, credentials)`.
    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        Ok(body.clone())
    }

    /// JS `shouldRetry(status, urlIndex)`: move on to the next base URL.
    fn should_retry(&self, status: u16, url_index: usize) -> bool {
        status == 429 && url_index + 1 < self.fallback_count()
    }

    /// JS `computeRetryDelay(response, attempt, delayMs)`. `body` is the
    /// buffered error body (JS reads `response.clone().text()`).
    async fn compute_retry_delay(&self, _status: u16, _headers: &HeaderMap, _body: &[u8], _attempt: u32, _delay_ms: u64) -> RetryDelay {
        RetryDelay::Default
    }

    /// Upstream must answer with headers within this (JS `config.timeoutMs || FETCH_CONNECT_TIMEOUT_MS`).
    fn header_timeout(&self) -> Duration {
        let ms = self.config().get("timeoutMs").and_then(Json::as_u64).unwrap_or_else(|| nr_data::runtime_u64("FETCH_CONNECT_TIMEOUT_MS"));
        Duration::from_millis(ms)
    }

    /// `{ ...DEFAULT_RETRY_CONFIG, ...this.config.retry }`, keyed by status code string.
    fn retry_config(&self) -> JsonMap {
        let mut cfg = nr_data::runtime("DEFAULT_RETRY_CONFIG").and_then(Json::as_object).cloned().unwrap_or_default();
        if let Some(own) = self.config().get("retry").and_then(Json::as_object) {
            cfg.extend(own.clone());
        }
        cfg
    }
}

/// Base-class `buildUrl` body, reusable from overrides.
pub fn default_build_url<H: BaseHooks + ?Sized>(h: &H, url_index: usize, creds: &Credentials) -> String {
    let provider = h.provider();
    if provider.starts_with("openai-compatible-") {
        let base = creds.psd_str("baseUrl").unwrap_or(shared_const("OPENAI_COMPAT_BASE"));
        let path = if openai_compatible_api_type(provider, creds) == "responses" { "/responses" } else { "/chat/completions" };
        return format!("{}{path}", base.strip_suffix('/').unwrap_or(base));
    }
    if provider.starts_with("anthropic-compatible-") {
        let base = creds.psd_str("baseUrl").unwrap_or(shared_const("ANTHROPIC_COMPAT_BASE"));
        return format!("{}/messages", base.strip_suffix('/').unwrap_or(base));
    }
    let urls = h.base_urls();
    urls.get(url_index)
        .or_else(|| urls.first())
        .cloned()
        .or_else(|| h.config().get("baseUrl").and_then(Json::as_str).map(String::from))
        .unwrap_or_default()
}

/// Base-class `buildHeaders` body, reusable from overrides.
pub fn default_build_headers<H: BaseHooks + ?Sized>(h: &H, creds: &Credentials, stream: bool) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    if let Some(cfg) = h.config().get("headers").and_then(Json::as_object) {
        for (k, v) in cfg {
            if let Some(v) = v.as_str() {
                headers.insert(k.clone(), v.to_string());
            }
        }
    }
    if h.provider().starts_with("anthropic-compatible-") {
        if let Some(key) = creds.api_key() {
            headers.insert("x-api-key".into(), key.to_string());
        } else if let Some(token) = creds.access_token() {
            headers.insert("Authorization".into(), format!("Bearer {token}"));
        }
        if !headers.contains_key("anthropic-version") {
            headers.insert("anthropic-version".into(), shared_const("ANTHROPIC_API_VERSION").to_string());
        }
    } else if let Some(token) = creds.bearer() {
        headers.insert("Authorization".into(), format!("Bearer {token}"));
    }
    if stream {
        headers.insert("Accept".into(), "text/event-stream".into());
    }
    headers
}

/// `resolveRetryEntry(entry)` → (attempts, delayMs).
pub fn resolve_retry_entry(entry: Option<&Json>) -> (u32, u64) {
    let default_delay = nr_data::runtime("RETRY_CONFIG").and_then(|c| c.get("delayMs")).and_then(Json::as_u64).unwrap_or(0);
    match entry {
        None | Some(Json::Null) => (0, default_delay),
        Some(Json::Number(n)) => (n.as_u64().unwrap_or(0) as u32, default_delay),
        Some(e) => (
            e.get("attempts").and_then(Json::as_u64).unwrap_or(0) as u32,
            e.get("delayMs").and_then(Json::as_u64).unwrap_or(default_delay),
        ),
    }
}

/// Merge `providerOverrides.headers` over the built headers (user override wins).
fn apply_override_headers(headers: &mut HeaderList, overrides: Option<&Json>) {
    if let Some(o) = overrides.and_then(|o| o.get("headers")).and_then(Json::as_object) {
        for (k, v) in o {
            if let Some(v) = v.as_str() {
                headers.insert(k.clone(), v.to_string());
            }
        }
    }
}

/// Port of `BaseExecutor.execute`: per-URL retry on configured statuses,
/// fresh-socket retry after 502/network errors, then URL fallback.
pub async fn base_execute<H: BaseHooks + ?Sized>(h: &H, req: ExecRequest<'_>) -> Result<ExecResult> {
    let fallback_count = h.fallback_count();
    let retry_cfg = h.retry_config();
    let mut attempts_by_url = vec![0u32; fallback_count];
    let mut fresh_socket = vec![false; fallback_count];
    let mut last_error: Option<Error> = None;
    let mut last_status = 0u16;

    let mut url_index = 0;
    while url_index < fallback_count {
        let url = h.build_url(req.model, req.stream, url_index, req.credentials)?;
        let body = h.transform_request(req.model, &req.body, req.stream, req.credentials)?;
        let mut headers = h.build_headers(req.credentials, req.stream, &url, req.model, &body)?;
        apply_override_headers(&mut headers, req.provider_overrides.as_ref());

        let mut proxy = req.proxy.clone();
        proxy.force_fresh_socket |= fresh_socket[url_index];
        let fetch = FetchRequest {
            header_timeout: Some(h.header_timeout()),
            cancel: Some(req.cancel.clone()),
            ..FetchRequest::post_json(url.clone(), headers.clone(), &body)
        };
        req.log.debug("FETCH", format!("{} → {url}", h.provider().to_uppercase()));

        match proxy_fetch(fetch, Some(&proxy)).await {
            Ok(response) => {
                req.log.debug("FETCH", format!("{} ← {}", h.provider().to_uppercase(), response.status));
                let status = response.status;
                let (max_attempts, delay_ms) = resolve_retry_entry(retry_cfg.get(&status.to_string()));
                let mut response = response;
                if max_attempts > 0 && attempts_by_url[url_index] < max_attempts {
                    // Buffer the error body so the hook can read it and the response survives a veto.
                    let (resp_status, resp_headers) = (response.status, response.headers.clone());
                    let bytes = response.bytes().await.unwrap_or_default();
                    let wait = match h.compute_retry_delay(status, &resp_headers, &bytes, attempts_by_url[url_index] + 1, delay_ms).await {
                        RetryDelay::Default => Some(delay_ms),
                        RetryDelay::Ms(ms) => Some(ms),
                        RetryDelay::Veto => None,
                    };
                    response = UpstreamResponse::from_bytes(resp_status, resp_headers, bytes);
                    if let Some(wait) = wait {
                        attempts_by_url[url_index] += 1;
                        req.log.debug("RETRY", format!("status {status} retry {}/{max_attempts} after {}s", attempts_by_url[url_index], wait as f64 / 1000.0));
                        if status == 502 {
                            // A 502 from an edge is typically a poisoned keep-alive socket.
                            fresh_socket[url_index] = true;
                        }
                        sleep_or_cancel(wait, &req.cancel).await?;
                        continue;
                    }
                }
                if h.should_retry(status, url_index) {
                    req.log.debug("RETRY", format!("{status} on {url}, trying fallback {}", url_index + 1));
                    last_status = status;
                    url_index += 1;
                    continue;
                }
                return Ok(ExecResult { response, url, headers, transformed_body: body, response_format: None });
            }
            Err(Error::Aborted) => return Err(Error::Aborted),
            Err(err) => {
                req.log.debug("FETCH", format!("{} ✖ {err}", h.provider().to_uppercase()));
                // Network failures use the 502 retry entry, always on a fresh socket.
                let (max_attempts, delay_ms) = resolve_retry_entry(retry_cfg.get("502"));
                if max_attempts > 0 && attempts_by_url[url_index] < max_attempts {
                    attempts_by_url[url_index] += 1;
                    fresh_socket[url_index] = true;
                    req.log.debug("RETRY", format!("network \"{err}\" retry {}/{max_attempts} after {}s", attempts_by_url[url_index], delay_ms as f64 / 1000.0));
                    sleep_or_cancel(delay_ms, &req.cancel).await?;
                    continue;
                }
                if url_index + 1 < fallback_count {
                    req.log.debug("RETRY", format!("Error on {url}, trying fallback {}", url_index + 1));
                    last_error = Some(err);
                    url_index += 1;
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Network(format!("All {fallback_count} URLs failed with status {last_status}"))))
}

async fn sleep_or_cancel(ms: u64, cancel: &tokio_util::sync::CancellationToken) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(Error::Aborted),
        _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct T(Json);
    impl BaseHooks for T {
        fn provider(&self) -> &str {
            "openai-compatible-x"
        }
        fn config(&self) -> &Json {
            &self.0
        }
    }

    #[test]
    fn urls_and_headers() {
        let h = T(json!({"baseUrl": "https://a/v1/chat/completions", "headers": {"User-Agent": "ua"}}));
        let c = Credentials::from_value(json!({"apiKey": "k", "providerSpecificData": {"baseUrl": "https://n/v1/"}}));
        assert_eq!(default_build_url(&h, 0, &c), "https://n/v1/chat/completions");
        let hd = default_build_headers(&h, &c, true);
        assert_eq!(hd["Authorization"], "Bearer k");
        assert_eq!(hd["User-Agent"], "ua");
        assert_eq!(hd["Accept"], "text/event-stream");
    }

    #[test]
    fn retry_entries() {
        assert_eq!(resolve_retry_entry(Some(&json!({"attempts": 3, "delayMs": 5}))), (3, 5));
        assert_eq!(resolve_retry_entry(None).0, 0);
        assert_eq!(resolve_retry_entry(Some(&json!(2))).0, 2);
    }
}
