//! Port of open-sse/executors/xiaomi-mimo.js.
//!
//! Dual-route v2.6 models: account-service route (with session cookie) vs cloud API (with sk- key).
//! The account-session cookie is acquired via Xiaomi's 2-phase serviceLogin handshake
//! (open-sse/shared/mimoAccount.js), keyed on providerSpecificData.mimoPassToken.

use std::sync::{Arc, LazyLock, Mutex};
use std::collections::HashMap;

use async_trait::async_trait;
use base64::{engine::general_purpose, Engine};
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList};
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Credentials};
use nr_exec_base::default_executor::DefaultExecutor;
use nr_exec_base::base::{BaseHooks};
use sha1::{Digest, Sha1};

const COOKIE_KEY: &str = "__mimoAccountCookie";

/// V2.6 models that support dual-route account service.
const ACCOUNT_MODELS: &[&str] = &[
    "mimo-v2.6-pro",
    "mimo-v2.6-flash",
    "mimo-v2.6-pro-ultraspeed",
];

/// Account-service cluster hosts. MiMo Desktop declares five regions
/// (cn/sgp/ams/ru/in); the EU cluster is deployed in Amsterdam.
const API_BASE_BY_REGION: &[(&str, &str)] = &[
    ("cn", "https://mimo-server-cn.xiaomimimo.com"),
    ("sgp", "https://mimo-server-sgp.xiaomimimo.com"),
    ("ams", "https://mimo-server-ams.xiaomimimo.com"),
    ("ru", "https://mimo-server-ru.xiaomimimo.com"),
    ("in", "https://mimo-server-in.xiaomimimo.com"),
];

const DEFAULT_API_BASE: &str = "https://mimo-server-sgp.xiaomimimo.com";
const ACCOUNT_HOST: &str = "account.xiaomi.com";
const MIMO_API_UA: &str = "miNative PC/Normal Windows_NT/10.0.19045 SDKV/1.0.0 DEVT/PC DEVS/Windows APP/miaccount_desktop APPV/0.1.0";
const SSO_UA: &str = "MiClaw/1.0";
const COOKIE_TTL_MS: i64 = 30 * 60 * 1000;

