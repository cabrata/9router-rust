//! HTTP plumbing shared by executors, the engine and the server.
//!
//! `proxy_fetch` is the port of open-sse/utils/proxyFetch.js (`proxyAwareFetch`).
//! The executor-base owner extends it (Vercel relay, proxy pools, strict
//! proxy, stale-socket replay) without changing these signatures.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use http::HeaderMap;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{Credentials, Error, Json, Result};

// Constants from open-sse/config/runtimeConfig.js
#[allow(dead_code)]
const MEMORY_CONFIG_DNS_CACHE_TTL_MS: u64 = 5 * 60 * 1000; // 5 minutes
#[allow(dead_code)]
const MEMORY_CONFIG_PROXY_DISPATCHERS_MAX_SIZE: usize = 20;
#[allow(dead_code)]
const FRESH_RETRY_KEEPALIVE_TIMEOUT_MS: u64 = 1;
#[allow(dead_code)]
const DEFAULT_KEEPALIVE_TIMEOUT_MS: u64 = 4 * 1000;

// TLS certificate error codes from proxyFetch.js
#[allow(dead_code)]
const TLS_CERT_ERRORS: &[&str] = &[
    "SELF_SIGNED_CERT_IN_CHAIN",
    "DEPTH_ZERO_SELF_SIGNED_CERT",
    "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
    "UNABLE_TO_GET_ISSUER_CERT",
    "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
    "CERT_HAS_EXPIRED",
    "ERR_TLS_CERT_ALTNAME_INVALID",
];

// MITM bypass hosts from proxyFetch.js
#[allow(dead_code)]
const MITM_BYPASS_HOSTS: &[&str] = &[
    "cloudcode-pa.googleapis.com",
    "daily-cloudcode-pa.googleapis.com",
    "api.individual.githubcopilot.com",
    "q.us-east-1.amazonaws.com",
    "codewhisperer.us-east-1.amazonaws.com",
    "api2.cursor.sh",
];

// Stale socket error codes (Node.js/undici specific)
#[allow(dead_code)]
static STALE_SOCKET_ERROR_CODES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "UND_ERR_SOCKET",
        "ECONNRESET",
        "EPIPE",
        "UND_ERR_HEADERS_TIMEOUT",
    ].into_iter().collect()
});

// Non-stale socket error codes (DNS, connect, etc.)
#[allow(dead_code)]
static NON_STALE_SOCKET_ERROR_CODES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "ENOTFOUND",
        "EAI_AGAIN",
        "ECONNREFUSED",
        "EHOSTUNREACH",
        "ENETUNREACH",
        "UND_ERR_CONNECT_TIMEOUT",
        "UND_ERR_INVALID_ARG",
    ].into_iter().collect()
});

// Stale socket error substrings
#[allow(dead_code)]
const STALE_SOCKET_ERROR_SUBSTRINGS: &[&str] = &[
    "other side closed",
    "socket hang up",
];

/// A response body as a byte stream (upstream bytes or a synthesized SSE stream).
pub type BodyStream = Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static>>;

/// Request headers in insertion order; keys case-sensitive like a JS object.
pub type HeaderList = IndexMap<String, String>;

/// DNS cache entry with TTL.
#[allow(dead_code)]
struct DnsCacheEntry {
    ip: String,
    expiry: SystemTime,
}

/// DNS resolution cache (hostname → IP).
#[allow(dead_code)]
static DNS_CACHE: LazyLock<Mutex<HashMap<String, DnsCacheEntry>>> = LazyLock::new(Default::default);

/// An upstream response: the Rust stand-in for the JS `Response` object.
/// Executors that decode binary protocols (kiro, cursor, …) build one with a
/// synthesized body via [`UpstreamResponse::from_stream`] / [`UpstreamResponse::sse`].
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BodyStream,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse").field("status", &self.status).field("headers", &self.headers).finish_non_exhaustive()
    }
}

