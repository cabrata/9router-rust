//! Port of open-sse/services/webIdentity.js
//!
//! Identity + liveness probes for web-cookie providers.
//!
//! Two callers share this: "Login via Browser" names the captured connection
//! after the account it belongs to, and the connection Test button needs a real
//! probe per provider (the generic API-key probe has no idea how to talk to a
//! web session). A probe answers three things — ok / rejected / unknown — so a
//! flaky network never flips a healthy connection to "error".
//!
//! shortcut: the JS `grokTlsFetch` path (wreq-js TLS impersonation) has no Rust
//! equivalent; grok-web probes go straight to the plain fetch, which Cloudflare
//! may reject. Upgrade when a TLS-fingerprinting client is available.

use std::time::Duration;

use base64::Engine as _;
use nr_common::gemini_cookies::normalize_gemini_validation_cookie;
use nr_common::web_cookie_auth::{extract_cookie_value, normalize_session_cookie_header};
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, UpstreamResponse};
use nr_core::Json;
use serde_json::json;

const UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
const TIMEOUT_MS: u64 = 15000;

/// Must match the UA of the browser the pasted cf_clearance was issued to
/// (`GROK_TLS_USER_AGENT` in grokTlsClient.js).
pub const GROK_TLS_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";

/// Probe outcome. `ok` mirrors the JS object: `{ok, status, email, error}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebIdentity {
    pub ok: bool,
    /// "valid" | "invalid" | "unknown"
    pub status: &'static str,
    pub email: String,
    pub error: Option<String>,
}

fn ok(email: &str) -> WebIdentity {
    WebIdentity { ok: true, status: "valid", email: email.to_string(), error: None }
}

fn rejected(error: &str) -> WebIdentity {
    WebIdentity { ok: false, status: "invalid", email: String::new(), error: Some(error.to_string()) }
}

fn unknown(error: &str) -> WebIdentity {
    WebIdentity { ok: false, status: "unknown", email: String::new(), error: Some(error.to_string()) }
}

/// `cookieJar(raw)`: split a Cookie header into name/value pairs.
pub fn cookie_jar(raw: &str) -> std::collections::HashMap<String, String> {
    let mut jar = std::collections::HashMap::new();
    for part in raw.split(';') {
        let eq = match part.find('=') {
            Some(eq) if eq > 0 => eq,
            _ => continue,
        };
        let name = part[..eq].trim();
        let value = part[eq + 1..].trim();
        jar.insert(name.to_string(), value.to_string());
    }
    jar
}

/// `decodeJwtPayload(token)`: base64url payload of a JWT, `null` on any failure.
pub fn decode_jwt_payload(token: &str) -> Option<Json> {
    let stripped = token.strip_prefix("base64-").unwrap_or(token);
    let b64 = stripped.split('.').nth(1)?;
    if b64.is_empty() {
        return None;
    }
    let pad = (4 - (b64.len() % 4)) % 4;
    let padded = format!("{b64}{}", "=".repeat(pad));
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(padded.as_bytes()))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

struct ProbeResponse {
    status: u16,
    ok: bool,
    text: String,
    error: Option<String>,
}

impl ProbeResponse {
    fn network_error(message: impl Into<String>) -> Self {
        Self { status: 0, ok: false, text: String::new(), error: Some(message.into()) }
    }

    async fn from_response(resp: UpstreamResponse) -> Self {
        let status = resp.status;
        let ok = resp.ok();
        let text = resp.text().await.unwrap_or_default();
        Self { status, ok, text, error: None }
    }
}

fn headers_from(pairs: &[(&str, &str)]) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.insert("User-Agent".into(), UA.to_string());
    for (k, v) in pairs {
        headers.insert((*k).to_string(), (*v).to_string());
    }
    headers
}

async fn request(url: &str, method: &str, headers: HeaderList, body: Option<String>) -> ProbeResponse {
    let mut req = FetchRequest::new(
        http::Method::from_bytes(method.as_bytes()).unwrap_or(http::Method::GET),
        url,
    );
    req.headers = headers;
    req.body = body.map(Into::into);
    req.header_timeout = Some(Duration::from_millis(TIMEOUT_MS));
    match proxy_fetch(req, None).await {
        Ok(resp) => ProbeResponse::from_response(resp).await,
        Err(e) => ProbeResponse::network_error(e.to_string()),
    }
}