/// Per-passToken+cluster cache of acquired cookies.
static COOKIE_CACHE: LazyLock<Mutex<HashMap<String, (String, i64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Strip provider prefix from model id (e.g., "xiaomi-mimo/mimo-v2.6-pro" → "mimo-v2.6-pro").
fn bare_model(model: &str) -> &str {
    if let Some(idx) = model.find('/') {
        &model[idx + 1..]
    } else {
        model
    }
}

/// Check if a model is an account-route model and has account credentials.
fn is_account_route(model: &str, credentials: &Credentials) -> bool {
    let bare = bare_model(model);
    if !ACCOUNT_MODELS.contains(&bare) {
        return false;
    }
    credentials.get(COOKIE_KEY).is_some()
        || credentials.psd_str("mimoPassToken").is_some()
}

/// Resolve the account-service base URL for a connection.
fn resolve_mimo_server_base(creds: &Credentials) -> String {
    let region = creds.psd_str("region").unwrap_or("").to_lowercase();
    API_BASE_BY_REGION
        .iter()
        .find(|(code, _)| *code == region)
        .map(|(_, base)| base.to_string())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// Cluster service sid — 1:1 with the host code: mimo<code>.
fn sid_for_region(region: &str) -> &'static str {
    match region.to_lowercase().as_str() {
        "cn" => "mimopc",
        "sgp" => "mimosgp",
        "ams" => "mimoams",
        "ru" => "mimoru",
        "in" => "mimoin",
        _ => "mimosgp",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cookie_header(jar: &HashMap<String, String>) -> String {
    jar.iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// sha1(nonce [& ssecurity]) as URL-encoded base64 — Xiaomi's clientSign.
fn signature_client_sign(nonce: &str, ssecurity: &str) -> String {
    let input = if ssecurity.trim().is_empty() {
        format!("nonce={nonce}")
    } else {
        format!("nonce={nonce}&{}", ssecurity.trim())
    };
    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());
    let b64 = general_purpose::STANDARD.encode(hasher.finalize());
    // URL-encode
    let mut out = String::new();
    for byte in b64.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Absorb `Set-Cookie` headers from an upstream response into the cookie jar.
fn absorb_set_cookie(jar: &mut HashMap<String, String>, headers: &http::HeaderMap) {
    let values: Vec<&str> = headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    for set in values {
        if let Some(first) = set.split(';').next() {
            let first = first.trim();
            if let Some((k, v)) = first.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                if !v.is_empty() {
                    jar.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
}

/// Drop cached account cookies so the next call re-runs the handshake (e.g. after a 401).
fn invalidate_cookie_cache() {
    if let Ok(mut cache) = COOKIE_CACHE.lock() {
        cache.clear();
    }
}

/// Exchange a passToken for a mimo-server service-session cookie (2-phase SSO).
///
/// PHASE 1: GET /pass/serviceLogin?_locale=zh_CN&_snsNone=true&sid=<clusterSid>&_json=true
///          Cookie {userId, passToken, cUserId} → {code, location, ssecurity, nonce}
/// PHASE 2: GET {location}&clientSign=sha1(nonce&ssecurity), follow the chain
///          (manual, absorbing Set-Cookie) → serviceToken
/// Phase 2 MUST NOT send any Cookie header (app.asar / SSO_curl.cpp line 728).
async fn acquire_service_cookie(
    pass_jar: &HashMap<String, String>,
    proxy: Option<&nr_core::http::ProxyOptions>,
    _api_base: &str,
    region: &str,
) -> Option<String> {
    // CN is ALWAYS direct (ignores proxy even if set).
    let effective_proxy: Option<&nr_core::http::ProxyOptions> = if region == "cn" { None } else { proxy };
    let sid = sid_for_region(region);
    let mut jar = pass_jar.clone();

    // PHASE 1 — single serviceLogin call with the TARGET sid.
    let mut p1_jar: HashMap<String, String> = HashMap::new();
    if let Some(uid) = jar.get("userId") { p1_jar.insert("userId".into(), uid.clone()); }
    if let Some(pt) = jar.get("passToken") { p1_jar.insert("passToken".into(), pt.clone()); }
    if let Some(cuid) = jar.get("cUserId") { p1_jar.insert("cUserId".into(), cuid.clone()); }

    let p1_url = format!(
        "https://{ACCOUNT_HOST}/pass/serviceLogin?_locale=zh_CN&_snsNone=true&sid={sid}&_json=true"
    );
    let mut p1_headers = HeaderList::new();
    p1_headers.insert("Cookie".into(), cookie_header(&p1_jar));
    p1_headers.insert("User-Agent".into(), SSO_UA.to_string());
    p1_headers.insert("Accept".into(), "application/json".to_string());

    let mut p1_req = FetchRequest::get(p1_url);
    p1_req.headers = p1_headers;
    let p1 = proxy_fetch(p1_req, effective_proxy).await.ok()?;
    // `bytes()` consumes the response; capture the cookie headers first.
    let cookie_headers = p1.headers.clone();
    let raw = String::from_utf8_lossy(&p1.bytes().await.ok()?).to_string();
    // Xiaomi prefixes the JSON body with `&&&START&&&`.
    let clean = raw.strip_prefix("&&&START&&&").unwrap_or(&raw);
    let j: Json = serde_json::from_str(clean).ok()?;

    let code = j.get("code").and_then(Json::as_i64).unwrap_or(-1);
    let location = j.get("location").and_then(Json::as_str).unwrap_or("");
    let ssecurity = j.get("ssecurity").and_then(Json::as_str).unwrap_or("");
    let nonce = j.get("nonce").and_then(Json::as_str).unwrap_or("");
    if code != 0 || location.is_empty() || ssecurity.is_empty() || nonce.is_empty() {
        return None;
    }

    absorb_set_cookie(&mut jar, &cookie_headers);

    // PHASE 2 — clientSign the redirect, follow the redirect chain server-side.
    let sep = if location.contains('?') { "&" } else { "?" };
    let mut current = format!("{location}{sep}clientSign={}", signature_client_sign(nonce, ssecurity));

    for _ in 0..8 {
        let mut hop_headers = HeaderList::new();
        hop_headers.insert("User-Agent".into(), SSO_UA.to_string());
        let mut hop_req = FetchRequest::get(current.clone());
        hop_req.headers = hop_headers;
        let res = proxy_fetch(hop_req, effective_proxy).await.ok()?;
        absorb_set_cookie(&mut jar, &res.headers);
        let loc = res.headers.get("location").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        if res.status >= 300 && res.status < 400 {
            if let Some(loc) = loc {
                current = resolve_url(&current, &loc);
                continue;
            }
        }
        break;
    }

    let sid_key = format!("{sid}_serviceToken");
    if !jar.contains_key("serviceToken") {
        if let Some(st) = jar.get(&sid_key) {
            jar.insert("serviceToken".into(), st.clone());
        }
    }
    let service_token = jar.get("serviceToken")?;
    if service_token.is_empty() {
        return None;
    }

    let mut out: HashMap<String, String> = HashMap::new();
    for (k, v) in &jar {
        if v.is_empty() {
            continue;
        }
        if k == "serviceToken" || k == "userId" || k.ends_with("_ph") || k.ends_with("_slh") {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(cookie_header(&out))
}

/// Minimal `new URL(loc, current)` replacement: absolute loc wins, else join to current.
fn resolve_url(current: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    if loc.starts_with('/') {
        let base_end = current.find("://").map(|i| current[i + 3..].find('/').map(|j| i + 3 + j).unwrap_or(current.len()));
        if let Some(idx) = base_end {
            return format!("{}{}", &current[..idx], loc);
        }
        return format!("{current}{loc}");
    }
    current.trim_end_matches('/').to_string() + "/" + loc
}

/// Get (and cache) the mimo-server account cookie keyed on passToken hash + region.
async fn get_mimo_account_cookie(creds: &Credentials, proxy: Option<&nr_core::http::ProxyOptions>) -> Option<String> {
    let pass_token = creds.psd_str("mimoPassToken")?;
    let region = creds.psd_str("region").unwrap_or("sgp").to_lowercase();
    let api_base = resolve_mimo_server_base(creds);

    let key = format!("{}|{}", api_base, pass_token);
    {
        let cache = COOKIE_CACHE.lock().unwrap();
        if let Some((cookie, at)) = cache.get(&key) {
            if now_ms() - *at < COOKIE_TTL_MS {
                return Some(cookie.clone());
            }
        }
    }

    let mut pass_jar: HashMap<String, String> = HashMap::new();
    pass_jar.insert("passToken".into(), pass_token.to_string());
    if let Some(uid) = creds.psd_str("mimoUserId") {
        pass_jar.insert("userId".into(), uid.to_string());
    }
    if let Some(cuid) = creds.psd_str("mimoCUserId") {
        pass_jar.insert("cUserId".into(), cuid.to_string());
    }

    let cookie = acquire_service_cookie(&pass_jar, proxy, &api_base, &region).await?;
    let mut cache = COOKIE_CACHE.lock().unwrap();
    cache.insert(key, (cookie.clone(), now_ms()));
    Some(cookie)
}

struct XiaomiMimoExecutor {
    default: DefaultExecutor,
}

impl XiaomiMimoExecutor {
    fn new(provider: &str) -> Self {
        Self {
            default: DefaultExecutor::new(provider),
        }
    }
}

impl BaseHooks for XiaomiMimoExecutor {
    fn provider(&self) -> &str {
        BaseHooks::provider(&self.default)
    }

    fn config(&self) -> &Json {
        self.default.config()
    }

    fn base_urls(&self) -> Vec<String> {
        self.default.base_urls()
    }

    fn build_url(&self, model: &str, stream: bool, url_index: usize, creds: &Credentials) -> Result<String> {
        if is_account_route(model, creds) {
            let base = resolve_mimo_server_base(creds);
            return Ok(format!("{base}/api/route/chat/completions"));
        }
        self.default.build_url(model, stream, url_index, creds)
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, url: &str, model: &str, body: &Json) -> Result<HeaderList> {
        if is_account_route(model, creds) {
            if let Some(cookie) = creds.get(COOKIE_KEY).and_then(Json::as_str) {
                let mut headers = HeaderList::new();
                headers.insert("Content-Type".into(), "application/json".into());
                headers.insert("Accept".into(), if stream { "text/event-stream".into() } else { "application/json".into() });
                headers.insert("User-Agent".into(), MIMO_API_UA.to_string());
                headers.insert("Cookie".into(), cookie.to_string());
                return Ok(headers);
            }
        }
        self.default.build_headers(creds, stream, url, model, body)
    }

    fn transform_request(&self, model: &str, body: &Json, stream: bool, creds: &Credentials) -> Result<Json> {
        let mut out = self.default.transform_request(model, body, stream, creds)?;

        if is_account_route(model, creds) {
            if let Some(obj) = out.as_object_mut() {
                let raw_effort = obj
                    .get("reasoning_effort")
                    .and_then(Json::as_str)
                    .or_else(|| body.get("reasoning_effort").and_then(Json::as_str))
                    .or_else(|| body.get("output_config").and_then(|c| c.get("effort")).and_then(Json::as_str))
                    .map(String::from);

                if let Some(effort) = raw_effort {
                    obj.remove("reasoning_effort");
                    let norm = if effort.to_lowercase() == "xhigh" { "high" } else { &effort.to_lowercase() };
                    let mut output_config = obj.get("output_config").and_then(Json::as_object).cloned().unwrap_or_default();
                    output_config.insert("effort".into(), Json::String(norm.to_string()));
                    obj.insert("output_config".into(), Json::Object(output_config));
                }

                if obj.get("temperature").is_none() || obj.get("temperature") == Some(&Json::Null) {
                    obj.insert("temperature".into(), Json::Number(serde_json::Number::from_f64(1.0).unwrap_or(serde_json::Number::from(0))));
                }
                if obj.get("top_p").is_none() || obj.get("top_p") == Some(&Json::Null) {
                    obj.insert("top_p".into(), Json::Number(serde_json::Number::from_f64(0.95).unwrap_or(serde_json::Number::from(0))));
                }
            }
        }

        Ok(out)
    }
}

#[async_trait]
impl Executor for XiaomiMimoExecutor {
    fn provider(&self) -> &str {
        BaseHooks::provider(&self.default)
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(&self.default)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        if !is_account_route(req.model, req.credentials) {
            return self.default.execute(req).await;
        }

        let proxy = req.proxy.clone();
        let cookie = match get_mimo_account_cookie(req.credentials, Some(&proxy)).await {
            Some(c) => c,
            None => return self.default.execute(req).await,
        };

        // Destructure once so the 401-retry can rebuild the request: the first
        // attempt consumes `req`, and the retry must re-borrow credentials to
        // fetch a fresh cookie.
        let ExecRequest {
            model,
            body,
            stream,
            credentials,
            provider_session_id,
            client_tool,
            cancel,
            log,
            proxy,
            provider_overrides,
        } = req;

        credentials.set(COOKIE_KEY, Json::String(cookie));
        let result = self.default.execute(ExecRequest {
            model,
            body: body.clone(),
            stream,
            credentials,
            provider_session_id: provider_session_id.clone(),
            client_tool: client_tool.clone(),
            cancel: cancel.clone(),
            log,
            proxy: proxy.clone(),
            provider_overrides: provider_overrides.clone(),
        }).await;

        // A cached session can expire early — drop it and retry once with a
        // fresh one. The response body stream isn't `Send`, so the result must
        // not be held across the refresh await; only the status crosses.
        let status = match &result {
            Ok(r) => r.response.status,
            Err(_) => 0,
        };
        if status != 401 {
            return result;
        }

        invalidate_cookie_cache();
        let fresh = get_mimo_account_cookie(credentials, Some(&proxy)).await;
        match fresh {
            Some(c) => {
                credentials.set(COOKIE_KEY, Json::String(c));
                self.default.execute(ExecRequest {
                    model,
                    body,
                    stream,
                    credentials,
                    provider_session_id,
                    client_tool,
                    cancel,
                    log,
                    proxy,
                    provider_overrides,
                }).await
            }
            None => result,
        }
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(XiaomiMimoExecutor::new("xiaomi-mimo"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_bare_model() {
        assert_eq!(bare_model("xiaomi-mimo/mimo-v2.6-pro"), "mimo-v2.6-pro");
        assert_eq!(bare_model("mimo-v2.6-flash"), "mimo-v2.6-flash");
    }

    #[test]
    fn test_account_route() {
        let creds = Credentials::from_value(json!({
            "providerSpecificData": { "mimoPassToken": "pt" }
        }));
        assert!(is_account_route("mimo-v2.6-pro", &creds));
        assert!(!is_account_route("mimo-v1.0", &creds));
        let no_creds = Credentials::from_value(json!({}));
        assert!(!is_account_route("mimo-v2.6-pro", &no_creds));
    }

    #[test]
    fn test_client_sign_format() {
        // Nonce with no ssecurity → sha1("nonce=abc") base64-URL-encoded.
        let sign = signature_client_sign("abc", "");
        // URL-encoded output must not contain raw '+' or '=' bytes.
        assert!(!sign.is_empty());
        assert!(sign.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' || b == b'%'));
    }

    #[test]
    fn test_resolve_url() {
        assert_eq!(resolve_url("https://a.com/x", "https://b.com/y"), "https://b.com/y");
        assert_eq!(resolve_url("https://a.com/x/y", "/z"), "https://a.com/z");
    }

    #[test]
    fn test_mimo_server_base_regions() {
        let cn = Credentials::from_value(json!({"providerSpecificData": {"region": "cn"}}));
        assert_eq!(resolve_mimo_server_base(&cn), "https://mimo-server-cn.xiaomimimo.com");
        let none = Credentials::from_value(json!({}));
        assert_eq!(resolve_mimo_server_base(&none), "https://mimo-server-sgp.xiaomimimo.com");
    }

    #[test]
    fn test_account_url_routing() {
        let creds = Credentials::from_value(json!({
            "providerSpecificData": {"mimoPassToken": "pt", "region": "sgp"}
        }));
        let exec = XiaomiMimoExecutor::new("xiaomi-mimo");
        let url = exec.build_url("mimo-v2.6-pro", true, 0, &creds).unwrap();
        assert_eq!(url, "https://mimo-server-sgp.xiaomimimo.com/api/route/chat/completions");
    }
}