impl UpstreamResponse {
    pub fn from_reqwest(r: reqwest::Response) -> Self {
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let body = Box::pin(r.bytes_stream().map_err(std::io::Error::other));
        Self { status, headers, body }
    }

    pub fn from_bytes(status: u16, headers: HeaderMap, body: impl Into<Bytes>) -> Self {
        let body: Bytes = body.into();
        Self { status, headers, body: Box::pin(futures::stream::once(async move { Ok(body) })) }
    }

    pub fn from_stream(status: u16, headers: HeaderMap, body: BodyStream) -> Self {
        Self { status, headers, body }
    }

    /// `new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } })`.
    pub fn json(status: u16, value: &Json) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
        Self::from_bytes(status, headers, value.to_string())
    }

    /// A 200 `text/event-stream` response over a synthesized SSE byte stream.
    pub fn sse(body: BodyStream) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
        Self { status: 200, headers, body }
    }

    /// `response.ok`.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }

    pub async fn bytes(self) -> Result<Bytes> {
        let chunks: Vec<Bytes> = self.body.try_collect().await?;
        Ok(chunks.concat().into())
    }

    /// `await response.text()` (lossy UTF-8).
    pub async fn text(self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes().await?).into_owned())
    }

    /// `await response.json()`.
    pub async fn json_body(self) -> Result<Json> {
        Ok(serde_json::from_slice(&self.bytes().await?)?)
    }
}

/// Pool resolver hook: maps pool_id → proxy URL.
pub type ProxyPoolResolver = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

static PROXY_POOL_RESOLVER: LazyLock<Mutex<Option<std::sync::Arc<ProxyPoolResolver>>>> = LazyLock::new(Default::default);

/// Register a proxy pool resolver for resolving pool IDs to URLs.
pub fn set_proxy_pool_resolver(resolver: std::sync::Arc<ProxyPoolResolver>) {
    *PROXY_POOL_RESOLVER.lock().expect("pool resolver lock poisoned") = Some(resolver);
}

/// Per-connection outbound proxy settings (built in chatCore from `providerSpecificData`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyOptions {
    pub connection_proxy_enabled: bool,
    pub connection_proxy_url: String,
    pub connection_no_proxy: String,
    pub vercel_relay_url: String,
    pub proxy_pool_id: Option<String>,
    /// Strict pools must never silently fall back to a direct connection.
    pub strict_proxy: bool,
    /// Bypass the keep-alive pool (retry after a poisoned socket / 502).
    pub force_fresh_socket: bool,
}

impl ProxyOptions {
    /// The `proxyOptions` object chatCore builds from `credentials.providerSpecificData`.
    pub fn from_credentials(c: &Credentials) -> Self {
        let psd = c.psd();
        let s = |k: &str| psd.and_then(|m| m.get(k)).and_then(Json::as_str).unwrap_or("").to_string();
        let b = |k: &str| psd.and_then(|m| m.get(k)) == Some(&Json::Bool(true));
        let pool = s("connectionProxyPoolId");
        Self {
            connection_proxy_enabled: b("connectionProxyEnabled"),
            connection_proxy_url: s("connectionProxyUrl"),
            connection_no_proxy: s("connectionNoProxy"),
            vercel_relay_url: s("vercelRelayUrl"),
            proxy_pool_id: (!pool.is_empty()).then_some(pool),
            strict_proxy: b("strictProxy"),
            force_fresh_socket: false,
        }
    }

    /// Explicit proxy URL for `url`, honoring `connectionNoProxy` (comma-separated hosts/suffixes).
    pub fn proxy_for(&self, url: &str) -> Option<&str> {
        if !self.connection_proxy_enabled || self.connection_proxy_url.is_empty() {
            return None;
        }
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
        let bypass = self.connection_no_proxy.split(',').map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase()).filter(|s| !s.is_empty()).any(|p| p == "*" || host == p || host.ends_with(&format!(".{p}")));
        (!bypass).then_some(self.connection_proxy_url.as_str())
    }
}