async fn get_with(url: &str, pairs: &[(&str, &str)]) -> ProbeResponse {
    request(url, "GET", headers_from(pairs), None).await
}

async fn post_with(url: &str, pairs: &[(&str, &str)], body: &str) -> ProbeResponse {
    request(url, "POST", headers_from(pairs), Some(body.to_string())).await
}

/// Parse JSON, mapping a parse failure to the probe's "invalid JSON" answer.
fn parse_or(json: &str, message: &str) -> Result<Json, WebIdentity> {
    serde_json::from_str(json).map_err(|_| unknown(message))
}

async fn probe_deepseek_web(credential: &str) -> WebIdentity {
    let mut token = credential.trim().to_string();
    if let Ok(parsed) = serde_json::from_str::<Json>(&token) {
        if let Some(value) = parsed.get("value").and_then(Json::as_str) {
            token = value.to_string();
        }
    }
    if token.is_empty() {
        return rejected("Missing userToken");
    }
    let auth = format!("Bearer {token}");
    let res = get_with(
        "https://chat.deepseek.com/api/v0/users/current",
        &[
            ("Authorization", auth.as_str()),
            ("Accept", "*/*"),
            ("Origin", "https://chat.deepseek.com"),
            ("Referer", "https://chat.deepseek.com/"),
            ("X-Client-Bundle-Id", "com.deepseek.chat"),
            ("X-Client-Platform", "web"),
            ("X-Client-Version", "2.0.0"),
        ],
    )
    .await;
    if res.status == 0 {
        return unknown(res.error.as_deref().unwrap_or("network error"));
    }
    if res.status == 401 || res.status == 403 {
        return rejected("userToken is invalid or expired");
    }
    if !res.ok {
        return unknown(&format!("DeepSeek returned HTTP {}", res.status));
    }
    let json = match parse_or(&res.text, "DeepSeek returned invalid JSON") {
        Ok(j) => j,
        Err(e) => return e,
    };
    // Auth failures arrive as HTTP 200 with application-level code 40003.
    if json.get("code").and_then(Json::as_i64) == Some(40003) {
        return rejected("userToken is invalid or expired");
    }
    let biz = json.get("data").and_then(|d| d.get("biz_data"));
    let token_present = biz.and_then(|b| b.get("token")).map(|t| !t.is_null()).unwrap_or(false);
    if !token_present {
        return rejected("DeepSeek rejected the userToken");
    }
    ok(biz.and_then(|b| b.get("email")).and_then(Json::as_str).unwrap_or(""))
}

async fn probe_chatgpt_web(credential: &str) -> WebIdentity {
    let cookie = normalize_session_cookie_header(credential, "__Secure-next-auth.session-token");
    if cookie.is_empty() {
        return rejected("Missing session cookie");
    }
    let res = get_with(
        "https://chatgpt.com/api/auth/session",
        &[("Cookie", cookie.as_str()), ("Accept", "application/json")],
    )
    .await;
    if res.status == 0 {
        return unknown(res.error.as_deref().unwrap_or("network error"));
    }
    if res.status == 401 || res.status == 403 {
        return rejected("Session cookie is invalid or expired");
    }
    if !res.ok {
        return unknown(&format!("ChatGPT returned HTTP {}", res.status));
    }
    let json = match parse_or(&res.text, "ChatGPT returned invalid JSON") {
        Ok(j) => j,
        Err(e) => return e,
    };
    match json.get("user").and_then(|u| u.get("email")).and_then(Json::as_str) {
        Some(email) => ok(email),
        None => rejected("ChatGPT session is not signed in"),
    }
}

