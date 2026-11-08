//! HTTP plumbing shared by executors, the engine and the server.
//!
//! `proxy_fetch` is the port of open-sse/utils/proxyFetch.js (`proxyAwareFetch`).
//! The executor-base owner extends it (Vercel relay, proxy pools, strict
//! proxy, stale-socket replay) without changing these signatures.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use http::HeaderMap;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{Credentials, Error, Json, Result};

/// A response body as a byte stream (upstream bytes or a synthesized SSE stream).
pub type BodyStream = Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static>>;

/// Request headers in insertion order; keys case-sensitive like a JS object.
pub type HeaderList = IndexMap<String, String>;

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

/// One outbound HTTP request.
#[derive(Clone, Debug)]
pub struct FetchRequest {
    pub method: http::Method,
    pub url: String,
    pub headers: HeaderList,
    pub body: Option<Bytes>,
    /// Abort when response headers haven't arrived in time (JS connect timeout).
    /// `None` = no limit; executors pass `FETCH_CONNECT_TIMEOUT_MS` from nr-data.
    pub header_timeout: Option<Duration>,
    pub cancel: Option<CancellationToken>,
}

impl FetchRequest {
    pub fn new(method: http::Method, url: impl Into<String>) -> Self {
        Self { method, url: url.into(), headers: HeaderList::new(), body: None, header_timeout: None, cancel: None }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new(http::Method::GET, url)
    }

    /// POST with `JSON.stringify(body)`; `Content-Type` is added only when absent.
    pub fn post_json(url: impl Into<String>, mut headers: HeaderList, body: &Json) -> Self {
        if !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
            headers.insert("Content-Type".into(), "application/json".into());
        }
        Self { headers, body: Some(body.to_string().into()), ..Self::new(http::Method::POST, url) }
    }
}

static CLIENTS: LazyLock<Mutex<HashMap<(Option<String>, bool), reqwest::Client>>> = LazyLock::new(Default::default);

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

/// Port of `proxyAwareFetch`: send `req` through the connection's proxy (or
/// the environment proxy / direct), honoring cancellation and the header timeout.
pub async fn proxy_fetch(req: FetchRequest, opts: Option<&ProxyOptions>) -> Result<UpstreamResponse> {
    let proxy = opts.and_then(|o| o.proxy_for(&req.url));
    let client = client_for(proxy, opts.is_some_and(|o| o.force_fresh_socket))?;
    let mut rb = client.request(req.method.clone(), &req.url);
    for (k, v) in &req.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if let Some(body) = req.body.clone() {
        rb = rb.body(body);
    }
    let send = async {
        match req.header_timeout {
            Some(t) => tokio::time::timeout(t, rb.send()).await.map_err(|_| Error::Network("fetch connect timeout".into()))?.map_err(Error::from),
            None => rb.send().await.map_err(Error::from),
        }
    };
    let cancel = req.cancel.clone().unwrap_or_default();
    tokio::select! {
        _ = cancel.cancelled() => Err(Error::Aborted),
        r = send => r.map(UpstreamResponse::from_reqwest),
    }
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