/// Optional direct fetch function bypassing proxy logic. Called only when no proxy
/// is configured (proxy resolution yields None). Allows protocol adapters to override
/// the standard HTTP/1 transport.
pub type DirectFetch = Box<dyn Fn(&str, &FetchRequest) -> futures::future::BoxFuture<'static, Result<UpstreamResponse>> + Send + Sync>;

/// One outbound HTTP request.
pub struct FetchRequest {
    pub method: http::Method,
    pub url: String,
    pub headers: HeaderList,
    pub body: Option<Bytes>,
    /// Abort when response headers haven't arrived in time (JS connect timeout).
    /// `None` = no limit; executors pass `FETCH_CONNECT_TIMEOUT_MS` from nr-data.
    pub header_timeout: Option<Duration>,
    pub cancel: Option<CancellationToken>,
    /// Optional direct fetch override (bypasses proxy, used for protocol adapters).
    pub direct_fetch: Option<std::sync::Arc<DirectFetch>>,
}

impl std::fmt::Debug for FetchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .field("header_timeout", &self.header_timeout)
            .field("cancel", &"<CancellationToken>")
            .field("direct_fetch", &self.direct_fetch.as_ref().map(|_| "<DirectFetch>"))
            .finish_non_exhaustive()
    }
}

impl FetchRequest {
    pub fn new(method: http::Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: HeaderList::new(),
            body: None,
            header_timeout: None,
            cancel: None,
            direct_fetch: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new(http::Method::GET, url)
    }

    /// POST with `JSON.stringify(body)`; `Content-Type` is added only when absent.
    pub fn post_json(url: impl Into<String>, mut headers: HeaderList, body: &Json) -> Self {
        if !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
            headers.insert("Content-Type".into(), "application/json".into());
        }
        let mut req = Self::new(http::Method::POST, url);
        req.headers = headers;
        req.body = Some(body.to_string().into());
        req
    }
}

static CLIENTS: LazyLock<Mutex<HashMap<(Option<String>, bool), reqwest::Client>>> = LazyLock::new(Default::default);

/// Check if error is a TLS certificate error (unrecoverable).
#[allow(dead_code)]
fn is_tls_cert_error(err: &Error) -> bool {
    let msg = format!("{:?}", err).to_lowercase();
    TLS_CERT_ERRORS.iter().any(|code| msg.contains(&code.to_lowercase()))
}

/// Check if error looks like a stale socket (connection died cleanly before headers).
#[allow(dead_code)]
fn is_stale_socket_error(err: &Error) -> bool {
    let msg = format!("{:?}", err).to_lowercase();

    // Check for specific stale socket error codes/substrings
    if STALE_SOCKET_ERROR_CODES.iter().any(|code| msg.contains(code)) {
        return true;
    }

    if STALE_SOCKET_ERROR_SUBSTRINGS.iter().any(|sub| msg.contains(sub)) {
        return true;
    }

    false
}

/// Check if hostname matches no-proxy pattern (case-insensitive).
fn should_bypass_by_no_proxy(url: &str, no_proxy: &str) -> bool {
    if no_proxy.trim().is_empty() {
        return false;
    }

    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };

    let hostname = match parsed.host_str() {
        Some(h) => h.to_lowercase(),
        None => return false,
    };

    no_proxy
        .split(',')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .any(|pattern| {
            if pattern == "*" {
                return true;
            }
            let pattern = pattern.trim_start_matches('.');
            hostname == pattern || hostname.ends_with(&format!(".{pattern}"))
        })
}