async fn probe_grok_web(credential: &str) -> WebIdentity {
    let sso = extract_cookie_value(credential, "sso");
    if sso.is_empty() {
        return rejected("Missing sso cookie");
    }
    // shortcut: no TLS-impersonating client, plain fetch only. See module docs.
    let res = get_with(
        "https://grok.com/api/auth/session",
        &[
            ("Cookie", credential),
            ("User-Agent", GROK_TLS_USER_AGENT),
            ("Accept", "application/json"),
            ("Origin", "https://grok.com"),
            ("Referer", "https://grok.com/"),
        ],
    )
    .await;
    if res.status == 0 {
        return unknown(res.error.as_deref().unwrap_or("network error"));
    }
    if res.status == 401 || res.status == 403 {
        return rejected("sso cookie is invalid or expired");
    }
    if !res.ok {
        return unknown(&format!("Grok returned HTTP {}", res.status));
    }
    let json = match parse_or(&res.text, "Grok returned invalid JSON") {
        Ok(j) => j,
        Err(e) => return e,
    };
    if json.get("status").and_then(Json::as_str) != Some("authenticated") {
        return rejected("Grok session is not signed in");
    }
    ok(json
        .get("session")
        .and_then(|s| s.get("email"))
        .and_then(Json::as_str)
        .unwrap_or(""))
}

/// Gemini sends response headers too large for undici's default cap; reqwest
/// has no comparable cap, so the plain HTTPS GET is used here.
async fn probe_gemini_web(credential: &str) -> WebIdentity {
    let cookie = match normalize_gemini_validation_cookie(credential) {
        Some(c) => c,
        None => return rejected("Missing __Secure-1PSID cookie"),
    };
    let res = get_with(
        "https://gemini.google.com/app",
        &[
            ("Cookie", cookie.as_str()),
            ("Accept", "text/html,application/xhtml+xml"),
        ],
    )
    .await;
    if res.status == 0 {
        return unknown(res.error.as_deref().unwrap_or("network error"));
    }
    if res.status == 401 || res.status == 403 {
        return rejected("__Secure-1PSID cookie is invalid or expired");
    }
    if !res.ok {
        return unknown(&format!("Gemini returned HTTP {}", res.status));
    }
    // A signed-out landing page also answers 200; only a real session carries SNlM0e.
    if !has_snlm0e(&res.text) {
        return rejected("Gemini session is not signed in");
    }
    ok("")
}

fn has_snlm0e(text: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r#""SNlM0e"\s*:\s*"[^"]+""#).expect("valid SNlM0e regex")
    });
    RE.is_match(text)
}

async fn probe_muse_web(credential: &str) -> WebIdentity {
    if cookie_jar(credential).get("hatch_sess").map(String::as_str).unwrap_or("").is_empty() {
        return rejected("Missing hatch_sess cookie");
    }
    let res = get_with(
        "https://muse.ai/api/session",
        &[
            ("Cookie", credential),
            ("Accept", "application/json"),
            ("Origin", "https://muse.ai"),
            ("Referer", "https://muse.ai/thread/new"),
        ],
    )
    .await;
    if res.status == 0 {
        return unknown(res.error.as_deref().unwrap_or("network error"));
    }
    if res.status == 401 || res.status == 403 {
        return rejected("muse.ai session cookie is invalid or expired");
    }
    if !res.ok {
        return unknown(&format!("muse.ai returned HTTP {}", res.status));
    }
    ok("")
}

async fn probe_uc_web(credential: &str) -> WebIdentity {
    let jar = cookie_jar(credential);
    let client = match jar.get("__client") {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return rejected("Missing __client cookie"),
    };
    let clerk_origin = "https://uncensored.com";
    let client_cookie = format!("__client={client}");
    // Legacy __client JWTs carry `sid`; current ones only `{id, rotating_token}`,
    // so fall back to the client's own session list.
    let mut sid = decode_jwt_payload(&client)
        .and_then(|p| p.get("sid").and_then(Json::as_str).map(str::to_string))
        .filter(|s| !s.is_empty());

    if sid.is_none() {
        let referer = format!("{clerk_origin}/");
        let list = get_with(
            "https://clerk.uncensored.com/v1/client?_clerk_js_version=5.127.1",
            &[
                ("Cookie", client_cookie.as_str()),
                ("Accept", "application/json"),
                ("Origin", clerk_origin),
                ("Referer", referer.as_str()),
            ],
        )
        .await;
        if list.status == 0 {
            return unknown(list.error.as_deref().unwrap_or("network error"));
        }
        if list.status == 401 || list.status == 403 {
            return rejected("Clerk session is invalid or expired");
        }
        if !list.ok {
            return unknown(&format!("Clerk returned HTTP {}", list.status));
        }
        let body = match parse_or(&list.text, "Clerk returned invalid JSON") {
            Ok(j) => j,
            Err(e) => return e,
        };
        let payload = body.get("response").or_else(|| body.get("client")).unwrap_or(&body);
        let sessions: Vec<&Json> = payload
            .get("sessions")
            .and_then(Json::as_array)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        let active = sessions
            .iter()
            .find(|s| s.get("status").and_then(Json::as_str) == Some("active"))
            .copied()
            .or_else(|| sessions.first().copied());
        sid = active
            .and_then(|s| s.get("id").and_then(Json::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if sid.is_none() {
            return rejected("Not signed in to uncensored.com — use Login via Browser");
        }
    }

    let sid = sid.unwrap_or_default();
    let url = format!(
        "https://clerk.uncensored.com/v1/client/sessions/{}/tokens?_clerk_js_version=5.127.1",
        urlencoding::encode(&sid)
    );
    let res = post_with(
        &url,
        &[
            ("Cookie", client_cookie.as_str()),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Origin", clerk_origin),
        ],
        "",
    )
    .await;
    if res.status == 0 {
        return unknown(res.error.as_deref().unwrap_or("network error"));
    }
    if res.status == 401 || res.status == 403 {
        return rejected("Clerk session is invalid or expired");
    }
    if !res.ok {
        return unknown(&format!("Clerk returned HTTP {}", res.status));
    }
    let json = match parse_or(&res.text, "Clerk returned invalid JSON") {
        Ok(j) => j,
        Err(e) => return e,
    };
    let jwt = match json.get("jwt").and_then(Json::as_str) {
        Some(j) if !j.is_empty() => j,
        _ => return rejected("Clerk did not mint a session token"),
    };
    let uid = decode_jwt_payload(jwt)
        .and_then(|p| p.get("uid").and_then(Json::as_str).map(str::to_string))
        .unwrap_or_default();
    ok(if uid.contains('@') { &uid } else { "" })
}

/// Provider ids with a dedicated probe (`Object.hasOwn(PROBES, providerId)`).
pub fn has_web_identity_probe(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "deepseek-web" | "chatgpt-web" | "grok-web" | "gemini-web" | "muse-web" | "uc-web"
    )
}

/// Probe a web-cookie credential.
pub async fn fetch_web_identity(provider_id: &str, credential: &str) -> WebIdentity {
    match provider_id {
        "deepseek-web" => probe_deepseek_web(credential).await,
        "chatgpt-web" => probe_chatgpt_web(credential).await,
        "grok-web" => probe_grok_web(credential).await,
        "gemini-web" => probe_gemini_web(credential).await,
        "muse-web" => probe_muse_web(credential).await,
        "uc-web" => probe_uc_web(credential).await,
        // The `muse-web` comment in JS ("identity lives inside the Supabase SSR
        // auth cookie") has no probe of its own; unknown is the JS answer.
        _ => unknown("No identity probe for this provider"),
    }
}

/// The JS shape `{ok, status, email, error}` — `error` omitted when absent.
impl WebIdentity {
    pub fn to_json(&self) -> Json {
        json!({
            "ok": self.ok,
            "status": self.status,
            "email": self.email,
            "error": self.error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_jar_parses_pairs() {
        let jar = cookie_jar("sso=abc; sso-rw=def; broken; =empty");
        assert_eq!(jar.get("sso").map(String::as_str), Some("abc"));
        assert_eq!(jar.get("sso-rw").map(String::as_str), Some("def"));
        // `indexOf("=") > 0` skips `broken`; ` =empty` has eq == 1 so JS keeps the
        // (empty, trimmed) name. Faithful to the JS quirk.
        assert_eq!(jar.get("").map(String::as_str), Some("empty"));
        assert_eq!(jar.len(), 3);
    }

    #[test]
    fn cookie_jar_empty_input() {
        assert!(cookie_jar("").is_empty());
    }

    #[test]
    fn jwt_payload_decodes_uc_client_sid() {
        // {"sid":"sess_123"}
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sid":"sess_123"}"#);
        let token = format!("header.{payload}.signature");
        let decoded = decode_jwt_payload(&token).expect("payload parses");
        assert_eq!(decoded.get("sid").and_then(Json::as_str), Some("sess_123"));
    }

    #[test]
    fn jwt_payload_strips_base64_prefix() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"uid":"a@b.c"}"#);
        let token = format!("base64-header.{payload}.sig");
        let decoded = decode_jwt_payload(&token).expect("payload parses");
        assert_eq!(decoded.get("uid").and_then(Json::as_str), Some("a@b.c"));
    }

    #[test]
    fn jwt_payload_rejects_garbage() {
        assert!(decode_jwt_payload("").is_none());
        assert!(decode_jwt_payload("nodots").is_none());
        assert!(decode_jwt_payload("a.!!!!.c").is_none());
    }

    #[test]
    fn probe_set_matches_js_keys() {
        for id in ["deepseek-web", "chatgpt-web", "grok-web", "gemini-web", "muse-web", "uc-web"] {
            assert!(has_web_identity_probe(id), "{id} should have a probe");
        }
        assert!(!has_web_identity_probe("perplexity-web"));
        assert!(!has_web_identity_probe(""));
    }

    #[tokio::test]
    async fn unknown_provider_is_unknown_status() {
        let result = fetch_web_identity("perplexity-web", "x").await;
        assert!(!result.ok);
        assert_eq!(result.status, "unknown");
        assert_eq!(result.error.as_deref(), Some("No identity probe for this provider"));
    }

    #[tokio::test]
    async fn deepseek_requires_token() {
        let result = fetch_web_identity("deepseek-web", "   ").await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing userToken"));
    }

    #[tokio::test]
    async fn deepseek_unwraps_json_value() {
        // Wrapper JSON without a usable token still rejects before any network call
        // only when the unwrapped value is empty.
        let result = fetch_web_identity("deepseek-web", r#"{"value":""}"#).await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing userToken"));
    }

    #[tokio::test]
    async fn chatgpt_requires_session_cookie() {
        let result = fetch_web_identity("chatgpt-web", "").await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing session cookie"));
    }

    #[tokio::test]
    async fn grok_requires_sso_cookie() {
        // Empty input is the only case JS rejects up front: `extractCookieValue`
        // falls through to the bare-value branch for a non-empty blob that has no
        // `sso=` pair, so "other=1" proceeds to the network as `Cookie: other=1`.
        let result = fetch_web_identity("grok-web", "   ").await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing sso cookie"));
        assert_eq!(extract_cookie_value("other=1", "sso"), "other=1");
    }

    #[tokio::test]
    async fn gemini_requires_psid_cookie() {
        let result = fetch_web_identity("gemini-web", "NID=abc").await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing __Secure-1PSID cookie"));
    }

    #[tokio::test]
    async fn muse_requires_hatch_sess() {
        let result = fetch_web_identity("muse-web", "a=b").await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing hatch_sess cookie"));
    }

    #[tokio::test]
    async fn uc_requires_client_cookie() {
        let result = fetch_web_identity("uc-web", "other=1").await;
        assert_eq!(result.status, "invalid");
        assert_eq!(result.error.as_deref(), Some("Missing __client cookie"));
    }

    #[test]
    fn snlm0e_detection() {
        assert!(has_snlm0e(r#"{"SNlM0e":"abc-1"}"#));
        assert!(has_snlm0e(r#"{ "SNlM0e" : "x" }"#));
        assert!(!has_snlm0e(r#"{"SNlM0e":""}"#));
        assert!(!has_snlm0e("<html>signed out</html>"));
    }

    #[test]
    fn identity_json_shape() {
        let j = ok("a@b.c").to_json();
        assert_eq!(j["ok"], Json::Bool(true));
        assert_eq!(j["status"], Json::String("valid".into()));
        assert_eq!(j["email"], Json::String("a@b.c".into()));
        assert!(j["error"].is_null());
    }
}