/// Get proxy URL from environment (HTTP(S)_PROXY, ALL_PROXY, NO_PROXY).
fn get_env_proxy_url(url: &str) -> Option<String> {
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();

    if should_bypass_by_no_proxy(url, &no_proxy) {
        return None;
    }

    let Ok(parsed) = url::Url::parse(url) else {
        return None;
    };

    let proxy_var = if parsed.scheme() == "https" {
        std::env::var("HTTPS_PROXY")
            .or_else(|_| std::env::var("https_proxy"))
            .or_else(|_| std::env::var("ALL_PROXY"))
            .or_else(|_| std::env::var("all_proxy"))
    } else {
        std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("ALL_PROXY"))
            .or_else(|_| std::env::var("all_proxy"))
    };

    proxy_var.ok().and_then(|p| {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Normalize proxy URL (allow "host:port" style).
fn normalize_proxy_url(proxy_url: &str) -> Option<String> {
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try parsing as-is
    if url::Url::parse(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Allow "127.0.0.1:7890" style
    Some(format!("http://{trimmed}"))
}

/// Cached client per (explicit proxy, fresh-socket). Without an explicit proxy,
/// reqwest honors HTTP(S)_PROXY / ALL_PROXY / NO_PROXY from the environment.
pub fn client_for(proxy: Option<&str>, fresh_socket: bool) -> Result<reqwest::Client> {
    let key = (proxy.map(str::to_string), fresh_socket);
    if let Some(c) = CLIENTS.lock().expect("client cache poisoned").get(&key) {
        return Ok(c.clone());
    }
    let mut b = reqwest::Client::builder();
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| Error::BadRequest(format!("invalid proxy url: {e}")))?);
    }
    if fresh_socket {
        b = b.pool_max_idle_per_host(0);
    }
    let client = b.build().map_err(|e| Error::other(format!("http client: {e}")))?;
    CLIENTS.lock().expect("client cache poisoned").insert(key, client.clone());
    Ok(client)
}

/// Implementation: send request with optional proxy, retry on stale socket.
async fn proxy_fetch_impl(req: &FetchRequest, proxy: Option<&str>) -> Result<UpstreamResponse> {
    let client = client_for(proxy, false)?;
    let mut rb = client.request(req.method.clone(), &req.url);

    for (k, v) in &req.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }

    if let Some(body) = &req.body {
        rb = rb.body(body.clone());
    }

    let send = async {
        match req.header_timeout {
            Some(t) => tokio::time::timeout(t, rb.send())
                .await
                .map_err(|_| Error::Network("fetch connect timeout".into()))?
                .map_err(Error::from),
            None => rb.send().await.map_err(Error::from),
        }
    };

    let cancel = req.cancel.clone().unwrap_or_default();
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(Error::Aborted),
        r = send => r.map(UpstreamResponse::from_reqwest),
    };

    // Stale socket retry: replay once on fresh socket if error looks like dead keep-alive
    if let Err(ref e) = result {
        if proxy.is_none()
            && req.direct_fetch.is_none()
            && is_stale_socket_error(e)
            && req.body.as_ref().map(|b| !b.is_empty()).unwrap_or(true)
        {
            let client_fresh = client_for(None, true)?;
            let mut rb_fresh = client_fresh.request(req.method.clone(), &req.url);

            for (k, v) in &req.headers {
                rb_fresh = rb_fresh.header(k.as_str(), v.as_str());
            }

            if let Some(body) = &req.body {
                rb_fresh = rb_fresh.body(body.clone());
            }

            let cancel = req.cancel.clone().unwrap_or_default();
            return tokio::select! {
                _ = cancel.cancelled() => Err(Error::Aborted),
                r = rb_fresh.send() => r.map_err(Error::from).map(UpstreamResponse::from_reqwest),
            };
        }
    }

    result
}

/// Port of `proxyAwareFetch`: send `req` through connection proxy, env proxy, or direct.
/// Handles Vercel relay, strict proxy enforcement, stale socket replay, and TLS fallback.
pub async fn proxy_fetch(req: FetchRequest, opts: Option<&ProxyOptions>) -> Result<UpstreamResponse> {
    let url = &req.url;

    // Vercel relay: forward via relay headers instead of direct proxy
    if let Some(opts) = opts {
        if !opts.vercel_relay_url.is_empty() {
            if let Ok(target_parsed) = url::Url::parse(url) {
                let mut relay_headers = req.headers.clone();
                relay_headers.insert(
                    "x-relay-target".into(),
                    format!("{}://{}", target_parsed.scheme(), target_parsed.host_str().unwrap_or(""))
                );
                relay_headers.insert(
                    "x-relay-path".into(),
                    format!("{}{}", target_parsed.path(), target_parsed.query().map(|q| format!("?{q}")).unwrap_or_default())
                );

                let mut relay_req = FetchRequest::new(req.method.clone(), &opts.vercel_relay_url);
                relay_req.headers = relay_headers;
                relay_req.body = req.body.clone();
                relay_req.header_timeout = req.header_timeout;
                relay_req.cancel = req.cancel.clone();

                return proxy_fetch_impl(&relay_req, None).await;
            }
        }
    }

    // Resolve proxy: connection-specific, pool, or env
    let connection_proxy = opts.and_then(|o| {
        if !o.connection_proxy_enabled || o.connection_proxy_url.is_empty() {
            return None;
        }
        if should_bypass_by_no_proxy(url, &o.connection_no_proxy) {
            return None;
        }
        normalize_proxy_url(&o.connection_proxy_url)
    });

    let pool_proxy = opts.and_then(|o| {
        o.proxy_pool_id.as_ref().and_then(|pool_id| {
            PROXY_POOL_RESOLVER.lock().expect("pool resolver poisoned").as_ref()
                .and_then(|resolver| resolver(pool_id))
        })
    });

    let env_proxy = if connection_proxy.is_none() && pool_proxy.is_none() {
        get_env_proxy_url(url)
    } else {
        None
    };

    let proxy_url = connection_proxy.or(pool_proxy).or(env_proxy);

    // Try proxy if configured
    if let Some(ref proxy) = proxy_url {
        match proxy_fetch_impl(&req, Some(proxy.as_str())).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                if opts.is_some_and(|o| o.strict_proxy) {
                    return Err(Error::BadRequest(format!("proxy required but failed (strictProxy=true): {e}")));
                }
                // Fall through to direct attempt
            }
        }
    }

    // Strict mode: never fall back to direct if proxy was intended
    if let Some(opts) = opts {
        if opts.strict_proxy {
            let proxy_intended = opts.proxy_pool_id.is_some()
                || !opts.connection_proxy_url.is_empty()
                || !opts.vercel_relay_url.is_empty();
            if proxy_intended {
                return Err(Error::BadRequest("[ProxyFetch] Proxy required but none resolved (strictProxy=true)".into()));
            }
        }
    }

    // Fallback: direct (or fresh socket retry if stale socket error)
    proxy_fetch_impl(&req, None).await
}

/// Collect an SSE byte stream into `data:` payload lines — handy in tests.
pub async fn collect_sse_data(mut body: BodyStream) -> Result<Vec<String>> {
    let mut buf = String::new();
    while let Some(chunk) = body.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
    }
    Ok(buf.lines().filter_map(|l| l.strip_prefix("data:")).map(|l| l.trim().to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn proxy_selection_honors_no_proxy() {
        let c = Credentials::from_value(json!({"providerSpecificData": {"connectionProxyEnabled": true, "connectionProxyUrl": "http://p:1", "connectionNoProxy": "internal.example, .corp"}}));
        let o = ProxyOptions::from_credentials(&c);
        assert_eq!(o.proxy_for("https://api.openai.com/v1"), Some("http://p:1"));
        assert_eq!(o.proxy_for("https://x.internal.example/"), None);
        assert_eq!(o.proxy_for("https://a.corp/"), None);
        assert_eq!(ProxyOptions::default().proxy_for("https://a/"), None);
    }

    #[tokio::test]
    async fn response_helpers() {
        let r = UpstreamResponse::json(201, &json!({"a": 1}));
        assert!(r.ok());
        assert_eq!(r.content_type(), "application/json");
        assert_eq!(r.json_body().await.unwrap(), json!({"a": 1}));
    }
}
