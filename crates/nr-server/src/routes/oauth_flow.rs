//! OAuth flow HTTP routes: dynamic `/api/oauth/[provider]/[action]` endpoints plus
//! provider-specific import routes.
//! Faithful port of `src/app/api/oauth/**` and `src/lib/oauth/utils/server.js`.
//!
//! Backed by `nr_oauth` (`OAuthProvider` trait + flows helpers). Provider-flow
//! gaps inside `nr_oauth` surface as its own errors; this module never fabricates
//! a success.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use nr_oauth::flows::providers::{
    exchange_code, generate_auth_data, get_provider, poll_device_token, request_device_code,
};
use nr_oauth::flows::DeviceCodeResponse;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn error_response(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn invalid_body_response() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "Invalid or empty request body",
    )
}

/// Providers whose device-code flow does NOT use PKCE. JS: noPkceDeviceProviders.
const NO_PKCE_DEVICE_PROVIDERS: &[&str] = &[
    "github",
    "kiro",
    "kimi",
    "kimi-coding",
    "kilocode",
    "codebuddy-cn",
    "codebuddy-intl",
    "qoder",
    "qoder-cn",
    "grok-cli",
    "muse",
    "glm",
    "nous",
];

/// Whether the provider exposes a `device_code` flow (per its OAuthProvider trait).
fn supports_device_code(provider: &str) -> bool {
    match get_provider(provider) {
        Some(p) => p.flow_type() == "device_code",
        None => false,
    }
}

// ─── GET Handlers ───────────────────────────────────────────────────────────

/// GET /api/oauth/[provider]/[action]
pub async fn dynamic_get(
    Path((provider, action)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let result: nr_core::Result<Response> = handle_get(&provider, &action, params).await;
    match result {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            e.to_string(),
        ),
    }
}

async fn handle_get(
    provider: &str,
    action: &str,
    params: HashMap<String, String>,
) -> nr_core::Result<Response> {
    match action {
        "authorize" => handle_authorize(provider, params).await,
        "device-code" => handle_device_code(provider, params).await,
        "start-proxy" => Ok(handle_start_proxy(provider, &params).await),
        "poll-status" => Ok(handle_poll_status(provider, &params).await),
        "stop-proxy" => Ok(handle_stop_proxy(provider).await),
        "ide-status" => Ok(handle_ide_status(provider).await),
        "lease-status" => Ok(handle_lease_status(provider, &params).await),
        _ => Ok(error_response(StatusCode::BAD_REQUEST, "Unknown action")),
    }
}

async fn handle_authorize(provider: &str, params: HashMap<String, String>) -> nr_core::Result<Response> {
    // Xiaomi Desktop: custom ECDH flow — generate keypair, start proxy, return
    // the authorize URL (JS: `if (provider === "xiaomi-mimo")` branch).
    if provider == "xiaomi-mimo" {
        let (public_key, private_key) = generate_xiaomi_keypair();
        let runtime = proxy_runtime(provider);
        let Some(port) = start_proxy(&runtime, provider, None).await else {
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to start callback server",
            ));
        };
        let state = params
            .get("state")
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let redirect_uri = format!("http://127.0.0.1:{port}/");
        {
            let mut rt = runtime.lock().unwrap();
            rt.session = Some(ProxySession {
                status: "pending",
                error: None,
                exchange: ExchangeSpec {
                    code_verifier: String::new(),
                    redirect_uri: redirect_uri.clone(),
                    state: state.clone(),
                },
                private_key: Some(private_key),
                xiaomi_result: None,
                system_id: None,
                connection_id: None,
                email: None,
            });
        }
        let key_name = xiaomi_key_name();
        let authorize_url = format!(
            "https://platform.xiaomimimo.com/authorize?pk={}&redirect_uri={}&kn=mimocode&key_name={}",
            urlencoding::encode(&public_key),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(&key_name),
        );
        return Ok((
            StatusCode::OK,
            Json(json!({
                "state": state,
                "authorizeUrl": authorize_url,
                "redirectUri": redirect_uri,
                "port": port,
            })),
        )
            .into_response());
    }

    let redirect_uri = params
        .get("redirect_uri")
        .cloned()
        .unwrap_or_else(|| "http://localhost:8080/callback".to_string());

    // Collect provider-specific meta (everything except redirect_uri).
    let mut meta_map = serde_json::Map::new();
    for (k, v) in &params {
        if k != "redirect_uri" && !v.is_empty() {
            meta_map.insert(k.clone(), Value::String(v.clone()));
        }
    }
    // Zed: derive native_app_port from the local callback URL so the RSA keypair
    // is bound to the port the proxy is actually listening on.
    if provider == "zed" {
        if let Some(port) = url::Url::parse(&redirect_uri).ok().and_then(|u| u.port()) {
            meta_map.insert("nativeAppPort".into(), json!(port));
        }
    }
    let meta = if meta_map.is_empty() { None } else { Some(Value::Object(meta_map)) };

    if get_provider(provider).is_none() {
        return Ok(error_response(
            StatusCode::NOT_IMPLEMENTED,
            format!("Provider `{provider}` is not registered in this build"),
        ));
    }

    let log = nr_core::Logger::new();
    let auth_data =
        generate_auth_data(provider, &redirect_uri, meta.as_ref(), &log).await?;
    Ok((StatusCode::OK, Json(auth_data)).into_response())
}

async fn handle_device_code(
    provider: &str,
    params: HashMap<String, String>,
) -> nr_core::Result<Response> {
    if !supports_device_code(provider) {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "Provider does not support device code flow",
        ));
    }

    let log = nr_core::Logger::new();
    let mut code_challenge: Option<String> = None;

    // For PKCE-using providers we first generate a generic PKCE pair so the
    // client can retain the verifier for polling (JS generateAuthData + challenge).
    if !NO_PKCE_DEVICE_PROVIDERS.contains(&provider) {
        let auth_data = generate_auth_data(provider, "", None, &log).await?;
        code_challenge = auth_data
            .get("codeChallenge")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }

    let mut device_options_map = serde_json::Map::new();
    for key in ["startUrl", "region", "authMethod"] {
        if let Some(v) = params.get(key) {
            if !v.is_empty() {
                device_options_map.insert(key.to_string(), Value::String(v.clone()));
            }
        }
    }
    let device_options = if device_options_map.is_empty() {
        None
    } else {
        Some(Value::Object(device_options_map))
    };

    let device_data =
        request_device_code(provider, code_challenge.as_deref(), device_options.as_ref(), &log)
            .await?;

    let mut resp = device_data_to_json(&device_data);

    // JS prefers the provider-generated verifier when present, falling back to the
    // generic one computed above.
    if let Some(extra) = &device_data.extra {
        if let Some(v) = extra.get("codeVerifier").and_then(|x| x.as_str()) {
            resp["codeVerifier"] = Value::String(v.to_string());
        }
    }
    if !resp.get("codeVerifier").is_some() {
        // No provider verifier: fall back to generating a fresh pair so PKCE
        // clients still have something to exchange with.
        let log2 = nr_core::Logger::new();
        let fresh = generate_auth_data(provider, "", None, &log2).await?;
        resp["codeVerifier"] = fresh.get("codeVerifier").cloned().unwrap_or(Value::Null);
    }

    Ok((StatusCode::OK, Json(resp)).into_response())
}

fn device_data_to_json(d: &DeviceCodeResponse) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("deviceCode".into(), Value::String(d.device_code.clone()));
    m.insert("userCode".into(), Value::String(d.user_code.clone()));
    m.insert("verificationUri".into(), Value::String(d.verification_uri.clone()));
    if let Some(uri) = &d.verification_uri_complete {
        m.insert("verificationUriComplete".into(), Value::String(uri.clone()));
    }
    m.insert("expiresIn".into(), json!(d.expires_in));
    m.insert("interval".into(), json!(d.interval));
    if let Some(extra) = &d.extra {
        for (k, v) in extra {
            if !m.contains_key(k) {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(m)
}

// ─── Local callback proxies ─────────────────────────────────────────────────
//
// Port of `src/lib/oauth/utils/server.js`: in-process loopback HTTP servers that
// receive the provider's OAuth callback, auto-exchange the code, store the
// connection and record the outcome on the session the modal polls.
//
// One `ProxyRuntime` per provider; the modal drives it via /start-proxy,
// /register-session, /poll-status and /stop-proxy exactly as in JS.

struct ProxyRuntime {
    // Fixed callbacks (codex: 1455, zed preferred: 58443) fall back to a
    // random port when busy, like the JS `server.on("error", …)` handlers.
    preferred_port: Option<u16>,
    port: Option<u16>,
    task: Option<tokio::task::AbortHandle>,
    callback_path: &'static str,
    session: Option<ProxySession>,
}

/// One registered server-side exchange session.
struct ProxySession {
    status: &'static str,
    error: Option<String>,
    exchange: ExchangeSpec,
    /// xiaomi-mimo: raw X25519 private key (32 bytes) used to decrypt `u`.
    #[allow(dead_code)]
    private_key: Option<Vec<u8>>,
    /// xiaomi-mimo: decrypted callback payload, surfaced by poll-status.
    xiaomi_result: Option<Value>,
    /// zed: login attempt's own id, merged into the exchange meta.
    system_id: Option<String>,
    connection_id: Option<String>,
    email: Option<String>,
}

#[derive(Clone)]
struct ExchangeSpec {
    code_verifier: String,
    redirect_uri: String,
    state: String,
}

static PROXY_STATE: LazyLock<Mutex<HashMap<String, Arc<Mutex<ProxyRuntime>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn proxy_runtime(provider: &str) -> Arc<Mutex<ProxyRuntime>> {
    let mut states = PROXY_STATE.lock().unwrap_or_else(|e| e.into_inner());
    states
        .entry(provider.to_string())
        .or_insert_with(|| {
            let (preferred_port, callback_path) = match provider {
                "codex" => (Some(1455), "/auth/callback"),
                "zed" => (Some(58443), "/"),
                "windsurf" => (None, "/windsurf-auth-callback"),
                _ => (None, "/callback"),
            };
            Arc::new(Mutex::new(ProxyRuntime {
                preferred_port,
                port: None,
                task: None,
                callback_path,
                session: None,
            }))
        })
        .clone()
}

/// `isLoopbackOrigin(origin)`: navigation redirects have no Origin; anything
/// else must be a loopback HTTP origin.
fn is_loopback_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("origin") else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" {
        return false;
    }
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host == "127.0.0.1" || host == "localhost"
}

fn html_result_page(success: bool, message: &str) -> String {
    let (color, icon, title) = if success {
        ("#22c55e", "&#10003;", "Authentication Successful")
    } else {
        ("#ef4444", "&#10007;", "Authentication Failed")
    };
    let message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    format!(
        r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{title}</title>
<style>body{{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#f5f5f5}}.c{{text-align:center;padding:2rem;background:#fff;border-radius:8px;box-shadow:0 2px 10px rgba(0,0,0,.1)}}.i{{color:{color};font-size:3rem}}h1{{margin:1rem 0}}p{{color:#666}}</style>
</head><body><div class="c"><div class="i">{icon}</div><h1>{title}</h1><p>{message}</p><p>Closing in <span id="cd">3</span>s...</p>
<script>let n=3;const c=document.getElementById("cd");const t=setInterval(()=>{{n--;c.textContent=n;if(n<=0){{clearInterval(t);window.close();}}}},1000);</script>
</div></body></html>"##
    )
}

async fn bind_loopback(preferred: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", preferred)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// `GET /api/oauth/[provider]/start-proxy`.
async fn handle_start_proxy(provider: &str, params: &HashMap<String, String>) -> Response {
    let runtime = proxy_runtime(provider);
    let preferred = params
        .get("native_app_port")
        .and_then(|p| p.parse::<u16>().ok());
    match start_proxy(&runtime, provider, preferred).await {
        Some(port) => {
            let path = runtime.lock().unwrap().callback_path;
            Json(json!({
                "success": true,
                "port": port,
                "callbackUrl": format!("http://127.0.0.1:{port}{path}"),
            }))
            .into_response()
        }
        None => error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to start callback server"),
    }
}

async fn start_proxy(
    runtime: &Arc<Mutex<ProxyRuntime>>,
    provider: &str,
    preferred: Option<u16>,
) -> Option<u16> {
    {
        let mut rt = runtime.lock().unwrap();
        if let Some(port) = rt.port {
            // Reuse the live listener and renew its idle timeout (JS does the
            // same so a previous flow's deadline can't kill the new one).
            if let Some(task) = rt.task.take() {
                task.abort();
            }
            rt.task = Some(tokio::spawn(proxy_idle_kill(runtime.clone())).abort_handle());
            return Some(port);
        }
    }
    let preferred = preferred.or_else(|| runtime.lock().unwrap().preferred_port).unwrap_or(0);
    let (listener, port) = match bind_loopback(preferred).await {
        Ok(ok) => ok,
        Err(_) => {
            if preferred != 0 {
                bind_loopback(0).await.ok()?
            } else {
                return None;
            }
        }
    };
    {
        let mut rt = runtime.lock().unwrap();
        rt.port = Some(port);
        rt.task = Some(tokio::spawn(proxy_idle_kill(runtime.clone())).abort_handle());
    }
    let app = Router::new()
        .route("/callback", any(callback_handler))
        .route("/auth/callback", any(callback_handler))
        .route("/windsurf-auth-callback", any(callback_handler))
        .route("/", any(callback_handler))
        .with_state((provider.to_string(), runtime.clone()));
    runtime.lock().unwrap().task = Some(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }).abort_handle());
    Some(port)
}

async fn proxy_idle_kill(runtime: Arc<Mutex<ProxyRuntime>>) {
    // JS timeouts: codex/xai 300s, trae/windsurf/zed/perchai 600s.
    tokio::time::sleep(Duration::from_secs(600)).await;
    let mut rt = runtime.lock().unwrap();
    if let Some(task) = rt.task.take() {
        task.abort();
    }
    rt.port = None;
    rt.session = None;
}

/// Extract a parsed callback as JSON from the query string, or None when the
/// request is not the actual provider callback (probe/prefetch/stray hit).
fn select_callback(provider: &str, query: &HashMap<String, String>) -> Option<Value> {
    match provider {
        "codex" | "xai" => {
            let code = query.get("code")?;
            Some(json!({ "code": code }))
        }
        "trae" => {
            if !query.contains_key("refreshToken") {
                return None;
            }
            let raw = format!("/callback?{}", serde_urlencoded::to_string(query).unwrap_or_default());
            Some(json!({ "code": raw }))
        }
        "windsurf" => {
            if !query.contains_key("access_token") {
                return None;
            }
            let raw = format!(
                "/windsurf-auth-callback?{}",
                serde_urlencoded::to_string(query).unwrap_or_default()
            );
            Some(json!({ "code": raw }))
        }
        "zed" => {
            if !(query.contains_key("user_id")
                || query.contains_key("userId")
                || query.contains_key("access_token")
                || query.contains_key("accessToken")
                || query.contains_key("token"))
            {
                return None;
            }
            let raw = format!("/?{}", serde_urlencoded::to_string(query).unwrap_or_default());
            Some(json!({ "code": raw }))
        }
        "xiaomi-mimo" => query.get("u").map(|u| json!({ "u": u })),
        "perchai" => query
            .get("code")
            .or_else(|| query.get("access_token"))
            .map(|c| json!({ "code": c })),
        _ => None,
    }
}

async fn callback_handler(
    State((provider, runtime)): State<(String, Arc<Mutex<ProxyRuntime>>)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if !is_loopback_origin(&headers) {
        return (StatusCode::FORBIDDEN, Html(html_result_page(false, "Cross-origin callback rejected")))
            .into_response();
    }

    let query: HashMap<String, String> = uri
        .query()
        .map(|q| serde_urlencoded::from_str(q).unwrap_or_default())
        .unwrap_or_default();

    // Snapshot the session state under a short-lived lock, then release it
    // before any `.await` (holding a MutexGuard across an await makes the
    // handler future !Send, which fails axum's Handler bound).
    struct SessionSnapshot {
        exchange: ExchangeSpec,
        private_key: Option<Vec<u8>>,
        system_id: Option<String>,
    }
    let snap: Option<SessionSnapshot> = {
        let rt = runtime.lock().unwrap();
        rt.session.as_ref().map(|s| SessionSnapshot {
            exchange: s.exchange.clone(),
            private_key: s.private_key.clone(),
            system_id: s.system_id.clone(),
        })
    };

    let Some(snap) = snap else {
        // Mode B (codex/xai): no registered session → legacy redirect to the app
        // port, exactly like startCodexProxy/startXaiProxy.
        if provider == "codex" || provider == "xai" {
            if let Some(app_port) = query.get("app_port") {
                let location = format!("http://localhost:{app_port}/callback?{}", uri.query().unwrap_or(""));
                let rt2 = runtime.clone();
                tokio::spawn(async move {
                    stop_proxy(&rt2).await;
                });
                return (StatusCode::FOUND, [("Location", location)]).into_response();
            }
        }
        return (StatusCode::OK, Html(html_result_page(false, "No active login session"))).into_response();
    };

    // Anti-CSRF: a state-bearing callback must match the registered session.
    if let Some(cb_state) = query.get("state") {
        if cb_state != &snap.exchange.state {
            {
                let mut rt = runtime.lock().unwrap();
                if let Some(session) = rt.session.as_mut() {
                    session.status = "error";
                    session.error = Some("Callback state mismatch".to_string());
                }
            }
            let html = Html(html_result_page(false, "Callback state mismatch"));
            let rt2 = runtime.clone();
            tokio::spawn(async move {
                stop_proxy(&rt2).await;
            });
            return (StatusCode::OK, html).into_response();
        }
    }

    let Some(selected) = select_callback(&provider, &query) else {
        // Probe/prefetch: keep session and server alive for the real redirect.
        return (StatusCode::OK, Html(html_result_page(
            false,
            "Waiting for sign-in — this request carried no login data.",
        )))
        .into_response();
    };

    if provider == "xiaomi-mimo" {
        let Some(private_key) = snap.private_key.as_ref() else {
            {
                let mut rt = runtime.lock().unwrap();
                if let Some(session) = rt.session.as_mut() {
                    session.status = "error";
                    session.error = Some("No decryption key for this session".to_string());
                }
            }
            return (StatusCode::OK, Html(html_result_page(false, "No decryption key for this session")))
                .into_response();
        };
        let u = selected.get("u").and_then(Value::as_str).unwrap_or("");
        match decrypt_xiaomi_callback(private_key, u) {
            Ok(result) => {
                {
                    let mut rt = runtime.lock().unwrap();
                    if let Some(session) = rt.session.as_mut() {
                        session.status = "done";
                        session.xiaomi_result = Some(result);
                    }
                }
                (StatusCode::OK, Html(html_result_page(true, "You can close this window.")))
                    .into_response()
            }
            Err(err) => {
                {
                    let mut rt = runtime.lock().unwrap();
                    if let Some(session) = rt.session.as_mut() {
                        session.status = "error";
                        session.error = Some(err.clone());
                    }
                }
                let rt2 = runtime.clone();
                tokio::spawn(async move {
                    stop_proxy(&rt2).await;
                });
                (StatusCode::OK, Html(html_result_page(false, &err))).into_response()
            }
        }
    } else {
        let rt2 = runtime.clone();
        let result =
            run_callback_exchange(&provider, &selected, &snap.exchange, snap.system_id.as_deref())
                .await;
        match result {
            Ok((html, connection_id, email)) => {
                {
                    let mut rt = runtime.lock().unwrap();
                    if let Some(session) = rt.session.as_mut() {
                        session.status = "done";
                        session.connection_id = connection_id;
                        session.email = email;
                    }
                }
                tokio::spawn(async move {
                    stop_proxy(&rt2).await;
                });
                (StatusCode::OK, Html(html)).into_response()
            }
            Err(err) => {
                {
                    let mut rt = runtime.lock().unwrap();
                    if let Some(session) = rt.session.as_mut() {
                        session.status = "error";
                        session.error = Some(err.clone());
                    }
                }
                (StatusCode::OK, Html(html_result_page(false, &err))).into_response()
            }
        }
    }
}

async fn stop_proxy(runtime: &Arc<Mutex<ProxyRuntime>>) {
    let mut rt = runtime.lock().unwrap();
    if let Some(task) = rt.task.take() {
        task.abort();
    }
    rt.port = None;
    rt.session = None;
}

/// Exchange the callback code and persist the connection; returns the result
/// page HTML on success, the error message otherwise.
async fn run_callback_exchange(
    provider: &str,
    selected: &Value,
    exchange: &ExchangeSpec,
    system_id: Option<&str>,
) -> Result<(String, Option<String>, Option<String>), String> {
    let code = selected.get("code").and_then(Value::as_str).unwrap_or("");
    let mut meta = None;
    if provider == "zed" {
        if let Some(system_id) = system_id {
            meta = Some(json!({ "systemId": system_id }));
        }
    }
    let mapped = exchange_code(
        provider,
        code,
        &exchange.redirect_uri,
        Some(&exchange.code_verifier),
        Some(&exchange.state),
        meta.as_ref(),
        &nr_core::Logger::new(),
    )
    .await
    .map_err(|e| e.to_string())?;

    let mut conn = mapped.to_map();
    conn.insert("provider".into(), Value::String(provider.to_string()));
    conn.insert("authType".into(), Value::String("oauth".into()));
    if let Some(expires_in) = mapped.expires_in {
        let expires_at = chrono::Utc::now() + chrono::Duration::seconds(expires_in);
        conn.insert(
            "expiresAt".into(),
            Value::String(expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
    }
    conn.insert("testStatus".into(), Value::String("active".into()));

    let db = nr_db::db().map_err(|e| e.to_string())?;
    let created = nr_db::create_provider_connection(&db, &Value::Object(conn))
        .map_err(|e| e.to_string())?;

    let connection_id = created.get("id").and_then(Value::as_str).map(str::to_string);
    let email = created.get("email").and_then(Value::as_str).map(str::to_string);
    Ok((html_result_page(true, "You can close this window."), connection_id, email))
}

/// `POST /api/oauth/[provider]/register-session`.
async fn handle_register_session(provider: &str, body: Option<&Value>) -> Response {
    let Some(body) = body else {
        return error_response(StatusCode::BAD_REQUEST, "Missing request body");
    };
    let state = body.get("state").and_then(Value::as_str).unwrap_or("").trim();
    if state.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Missing state");
    }
    let code_verifier = body
        .get("codeVerifier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let redirect_uri = body
        .get("redirectUri")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let system_id = body.get("systemId").and_then(Value::as_str).map(str::to_string);

    let runtime = proxy_runtime(provider);
    if runtime.lock().unwrap().port.is_none() {
        if start_proxy(&runtime, provider, None).await.is_none() {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to start callback server");
        }
    }

    let mut rt = runtime.lock().unwrap();
    rt.session = Some(ProxySession {
        status: "pending",
        error: None,
        exchange: ExchangeSpec {
            code_verifier,
            redirect_uri,
            state: state.to_string(),
        },
        private_key: None,
        xiaomi_result: None,
        system_id,
        connection_id: None,
        email: None,
    });
    Json(json!({ "success": true })).into_response()
}

fn proxy_session_status(session: &ProxySession, provider: &str) -> Value {
    let mut out = json!({ "status": session.status });
    if let Some(err) = &session.error {
        out["error"] = Value::String(err.clone());
    }
    if let Some(id) = &session.connection_id {
        out["connectionId"] = Value::String(id.clone());
    }
    if let Some(email) = &session.email {
        out["email"] = Value::String(email.clone());
    }
    if provider == "xiaomi-mimo" {
        if let Some(result) = &session.xiaomi_result {
            out["result"] = result.clone();
        }
    }
    out
}

/// `GET /api/oauth/[provider]/poll-status`.
async fn handle_poll_status(provider: &str, params: &HashMap<String, String>) -> Response {
    let state = params.get("state").cloned().unwrap_or_default();
    let runtime = proxy_runtime(provider);
    let rt = runtime.lock().unwrap();
    let Some(session) = rt.session.as_ref() else {
        return Json(json!({ "status": "unknown" })).into_response();
    };
    if !state.is_empty() && session.exchange.state != state {
        return Json(json!({ "status": "unknown" })).into_response();
    }
    if session.status == "done" || session.status == "error" {
        let payload = proxy_session_status(session, provider);
        let rt2 = runtime.clone();
        tokio::spawn(async move {
            let mut rt = rt2.lock().unwrap();
            rt.session = None;
        });
        return Json(payload).into_response();
    }
    Json(json!({ "status": session.status })).into_response()
}

/// `GET /api/oauth/[provider]/stop-proxy`.
async fn handle_stop_proxy(provider: &str) -> Response {
    stop_proxy(&proxy_runtime(provider)).await;
    Json(json!({ "success": true })).into_response()
}

/// `GET /api/oauth/[provider]/ide-status`.
async fn handle_ide_status(provider: &str) -> Response {
    match provider {
        "trae" | "windsurf" => {
            Json(super::oauth_local_creds::detect_ide_installed(provider)).into_response()
        }
        "perchai" => Json(super::oauth_local_creds::detect_perch_cli_installed()).into_response(),
        _ => error_response(
            StatusCode::BAD_REQUEST,
            "ide-status only supported for trae/windsurf/perchai",
        ),
    }
}

// ─── Xiaomi MiMo ECDH callback decryption ──────────────────────────────────
//
// Port of `src/lib/oauth/providers/xiaomi-mimo.js` `generateKeyPair` /
// `decryptCallback`: X25519 ECDH → SHA-256 → AES-256-GCM.
// Wire format (base64-decoded): [12-byte nonce][32-byte ephemeral pubkey]
// [ciphertext][16-byte GCM tag].

/// `generateKeyPair()` → (base64 SPKI public key, raw 32-byte private scalar).
fn generate_xiaomi_keypair() -> (String, Vec<u8>) {
    use base64::Engine;
    use x25519_dalek::{PublicKey, StaticSecret};
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public = PublicKey::from(&secret);
    // SPKI DER prefix for X25519 ("302a300506032b656e032100") + raw key.
    let mut spki = Vec::with_capacity(44);
    spki.extend_from_slice(&hex::decode("302a300506032b656e032100").unwrap());
    spki.extend_from_slice(public.as_bytes());
    (base64::engine::general_purpose::STANDARD.encode(spki), secret.to_bytes().to_vec())
}

/// `decryptCallback(privateKeyDer, encryptedB64)`.
fn decrypt_xiaomi_callback(private_key: &[u8], encrypted_b64: &str) -> Result<Value, String> {
    use aes_gcm::aead::{AeadInPlace, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use x25519_dalek::{PublicKey, StaticSecret};

    let raw = base64::engine::general_purpose::STANDARD
        .decode(encrypted_b64)
        .map_err(|_| "Encrypted payload is not valid base64".to_string())?;
    if raw.len() < 12 + 32 + 16 + 1 {
        return Err(format!("Encrypted payload too short: {} bytes", raw.len()));
    }
    let nonce = &raw[..12];
    let mut ephemeral = [0u8; 32];
    ephemeral.copy_from_slice(&raw[12..44]);
    let ciphertext = &raw[44..raw.len() - 16];
    let tag = &raw[raw.len() - 16..];

    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(
        private_key
            .get(..32)
            .ok_or_else(|| "Bad private key".to_string())?,
    );
    let private_key = StaticSecret::from(key_bytes);
    let shared = private_key.diffie_hellman(&PublicKey::from(ephemeral));
    let key = Sha256::digest(shared.as_bytes());

    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    let mut plaintext = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(Nonce::from_slice(nonce), b"", &mut plaintext, aes_gcm::Tag::from_slice(tag))
        .map_err(|_| "Decryption failed".to_string())?;
    let parsed: Value = serde_json::from_slice(&plaintext).map_err(|_| "Decrypted payload is not valid JSON".to_string())?;
    if !parsed.is_object() {
        return Err("Decrypted payload is not a valid object".to_string());
    }
    Ok(parsed)
}

// ─── POST Handlers ──────────────────────────────────────────────────────────

/// POST /api/oauth/[provider]/[action]
pub async fn dynamic_post(
    Path((provider, action)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> Response {
    if action == "exchange" || action == "poll" || action == "manual-code" {
        if body.is_none() {
            return invalid_body_response();
        }
    }

    let result: nr_core::Result<Response> = handle_post(&provider, &action, query, body.map(|Json(v)| v)).await;
    match result {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            e.to_string(),
        ),
    }
}

async fn handle_post(
    provider: &str,
    action: &str,
    query: HashMap<String, String>,
    body: Option<Value>,
) -> nr_core::Result<Response> {
    let log = nr_core::Logger::new();

    match action {
        "lease-connect" => handle_lease_connect(provider, body.as_ref()).await,
        "register-session" => Ok(handle_register_session(provider, body.as_ref()).await),
        "exchange" => handle_exchange(provider, body.as_ref(), &log).await,
        "poll" => handle_poll(provider, body.as_ref(), &log).await,
        "manual-code" => {
            let code = body
                .as_ref()
                .and_then(|b| b.get("code"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let state = body
                .as_ref()
                .and_then(|b| b.get("state"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();

            if provider != "xai" {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "Manual code only supported for xai",
                ));
            }
            if state.is_empty() || code.is_empty() {
                return Ok(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Missing xAI authorization code or state",
                ));
            }

            // `completeXaiManualCode`: the session was registered by
            // /register-session (or by the proxy's Mode A callback), so the
            // verifier and redirect URI live there. A missing session is an
            // error, exactly like JS.
            let runtime = proxy_runtime(provider);
            let Some((code_verifier, redirect_uri)) = runtime
                .lock()
                .unwrap()
                .session
                .as_ref()
                .filter(|s| s.exchange.state == state)
                .map(|s| (s.exchange.code_verifier.clone(), s.exchange.redirect_uri.clone()))
            else {
                return Ok(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "xAI OAuth session not found; restart the login flow and paste the code again",
                ));
            };

            let mapped = exchange_code(
                provider,
                code,
                &redirect_uri,
                Some(&code_verifier),
                Some(state),
                None,
                &log,
            )
            .await?;

            let mut conn = mapped.to_map();
            conn.insert("provider".into(), Value::String("xai".into()));
            conn.insert("authType".into(), Value::String("oauth".into()));
            if let Some(expires_in) = mapped.expires_in {
                let expires_at = chrono::Utc::now() + chrono::Duration::seconds(expires_in);
                conn.insert(
                    "expiresAt".into(),
                    Value::String(expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                );
            }
            conn.insert("testStatus".into(), Value::String("active".into()));
            let db = nr_db::db()?;
            let created = nr_db::create_provider_connection(&db, &Value::Object(conn))?;

            // Done: clear the session and stop the proxy, like completeXaiManualCode.
            {
                let mut rt = runtime.lock().unwrap();
                rt.session = None;
            }
            let runtime2 = runtime.clone();
            tokio::spawn(async move {
                stop_proxy(&runtime2).await;
            });

            Ok((
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "connection": {
                        "id": created.get("id").cloned().unwrap_or(Value::Null),
                        "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                        "email": created.get("email").cloned().unwrap_or(Value::Null),
                        "displayName": created.get("displayName").cloned().unwrap_or(Value::Null),
                    }
                })),
            )
                .into_response())
        }
        _ => Ok(error_response(StatusCode::BAD_REQUEST, "Unknown action")),
    }
}

/// `POST /api/oauth/[provider]/lease-connect`: MiniMax code desktop lease.
async fn handle_lease_connect(provider: &str, body: Option<&Value>) -> nr_core::Result<Response> {
    if provider != "minimax-code" && provider != "minimax-code-global" {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "lease-connect only supported for minimax-code/minimax-code-global",
        ));
    }
    let data_dir = body
        .and_then(|b| b.get("dataDir"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| nr_core::paths::home_dir().join(".minimax"));

    // Validate availability up front so the user gets the broker's exact
    // failure reason instead of a dead connection. The capability read is the
    // same check the executor performs for every request.
    let cap = data_dir.join("run").join("mcode-auth-lease-v1.cap");
    if !cap.is_file() {
        return Ok(error_response(
            StatusCode::CONFLICT,
            format!(
                "Cannot read capability file {}. Is the MiniMax Code desktop app installed and logged in on this machine?",
                cap.display()
            ),
        ));
    }

    // Idempotent: one lease connection per (provider, data dir).
    let db = nr_db::db()?;
    let existing = nr_db::get_provider_connections(&db, Some(&json!({ "provider": provider })))?
        .into_iter()
        .find(|c| {
            c.get("authType").and_then(Value::as_str) == Some("mcode-lease")
                && c.get("providerSpecificData")
                    .and_then(|p| p.get("mcodeDataDir"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    == Some(data_dir.clone())
        });
    if let Some(existing) = existing {
        return Ok((
            StatusCode::OK,
            Json(json!({
                "success": true,
                "connection": {
                    "id": existing.get("id").cloned().unwrap_or(Value::Null),
                    "provider": existing.get("provider").cloned().unwrap_or(Value::Null),
                },
                "reused": true,
            })),
        )
            .into_response());
    }

    let name = if provider == "minimax-code-global" {
        "MiniMax Desktop (Lease, Global)"
    } else {
        "MiniMax Desktop (Lease)"
    };
    let created = nr_db::create_provider_connection(
        &db,
        &json!({
            "provider": provider,
            "authType": "mcode-lease",
            "name": name,
            "providerSpecificData": { "mcodeDataDir": data_dir.to_string_lossy() },
            "testStatus": "active",
        }),
    )?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "success": true,
            "connection": {
                "id": created.get("id").cloned().unwrap_or(Value::Null),
                "provider": created.get("provider").cloned().unwrap_or(Value::Null),
            },
        })),
    )
        .into_response())
}

/// `GET /api/oauth/[provider]/lease-status`: MiniMax Code desktop broker probe.
/// Read-only — leases nothing. Matches the JS `lease-status` action.
async fn handle_lease_status(provider: &str, params: &HashMap<String, String>) -> Response {
    if provider != "minimax-code" && provider != "minimax-code-global" {
        return error_response(
            StatusCode::BAD_REQUEST,
            "lease-status only supported for minimax-code/minimax-code-global",
        );
    }
    let data_dir = params
        .get("data_dir")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| nr_core::paths::home_dir().join(".minimax"));
    match nr_exec_simple::minimax_code::request_status(&data_dir).await {
        Ok(status) => {
            let mut out = serde_json::Map::new();
            out.insert("found".into(), json!(true));
            out.insert("dataDir".into(), json!(data_dir.display().to_string()));
            if let Some(obj) = status.as_object() {
                out.extend(obj.clone());
            }
            Json(Value::Object(out)).into_response()
        }
        Err(e) => {
            let message = e.to_string();
            let code = if message.contains("Capability file") {
                "BROKER_UNAVAILABLE"
            } else if message.contains("sign-in was refused") {
                "BROKER_REJECTED"
            } else {
                "INTERNAL_ERROR"
            };
            Json(json!({
                "found": false,
                "dataDir": data_dir.display().to_string(),
                "error": code,
                "message": message,
            }))
            .into_response()
        }
    }
}

/// `getKeyName()` from `src/lib/oauth/providers/xiaomi-mimo.js`: stable
/// per-machine key label, no filesystem writes.
fn xiaomi_key_name() -> String {
    let machine_id = {
        use sha2::{Digest, Sha256};
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".into());
        let input = format!("{}-{}", std::env::consts::OS, host);
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..4])
    };
    format!("9router-xmd-{machine_id}")
}

async fn handle_exchange(
    provider: &str,
    body: Option<&Value>,
    log: &nr_core::Logger,
) -> nr_core::Result<Response> {
    if get_provider(provider).is_none() {
        return Ok(error_response(
            StatusCode::NOT_IMPLEMENTED,
            format!("Provider `{provider}` is not registered in this build"),
        ));
    }

    // ── xiaomi-mimo ───────────────────────────────────────────────────────
    // No token exchange: the callback already decrypted `sk`. Just read the
    // proxy session result and create the connection (JS first branch).
    if provider == "xiaomi-mimo" {
        let Some(state) = body.and_then(|b| b.get("state")).and_then(Value::as_str) else {
            return Ok(error_response(StatusCode::BAD_REQUEST, "Missing state"));
        };
        let runtime = proxy_runtime(provider);
        let session_payload = {
            let rt = runtime.lock().unwrap();
            let Some(session) = rt.session.as_ref() else {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "OAuth session not completed. Please restart the login flow.",
                ));
            };
            if session.exchange.state != state || session.status != "done" {
                let err = session.error.clone().unwrap_or_else(|| {
                    "OAuth session not completed. Please restart the login flow.".to_string()
                });
                return Ok(error_response(StatusCode::BAD_REQUEST, err));
            }
            session.xiaomi_result.clone()
        };
        let Some(result) = session_payload else {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "OAuth session not completed. Please restart the login flow.",
            ));
        };
        let uid = result.get("uid").and_then(Value::as_str);
        let access_token = result
            .get("sk")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if access_token.is_empty() {
            return Ok(error_response(StatusCode::BAD_REQUEST, "Xiaomi session result has no sk"));
        }
        let base_url = result
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("https://api.xiaomimimo.com/v1");

        // Desktop-exclusive Preview models authenticate with the account-session
        // passToken. shortcut: reading it needs the Desktop cookie store (locked
        // SQLite / OS keyring); left null so those models fall back to the API
        // key. Upgrade when Desktop exposes it without the lock.
        let db = nr_db::db()?;
        let created = nr_db::create_provider_connection(
            &db,
            &json!({
                "provider": "xiaomi-mimo",
                "authType": "oauth",
                "accessToken": access_token,
                "refreshToken": Value::Null,
                "expiresAt": iso_in_seconds(365 * 24 * 60 * 60),
                "email": uid.map(|u| format!("{u}@xiaomi")),
                "displayName": uid.map(|u| format!("Xiaomi {u}")).or_else(|| Some("Xiaomi MiMo".to_string())),
                "providerSpecificData": {
                    "uid": uid,
                    "baseUrl": base_url,
                    "authMethod": "oauth",
                    "mimoPassToken": Value::Null,
                    "mimoUserId": Value::Null,
                    "mimoCUserId": Value::Null,
                },
                "testStatus": "active",
            }),
        )?;
        // The modal consumed the session; clear it and stop the proxy.
        {
            let mut rt = runtime.lock().unwrap();
            rt.session = None;
        }
        let runtime2 = runtime.clone();
        tokio::spawn(async move {
            stop_proxy(&runtime2).await;
        });
        return Ok((
            StatusCode::OK,
            Json(json!({
                "success": true,
                "connection": {
                    "id": created.get("id").cloned().unwrap_or(Value::Null),
                    "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                    "email": created.get("email").cloned().unwrap_or(Value::Null),
                    "displayName": created.get("displayName").cloned().unwrap_or(Value::Null),
                }
            })),
        )
            .into_response());
    }

    let code = body
        .and_then(|b| b.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let redirect_uri = body
        .and_then(|b| b.get("redirectUri"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    let code_verifier = body
        .and_then(|b| b.get("codeVerifier"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    let state = body
        .and_then(|b| b.get("state"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    // ── perchai ───────────────────────────────────────────────────────────
    // Handles PKCE code, full callback URL, raw JWT, or JSON from
    // cli-auth-session.json — the provider accepts whatever is pasted.
    if provider == "perchai" {
        if code.is_empty() {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Missing token or authorization code",
            ));
        }
        let mapped = exchange_code(
            provider,
            &code,
            "http://localhost:8080/callback",
            code_verifier.as_deref(),
            state.as_deref(),
            None,
            log,
        )
        .await?;
        let mut conn = mapped.to_map();
        conn.insert("provider".into(), Value::String("perchai".into()));
        conn.insert("authType".into(), Value::String("oauth".into()));
        if let Some(expires_in) = mapped.expires_in {
            conn.insert(
                "expiresAt".into(),
                Value::String(iso_in_seconds(expires_in)),
            );
        }
        conn.insert("testStatus".into(), Value::String("active".into()));
        let db = nr_db::db()?;
        let created = nr_db::create_provider_connection(&db, &Value::Object(conn))?;
        return Ok((
            StatusCode::OK,
            Json(json!({
                "success": true,
                "connection": {
                    "id": created.get("id").cloned().unwrap_or(Value::Null),
                    "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                    "email": created.get("email").cloned().unwrap_or(Value::Null),
                    "displayName": created.get("displayName").cloned().unwrap_or(Value::Null),
                }
            })),
        )
            .into_response());
    }

    // ── trae / windsurf ───────────────────────────────────────────────────
    // Code is either a raw callback URL or a pasted token; no PKCE.
    if provider == "trae" || provider == "windsurf" {
        if code.is_empty() {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Missing token or callback URL",
            ));
        }
        let mapped = exchange_code(
            provider,
            &code,
            "http://localhost:8080/callback",
            None,
            state.as_deref(),
            None,
            log,
        )
        .await?;
        let mut conn = mapped.to_map();
        conn.insert("provider".into(), Value::String(provider.to_string()));
        conn.insert(
            "authType".into(),
            Value::String(if provider == "windsurf" { "api_key" } else { "oauth" }.into()),
        );
        if let Some(expires_in) = mapped.expires_in {
            conn.insert(
                "expiresAt".into(),
                Value::String(iso_in_seconds(expires_in)),
            );
        }
        conn.insert("testStatus".into(), Value::String("active".into()));
        let db = nr_db::db()?;
        let created = nr_db::create_provider_connection(&db, &Value::Object(conn))?;
        return Ok((
            StatusCode::OK,
            Json(json!({
                "success": true,
                "connection": {
                    "id": created.get("id").cloned().unwrap_or(Value::Null),
                    "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                    "email": created.get("email").cloned().unwrap_or(Value::Null),
                    "displayName": created.get("displayName").cloned().unwrap_or(Value::Null),
                }
            })),
        )
            .into_response());
    }

    // ── raw JWT access token ──────────────────────────────────────────────
    // Detect `eyJ…` tokens (ChatGPT website / Codex-style) and store them as
    // access_token connections without an OAuth exchange.
    if code.starts_with("eyJ") && code.contains('.') {
        let info = super::oauth_jwt::extract_codex_account_info(&code);
        let direct = super::oauth_jwt::decode_jwt_payload(&code).unwrap_or_else(|| json!({}));
        let account_id = info
            .get("chatgptAccountId")
            .or_else(|| direct.get("account_id"));
        let plan_type = info
            .get("chatgptPlanType")
            .or_else(|| direct.get("plan_type"));
        let email = info.get("email").or_else(|| direct.get("email"));
        let mut provider_specific_data = serde_json::Map::new();
        provider_specific_data.insert("authMethod".into(), json!("access_token"));
        if let Some(id) = account_id.filter(|v| !v.is_null()) {
            provider_specific_data.insert("chatgptAccountId".into(), id.clone());
        }
        if let Some(plan) = plan_type.filter(|v| !v.is_null()) {
            provider_specific_data.insert("chatgptPlanType".into(), plan.clone());
        }
        let db = nr_db::db()?;
        let created = nr_db::create_provider_connection(
            &db,
            &json!({
                "provider": provider,
                "authType": "access_token",
                "accessToken": code,
                "email": email.and_then(Value::as_str),
                "providerSpecificData": Value::Object(provider_specific_data),
                "testStatus": "active",
            }),
        )?;
        return Ok((
            StatusCode::OK,
            Json(json!({
                "success": true,
                "connection": {
                    "id": created.get("id").cloned().unwrap_or(Value::Null),
                    "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                    "email": created.get("email").cloned().unwrap_or(Value::Null),
                    "displayName": created.get("displayName").cloned().unwrap_or(Value::Null),
                }
            })),
        )
            .into_response());
    }

    // ── generic PKCE exchange ─────────────────────────────────────────────
    let no_pkce_exchange_providers = ["cline", "clinepass", "kimchi"];
    let redirect_uri = body
        .and_then(|b| b.get("redirectUri"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    let code_verifier = body
        .and_then(|b| b.get("codeVerifier"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    if code.is_empty()
        || (redirect_uri.is_none() && !no_pkce_exchange_providers.contains(&provider))
    {
        return Ok(error_response(StatusCode::BAD_REQUEST, "Missing required fields"));
    }
    if !no_pkce_exchange_providers.contains(&provider) && code_verifier.is_none() {
        return Ok(error_response(StatusCode::BAD_REQUEST, "Missing required fields"));
    }

    let redirect = redirect_uri.unwrap_or_else(|| "http://localhost:8080/callback".to_string());
    let mut meta = body.and_then(|b| b.get("meta")).cloned();
    if let Some(system_id) = body.and_then(|b| b.get("systemId")).and_then(Value::as_str) {
        let merged = meta.get_or_insert_with(|| json!({}));
        if merged.is_object() {
            merged["systemId"] = json!(system_id);
        }
    }

    let mapped = exchange_code(
        provider,
        &code,
        &redirect,
        code_verifier.as_deref(),
        state.as_deref(),
        meta.as_ref(),
        log,
    )
    .await?;

    let mut conn = mapped.to_map();
    let provider_id = if provider == "kimi-coding" { "kimi" } else { provider };
    conn.insert("provider".into(), Value::String(provider_id.to_string()));
    conn.insert("authType".into(), Value::String("oauth".into()));
    if let Some(expires_in) = mapped.expires_in {
        let expires_at = chrono::Utc::now() + chrono::Duration::seconds(expires_in);
        conn.insert(
            "expiresAt".into(),
            Value::String(expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
    }
    conn.insert("testStatus".into(), Value::String("active".into()));

    let db = nr_db::db()?;
    let created = nr_db::create_provider_connection(&db, &Value::Object(conn))?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "success": true,
            "connection": {
                "id": created.get("id").cloned().unwrap_or(Value::Null),
                "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                "email": created.get("email").cloned().unwrap_or(Value::Null),
                "displayName": created.get("displayName").cloned().unwrap_or(Value::Null),
            }
        })),
    )
        .into_response())
}

async fn handle_poll(
    provider: &str,
    body: Option<&Value>,
    log: &nr_core::Logger,
) -> nr_core::Result<Response> {
    if !supports_device_code(provider) {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "Provider does not support device code flow",
        ));
    }

    let device_code = body
        .and_then(|b| b.get("deviceCode"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let code_verifier = body
        .and_then(|b| b.get("codeVerifier"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    let extra = body.and_then(|b| b.get("extraData")).cloned();

    if device_code.is_empty() {
        return Ok(error_response(StatusCode::BAD_REQUEST, "Missing deviceCode"));
    }

    let result = poll_device_token(
        provider,
        &device_code,
        code_verifier.as_deref(),
        extra.as_ref(),
        log,
    )
    .await?;

    let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

    if ok {
        let tokens = result.get("data").cloned().unwrap_or(json!({}));
        let provider_for_map = if provider == "kimi-coding" { "kimi" } else { provider };
        let prov_obj = get_provider(provider_for_map)
            .ok_or_else(|| nr_core::Error::BadRequest(format!("Unknown provider: {}", provider_for_map)))?;

        // The grant succeeded, so the code is spent: run the post-login exchange
        // now (Muse mints its API key, grok-cli/nous enrich the profile) and treat
        // a failure as fatal rather than letting the client poll again.
        let extra = if tokens.get("access_token").and_then(Value::as_str).is_some() {
            match prov_obj.post_exchange(&tokens, None, log).await {
                Ok(extra) => extra,
                Err(err) => {
                    return Ok((
                        StatusCode::OK,
                        Json(json!({
                            "success": false,
                            "error": "exchange_failed",
                            "errorDescription": err.to_string(),
                            "fatal": true,
                        })),
                    )
                        .into_response())
                }
            }
        } else {
            None
        };
        let mapped = prov_obj.map_tokens(&tokens, extra.as_ref())?;

        let mut conn = mapped.to_map();
        let provider_id = if provider == "kimi-coding" { "kimi" } else { provider };
        conn.insert("provider".into(), Value::String(provider_id.to_string()));
        conn.insert("authType".into(), Value::String("oauth".into()));
        if let Some(expires_in) = mapped.expires_in {
            let expires_at = chrono::Utc::now() + chrono::Duration::seconds(expires_in);
            conn.insert(
                "expiresAt".into(),
                Value::String(expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            );
        }
        conn.insert("testStatus".into(), Value::String("active".into()));

        let db = nr_db::db()?;
        let created = nr_db::create_provider_connection(&db, &Value::Object(conn))?;

        return Ok((
            StatusCode::OK,
            Json(json!({
                "success": true,
                "connection": {
                    "id": created.get("id").cloned().unwrap_or(Value::Null),
                    "provider": created.get("provider").cloned().unwrap_or(Value::Null),
                }
            })),
        )
            .into_response());
    }

    let err_code = result
        .get("data")
        .and_then(|d| d.get("error"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let is_pending = err_code == "authorization_pending" || err_code == "slow_down";

    let mut resp = json!({
        "success": false,
        "error": err_code,
        "pending": is_pending,
    });
    if let Some(desc) = result
        .get("data")
        .and_then(|d| d.get("errorDescription"))
        .and_then(|v| v.as_str())
    {
        resp["errorDescription"] = Value::String(desc.to_string());
    }

    Ok((StatusCode::OK, Json(resp)).into_response())
}

// ─── Device Polling State Machine ───────────────────────────────────────────

/// Classification of a device-flow poll response, faithful to the JS client
/// contract: `success`, `pending`, `fatal` (unrecoverable → client must stop
/// polling and surface the error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePollState {
    Pending,
    SlowDown,
    Authorized,
    Fatal,
}

pub fn classify_device_poll(resp: &Value) -> DevicePollState {
    if resp.get("success").and_then(|v| v.as_bool()) == Some(true) {
        return DevicePollState::Authorized;
    }

    let err = resp
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if resp.get("fatal").and_then(|v| v.as_bool()) == Some(true) {
        return DevicePollState::Fatal;
    }

    match err {
        "authorization_pending" => DevicePollState::Pending,
        "slow_down" => DevicePollState::SlowDown,
        _ => DevicePollState::Fatal,
    }
}

// ─── Routes ─────────────────────────────────────────────────────────────────

pub fn routes() -> Router {
    Router::new()
        .route("/api/oauth/{provider}/{action}", get(dynamic_get).post(dynamic_post))
        // Static import surfaces (src/app/api/oauth/<provider>/<action>/route.js).
        .route("/api/oauth/cursor/import", get(cursor_import_instructions).post(cursor_import))
        .route("/api/oauth/kiro/import", post(kiro_import))
        .route("/api/oauth/kiro/import-cli-proxy", post(kiro_import_cli_proxy))
        .route("/api/oauth/kiro/api-key", post(kiro_api_key))
        .route("/api/oauth/kiro/social-authorize", get(kiro_social_authorize))
        .route("/api/oauth/kiro/social-exchange", post(kiro_social_exchange))
        .route("/api/oauth/iflow/cookie", post(iflow_cookie))
        .route("/api/oauth/zed/import", post(zed_import))
        .route("/api/oauth/xiaomi-mimo/api-key", post(xiaomi_mimo_api_key))
        .route("/api/oauth/xiaomi-mimo/login/start", post(xiaomi_mimo_login_start))
        .route("/api/oauth/xiaomi-mimo/login/status", get(xiaomi_mimo_login_status))
        .route("/api/oauth/perchai/plan/status", get(perchai_plan_status))
        .route("/api/oauth/perchai/plan/claim", post(perchai_plan_claim))
}

// ─── Static credential-import routes ────────────────────────────────────────
//
// Port of `src/app/api/oauth/<provider>/<action>/route.js`: each one validates
// a pasted credential against the provider's own API (or drives a short
// interactive session) and then writes a connection row. Paths, methods, body
// shapes and status codes match the JS handlers.

/// Test hook: `NR_OAUTH_ENDPOINT_<NAME>` replaces one upstream URL (base or full)
/// so tests can point the import routes at a local mock. Never set in production.
fn env_endpoint(name: &str) -> Option<String> {
    let var = format!("NR_OAUTH_ENDPOINT_{}", name.to_ascii_uppercase().replace('-', "_"));
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `env_endpoint` or a fallback base URL, without its trailing slashes.
fn endpoint_base(name: &str, fallback: &str) -> String {
    env_endpoint(name)
        .unwrap_or_else(|| fallback.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn import_error(status: StatusCode, message: impl Into<String>) -> Response {
    error_response(status, message)
}

fn db_handle() -> Result<nr_db::Db, Response> {
    nr_db::db().map_err(|e| import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn string_field<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

fn trimmed_field(body: &Value, key: &str) -> Option<String> {
    string_field(body, key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn import_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn save_connection(db: &nr_db::Db, payload: &Value) -> Response {
    match nr_db::create_provider_connection(db, payload) {
        Ok(conn) => import_response(
            StatusCode::OK,
            json!({
                "success": true,
                "connection": {
                    "id": conn.get("id").cloned().unwrap_or(Value::Null),
                    "provider": conn.get("provider").cloned().unwrap_or(Value::Null),
                    "email": conn.get("email").cloned().unwrap_or(Value::Null),
                    "displayName": conn.get("displayName").cloned().unwrap_or(Value::Null),
                },
            }),
        ),
        Err(e) => import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `new Date(Date.now() + seconds * 1000).toISOString()`.
fn iso_in_seconds(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `KiroService.extractEmailFromJWT` / `CursorService.extractUserInfo` share the
/// same claim order.
fn email_from_jwt(token: &str) -> Option<String> {
    super::oauth_jwt::extract_email_from_access_token(token)
}

// ─── cursor ─────────────────────────────────────────────────────────────────

/// GET /api/oauth/cursor/import — `CursorService.getTokenStorageInstructions`.
pub async fn cursor_import_instructions() -> Response {
    let config = nr_oauth::flows::constants::cursor_config();
    let paths = config.get("tokenStoragePaths").cloned().unwrap_or(json!({}));
    let path = |key: &str| {
        paths
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    import_response(
        StatusCode::OK,
        json!({
            "provider": "cursor",
            "method": "import_token",
            "instructions": {
                "title": "How to get your Cursor token",
                "steps": [
                    "1. Open Cursor IDE and make sure you're logged in",
                    "2. Find the state.vscdb file:",
                    format!("   - Linux: {}", path("linux")),
                    format!("   - macOS: {}", path("macos")),
                    format!("   - Windows: {}", path("windows")),
                    "3. Open the database with SQLite browser or CLI:",
                    "   sqlite3 state.vscdb \"SELECT value FROM itemTable WHERE key='cursorAuth/accessToken'\"",
                    "4. Also get the machine ID:",
                    "   sqlite3 state.vscdb \"SELECT value FROM itemTable WHERE key='storage.serviceMachineId'\"",
                    "5. Paste both values in the form below",
                ],
                "alternativeMethod": [
                    "Or use this one-liner to get both values:",
                    "sqlite3 state.vscdb \"SELECT key, value FROM itemTable WHERE key IN ('cursorAuth/accessToken', 'storage.serviceMachineId')\"",
                ],
            },
            "requiredFields": [
                {
                    "name": "accessToken",
                    "label": "Access Token",
                    "description": "From cursorAuth/accessToken in state.vscdb",
                    "type": "textarea",
                },
                {
                    "name": "machineId",
                    "label": "Machine ID",
                    "description": "From storage.serviceMachineId in state.vscdb",
                    "type": "text",
                },
            ],
        }),
    )
}

/// POST /api/oauth/cursor/import — `CursorService.validateImportToken`.
///
/// Cursor's API speaks connect+proto, so the token is only shape-checked here
/// and validated by the first real request (same as the JS build).
pub async fn cursor_import(Json(body): Json<Value>) -> Response {
    let Some(access_token) = string_field(&body, "accessToken").filter(|s| !s.is_empty()) else {
        return import_error(StatusCode::BAD_REQUEST, "Access token is required");
    };
    let Some(machine_id) = string_field(&body, "machineId").filter(|s| !s.is_empty()) else {
        return import_error(StatusCode::BAD_REQUEST, "Machine ID is required");
    };

    let access_token = access_token.trim();
    let machine_id = machine_id.trim();

    // Thrown errors become a 500 with `error.message`, like the JS catch block.
    if access_token.len() < 50 {
        return import_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid token format. Token appears too short.",
        );
    }
    let machine_digits: String = machine_id.chars().filter(|c| *c != '-').collect();
    let uuid_like = machine_digits.len() >= 32
        && machine_digits
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-');
    if !uuid_like {
        return import_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid machine ID format. Expected UUID format.",
        );
    }

    let user_info = super::oauth_jwt::decode_jwt_payload(access_token);
    let email = user_info.as_ref().and_then(|p| {
        p.get("email")
            .or_else(|| p.get("sub"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let user_id = user_info.as_ref().and_then(|p| {
        p.get("sub")
            .or_else(|| p.get("user_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable");
    };

    save_connection(
        &db,
        &json!({
            "provider": "cursor",
            "authType": "oauth",
            "accessToken": access_token,
            "refreshToken": Value::Null,
            "expiresAt": iso_in_seconds(86_400),
            "email": email,
            "providerSpecificData": {
                "machineId": machine_id,
                "authMethod": "imported",
                "provider": "Imported",
                "userId": user_id,
            },
            "testStatus": "active",
        }),
    )
}

// ─── iflow ──────────────────────────────────────────────────────────────────

const IFLOW_PLATFORM: &str = "https://platform.iflow.cn";

/// Browser-ish headers the platform checks before it hands out the API key.
fn iflow_headers(cookie: &str) -> nr_core::http::HeaderList {
    let mut headers = nr_core::http::HeaderList::new();
    headers.insert("Cookie".into(), cookie.to_string());
    headers.insert(
        "Accept".into(),
        "application/json, text/plain, */*".into(),
    );
    headers.insert(
        "User-Agent".into(),
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36".into(),
    );
    headers.insert("Accept-Language".into(), "zh-CN,zh;q=0.9,en;q=0.8".into());
    headers
}

/// POST /api/oauth/iflow/cookie — exchange a `BXAuth` cookie for an API key.
pub async fn iflow_cookie(Json(body): Json<Value>) -> Response {
    let Some(cookie) = string_field(&body, "cookie") else {
        return import_error(StatusCode::BAD_REQUEST, "Cookie is required");
    };
    let trimmed = cookie.trim();
    if !trimmed.contains("BXAuth=") {
        return import_error(
            StatusCode::BAD_REQUEST,
            "Cookie must contain BXAuth field",
        );
    }
    let normalized = if trimmed.ends_with(';') {
        trimmed.to_string()
    } else {
        format!("{trimmed};")
    };

    let base = endpoint_base("iflow", IFLOW_PLATFORM);
    let api_key_url = match env_endpoint("iflow-apikey") {
        Some(full) => full,
        None => format!("{base}/api/openapi/apikey"),
    };

    // Step 1: read the key name (the POST needs it to mint the key).
    let get_response = nr_core::http::proxy_fetch(
        {
            let mut req = nr_core::http::FetchRequest::get(api_key_url.clone());
            req.headers = iflow_headers(&normalized);
            req.header_timeout = Some(std::time::Duration::from_secs(20));
            req
        },
        None,
    )
    .await;
    let get_result = match get_response {
        Ok(r) if r.ok() => r.json_body().await.unwrap_or(Value::Null),
        Ok(r) => {
            let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let text = r.text().await.unwrap_or_default();
            return import_error(status, format!("Failed to fetch API key info: {text}"));
        }
        Err(e) => return import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if !nr_core::json::truthy(get_result.get("success").unwrap_or(&Value::Null)) {
        let message = get_result
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        return import_error(
            StatusCode::BAD_REQUEST,
            format!("API key fetch failed: {message}"),
        );
    }
    let key_data = get_result.get("data").cloned().unwrap_or(Value::Null);
    let Some(name) = string_field(&key_data, "name").filter(|s| !s.is_empty()) else {
        return import_error(StatusCode::BAD_REQUEST, "Missing name in API key info");
    };

    // Step 2: refresh (mint) the API key for that name.
    let post_response = nr_core::http::proxy_fetch(
        {
            let mut req = nr_core::http::FetchRequest::post_json(
                api_key_url,
                iflow_headers(&normalized),
                &json!({ "name": name }),
            );
            req.headers.insert("Origin".into(), base.clone());
            req.headers.insert("Referer".into(), format!("{base}/"));
            req.header_timeout = Some(std::time::Duration::from_secs(20));
            req
        },
        None,
    )
    .await;
    let post_result = match post_response {
        Ok(r) if r.ok() => r.json_body().await.unwrap_or(Value::Null),
        Ok(r) => {
            let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let text = r.text().await.unwrap_or_default();
            return import_error(status, format!("Failed to refresh API key: {text}"));
        }
        Err(e) => return import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if !nr_core::json::truthy(post_result.get("success").unwrap_or(&Value::Null)) {
        let message = post_result
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        return import_error(
            StatusCode::BAD_REQUEST,
            format!("API key refresh failed: {message}"),
        );
    }
    let refreshed = post_result.get("data").cloned().unwrap_or(Value::Null);
    let Some(api_key) = string_field(&refreshed, "apiKey").filter(|s| !s.is_empty()) else {
        return import_error(StatusCode::BAD_REQUEST, "Missing API key in response");
    };

    // Persist only `BXAuth` — the platform rejects a jar full of other cookies.
    let bx_auth = normalized
        .find("BXAuth=")
        .map(|start| &normalized[start + "BXAuth=".len()..])
        .map(|rest| rest.split(';').next().unwrap_or(""))
        .unwrap_or("");
    let cookie_to_save = if bx_auth.is_empty() {
        String::new()
    } else {
        format!("BXAuth={bx_auth};")
    };

    let resolved_name = trimmed_field(&refreshed, "name")
        .or_else(|| Some(name.to_string()))
        .unwrap_or_default();
    let expire_time = refreshed.get("expireTime").cloned().unwrap_or(Value::Null);

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable");
    };

    let payload = json!({
        "provider": "iflow",
        "authType": "cookie",
        "name": resolved_name,
        "email": resolved_name,
        "apiKey": api_key,
        "providerSpecificData": {
            "cookie": cookie_to_save,
            "expireTime": expire_time,
        },
        "testStatus": "active",
        "isActive": true,
    });

    match nr_db::create_provider_connection(&db, &payload) {
        // The response masks the key: only the first 10 characters leak back.
        Ok(conn) => {
            let masked: String = api_key.chars().take(10).collect();
            import_response(
                StatusCode::OK,
                json!({
                    "success": true,
                    "connection": {
                        "id": conn.get("id").cloned().unwrap_or(Value::Null),
                        "provider": conn.get("provider").cloned().unwrap_or(json!("iflow")),
                        "email": conn.get("email").cloned().unwrap_or(Value::Null),
                        "apiKey": format!("{masked}..."),
                        "expireTime": expire_time,
                    },
                }),
            )
        }
        Err(e) => import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ─── kiro ───────────────────────────────────────────────────────────────────

const KIRO_SOCIAL_HOST: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
const KIRO_SOCIAL_REDIRECT: &str = "kiro://kiro.kiroAgent/authenticate-success";

/// POST /api/oauth/kiro/import — import an IDE refresh token.
pub async fn kiro_import(Json(body): Json<Value>) -> Response {
    let Some(refresh_token) = trimmed_field(&body, "refreshToken").filter(|s| !s.is_empty()) else {
        return import_error(StatusCode::BAD_REQUEST, "Refresh token is required");
    };

    let client_id = trimmed_field(&body, "clientId");
    let client_secret = trimmed_field(&body, "clientSecret");
    let is_idc = client_id.is_some() && client_secret.is_some();
    let region = trimmed_field(&body, "region");

    let mut psd = serde_json::Map::new();
    if is_idc {
        psd.insert("clientId".into(), json!(client_id));
        psd.insert("clientSecret".into(), json!(client_secret));
        psd.insert("region".into(), json!(region.clone().unwrap_or_else(|| "us-east-1".into())));
        psd.insert("authMethod".into(), json!("idc"));
    }

    let log = nr_core::Logger::new();
    let Some(token_data) = nr_oauth::token_refresh::refresh_kiro_token(
        &refresh_token,
        (!psd.is_empty()).then(|| psd.clone()).as_ref(),
        &log,
        None,
    )
    .await
    else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "Token refresh failed");
    };

    let access_token = token_data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let resolved_refresh = token_data
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or(&refresh_token)
        .to_string();
    let expires_in = token_data.get("expiresIn").and_then(Value::as_i64).unwrap_or(3600);
    let token_arn = token_data
        .get("providerSpecificData")
        .and_then(|p| p.get("profileArn"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let profile_arn = trimmed_field(&body, "profileArn").or(token_arn);
    let email = email_from_jwt(&access_token);

    let resolved_auth_method = if is_idc { "idc" } else { "imported" };
    let provider_label = if is_idc { "Enterprise" } else { "Imported" };

    let mut out_psd = serde_json::Map::new();
    out_psd.insert("profileArn".into(), json!(profile_arn));
    out_psd.insert("authMethod".into(), json!(resolved_auth_method));
    out_psd.insert("provider".into(), json!(provider_label));
    if is_idc {
        out_psd.insert("clientId".into(), json!(client_id));
        out_psd.insert("clientSecret".into(), json!(client_secret));
        out_psd.insert("region".into(), json!(region.unwrap_or_else(|| "us-east-1".into())));
    }

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable");
    };

    save_connection(
        &db,
        &json!({
            "provider": "kiro",
            "authType": "oauth",
            "accessToken": access_token,
            "refreshToken": resolved_refresh,
            "expiresAt": iso_in_seconds(expires_in),
            "email": email,
            "providerSpecificData": Value::Object(out_psd),
            "testStatus": "active",
        }),
    )
}

const MICROSOFT_TOKEN_HOSTS: &[&str] =
    &["login.microsoftonline.com", "login.microsoft.com", "login.windows.net"];

/// `validateMicrosoftTokenEndpoint`: https + a Microsoft login host only, so a
/// hostile saved/imported endpoint can never be used as an SSRF pivot.
fn validate_microsoft_token_endpoint(raw: &str) -> Result<String, String> {
    let endpoint = raw.trim();
    if endpoint.is_empty() {
        return Err("token_endpoint is required".into());
    }
    let parsed = url::Url::parse(endpoint).map_err(|_| "token_endpoint must be a valid URL")?;
    if parsed.scheme() != "https" {
        return Err("token_endpoint must use https".into());
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if !MICROSOFT_TOKEN_HOSTS.contains(&host.as_str()) {
        return Err("token_endpoint must be a Microsoft login endpoint".into());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

/// `normalizeScope(scopes)`: an array joins with spaces, a string is trimmed.
fn normalize_scope(scopes: Option<&Value>) -> String {
    match scopes {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn pick(input: &Value, camel: &str, snake: &str) -> Option<String> {
    trimmed_field(input, camel).or_else(|| trimmed_field(input, snake))
}

/// `resolveExpiresAt(input)`: explicit date → `expires_in` → JWT `exp` → +1h.
fn resolve_expires_at(input: &Value) -> String {
    for key in ["expired", "expires_at", "expiresAt"] {
        if let Some(explicit) = input.get(key).filter(|v| !v.is_null()) {
            let parsed = match explicit {
                Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|d| d.with_timezone(&chrono::Utc)),
                Value::Number(n) => n
                    .as_i64()
                    .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms)),
                _ => None,
            };
            if let Some(at) = parsed {
                return at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            }
        }
    }

    let expires_in = input
        .get("expires_in")
        .or_else(|| input.get("expiresIn"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if expires_in > 0 {
        return iso_in_seconds(expires_in);
    }

    let token = input
        .get("access_token")
        .or_else(|| input.get("accessToken"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(exp) = super::oauth_jwt::decode_jwt_payload(token)
        .and_then(|p| p.get("exp").and_then(Value::as_i64))
    {
        if let Some(at) = chrono::DateTime::from_timestamp(exp, 0) {
            return at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        }
    }

    iso_in_seconds(3600)
}

/// POST /api/oauth/kiro/import-cli-proxy — `normalizeKiroExternalIdpAuth`.
///
/// Accepts CLIProxyAPI's Microsoft `external_idp` auth JSON (object or string)
/// under `cliProxyAuth` / `auth` / `json`, or as the whole body.
pub async fn kiro_import_cli_proxy(Json(body): Json<Value>) -> Response {
    let raw = body
        .get("cliProxyAuth")
        .or_else(|| body.get("auth"))
        .or_else(|| body.get("json"))
        .cloned()
        .unwrap_or_else(|| body.clone());

    let input = match raw {
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(parsed) => parsed,
            Err(_) => {
                return import_error(StatusCode::BAD_REQUEST, "CLIProxyAPI auth JSON is invalid")
            }
        },
        other => other,
    };

    if !input.is_object() {
        return import_error(StatusCode::BAD_REQUEST, "CLIProxyAPI auth JSON is required");
    }

    let auth_method = pick(&input, "authMethod", "auth_method");
    if let Some(method) = &auth_method {
        if method != "external_idp" {
            return import_error(
                StatusCode::BAD_REQUEST,
                "Only external_idp Kiro auth is supported by this importer",
            );
        }
    }

    let access_token = pick(&input, "accessToken", "access_token");
    let refresh_token = pick(&input, "refreshToken", "refresh_token");
    let client_id = pick(&input, "clientId", "client_id");
    let profile_arn = pick(&input, "profileArn", "profile_arn");
    let region = trimmed_field(&input, "region").unwrap_or_else(|| "us-east-1".into());
    let scope = normalize_scope(input.get("scopes").or_else(|| input.get("scope")));

    let token_endpoint = match validate_microsoft_token_endpoint(
        &pick(&input, "tokenEndpoint", "token_endpoint").unwrap_or_default(),
    ) {
        Ok(endpoint) => endpoint,
        Err(message) => return import_error(StatusCode::BAD_REQUEST, message),
    };

    for (value, message) in [
        (&access_token, "access_token is required"),
        (&refresh_token, "refresh_token is required"),
        (&client_id, "client_id is required"),
        (&profile_arn, "profile_arn is required"),
    ] {
        if value.is_none() {
            return import_error(StatusCode::BAD_REQUEST, message);
        }
    }
    if scope.is_empty() {
        return import_error(StatusCode::BAD_REQUEST, "scopes is required");
    }

    let access_token = access_token.unwrap_or_default();
    // `input.email || payload.email || … || payload.sub || null`
    let email = trimmed_field(&input, "email").or_else(|| {
        let payload = super::oauth_jwt::decode_jwt_payload(&access_token)?;
        for key in ["email", "preferred_username", "upn", "sub"] {
            if let Some(value) = payload.get(key).and_then(Value::as_str) {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    });

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "CLIProxyAPI import failed");
    };

    save_connection(
        &db,
        &json!({
            "provider": "kiro",
            "authType": "oauth",
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": resolve_expires_at(&input),
            "email": email,
            "providerSpecificData": {
                "profileArn": profile_arn,
                "region": region,
                "authMethod": "external_idp",
                "provider": "CLIProxyAPI",
                "clientId": client_id,
                "tokenEndpoint": token_endpoint,
                "scope": scope,
            },
            "testStatus": "active",
        }),
    )
}

/// `assertValidAwsRegion` of `src/lib/oauth/constants/oauth.js` (GHSA-6mwv-4mrm-5p3m:
/// a region is interpolated into an upstream URL, so it must be an AWS region).
fn validate_aws_region(region: &str) -> bool {
    nr_oauth::flows::constants::assert_valid_aws_region(region).is_ok()
}

/// `KiroService.listAvailableApiKeyModels`: the Amazon Q catalog proves the key
/// can actually run inference (ListAvailableProfiles answers 200 for junk keys).
async fn validate_kiro_api_key(api_key: &str, region: &str) -> Result<(), String> {
    if !validate_aws_region(region) {
        return Err("Invalid region".into());
    }

    let url = match env_endpoint("kiro-q-models") {
        Some(full) => full,
        None => format!("https://q.{region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR"),
    };
    let response = nr_core::http::proxy_fetch(
        {
            let mut req = nr_core::http::FetchRequest::get(url);
            req.headers.insert("Authorization".into(), format!("Bearer {api_key}"));
            req.headers.insert("TokenType".into(), "API_KEY".into());
            req.headers.insert("Accept".into(), "application/json".into());
            req.headers.insert("User-Agent".into(), "AWS-SDK-JS/3.0.0 kiro-ide/1.0.0".into());
            req.headers.insert("X-Amz-User-Agent".into(), "aws-sdk-js/3.0.0 kiro-ide/1.0.0".into());
            req.header_timeout = Some(std::time::Duration::from_secs(20));
            req
        },
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    if !response.ok() {
        return Err("API key validation failed".into());
    }
    let data = response.json_body().await.map_err(|e| e.to_string())?;
    let models = data
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if models.is_empty() {
        return Err("API key returned no available models".into());
    }
    Ok(())
}

/// POST /api/oauth/kiro/api-key — headless bearer credential, no refresh token.
pub async fn kiro_api_key(Json(body): Json<Value>) -> Response {
    let Some(api_key) = string_field(&body, "apiKey")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return import_error(StatusCode::BAD_REQUEST, "API key is required");
    };
    let region = trimmed_field(&body, "region").unwrap_or_else(|| "us-east-1".into());

    if let Err(message) = validate_kiro_api_key(api_key, &region).await {
        // Do not reflect the upstream body: it can echo the key.
        let _ = message;
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "API key validation failed");
    }

    let email = email_from_jwt(api_key);
    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "API key validation failed");
    };

    // API keys never expire on a fixed schedule; the far horizon skips the
    // proactive refresh path (which needs a refreshToken anyway).
    save_connection(
        &db,
        &json!({
            "provider": "kiro",
            "authType": "api_key",
            "accessToken": api_key,
            "refreshToken": Value::Null,
            "expiresAt": iso_in_seconds(365 * 24 * 60 * 60),
            "email": email,
            "providerSpecificData": {
                "region": region,
                "authMethod": "api_key",
                "provider": "API Key",
            },
            "testStatus": "active",
        }),
    )
}

/// `KiroService.buildSocialLoginUrl`: AWS Cognito only whitelists `kiro://`.
fn kiro_social_login_url(provider: &str, code_challenge: &str, state: &str) -> String {
    let idp = if provider == "google" { "Google" } else { "Github" };
    let host = endpoint_base("kiro-auth", KIRO_SOCIAL_HOST);
    format!(
        "{host}/login?idp={idp}&redirect_uri={}&code_challenge={code_challenge}&code_challenge_method=S256&state={state}&prompt=select_account",
        urlencoding::encode(KIRO_SOCIAL_REDIRECT),
    )
}

/// GET /api/oauth/kiro/social-authorize
pub async fn kiro_social_authorize(Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(provider) = params.get("provider").map(|s| s.as_str()) else {
        return import_error(
            StatusCode::BAD_REQUEST,
            "Invalid provider. Use 'google' or 'github'",
        );
    };
    if provider != "google" && provider != "github" {
        return import_error(
            StatusCode::BAD_REQUEST,
            "Invalid provider. Use 'google' or 'github'",
        );
    }

    let (code_verifier, code_challenge, state) =
        nr_oauth::flows::utils::generate_pkce(32);
    import_response(
        StatusCode::OK,
        json!({
            "authUrl": kiro_social_login_url(provider, &code_challenge, &state),
            "state": state,
            "codeVerifier": code_verifier,
            "codeChallenge": code_challenge,
            "provider": provider,
        }),
    )
}

/// POST /api/oauth/kiro/social-exchange — the callback lands on `kiro://`.
pub async fn kiro_social_exchange(Json(body): Json<Value>) -> Response {
    let code = string_field(&body, "code").filter(|s| !s.is_empty());
    let code_verifier = string_field(&body, "codeVerifier").filter(|s| !s.is_empty());
    if code.is_none() || code_verifier.is_none() {
        return import_error(StatusCode::BAD_REQUEST, "Missing required fields");
    }
    let provider = string_field(&body, "provider").unwrap_or("");
    if provider != "google" && provider != "github" {
        return import_error(StatusCode::BAD_REQUEST, "Invalid provider");
    }

    let host = endpoint_base("kiro-auth", KIRO_SOCIAL_HOST);
    let response = nr_core::http::proxy_fetch(
        nr_core::http::FetchRequest::post_json(
            format!("{host}/oauth/token"),
            nr_core::http::HeaderList::new(),
            &json!({
                "code": code,
                "code_verifier": code_verifier,
                "redirect_uri": KIRO_SOCIAL_REDIRECT,
            }),
        ),
        None,
    )
    .await;

    let response = match response {
        Ok(r) if r.ok() => r,
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            return import_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Token exchange failed: {text}"),
            );
        }
        Err(e) => return import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let data = match response.json_body().await {
        Ok(data) => data,
        Err(e) => return import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_in = data
        .get("expiresIn")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let email = email_from_jwt(&access_token);
    let label = format!("{}{}", &provider[..1].to_uppercase(), &provider[1..]);

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable");
    };

    save_connection(
        &db,
        &json!({
            "provider": "kiro",
            "authType": "oauth",
            "accessToken": access_token,
            "refreshToken": data.get("refreshToken").cloned().unwrap_or(Value::Null),
            "expiresAt": iso_in_seconds(expires_in),
            "email": email,
            "providerSpecificData": {
                "profileArn": data.get("profileArn").cloned().unwrap_or(Value::Null),
                "authMethod": provider,
                "provider": label,
            },
            "testStatus": "active",
        }),
    )
}

// ─── zed ────────────────────────────────────────────────────────────────────

/// `resolveZedOrganizationId` for the import path: an explicit value on the
/// request wins, then the user profile, then the personal/first organization.
fn resolve_zed_organization_id(body: &Value, user_info: &Value) -> String {
    let normalize = |value: Option<&Value>| -> String {
        match value {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(items)) => items
                .first()
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Some(Value::Object(map)) => map
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        }
    };

    for key in ["organizationId", "defaultOrganizationId"] {
        let explicit = normalize(body.get(key));
        if !explicit.is_empty() {
            return explicit;
        }
    }
    for key in ["default_organization_id", "defaultOrganizationId"] {
        let from_user = normalize(user_info.get(key));
        if !from_user.is_empty() {
            return from_user;
        }
    }

    let orgs = user_info
        .get("organizations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let org = orgs
        .iter()
        .find(|o| o.get("is_personal").and_then(Value::as_bool) == Some(true))
        .or_else(|| orgs.first());
    org.and_then(|o| o.get("id"))
        .map(|v| normalize(Some(v)))
        .unwrap_or_default()
}

/// POST /api/oauth/zed/import — validate a Zed session and save it.
pub async fn zed_import(Json(body): Json<Value>) -> Response {
    let access_token = string_field(&body, "accessToken")
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    // `body.userId != null ? String(body.userId).trim() : ""`
    let user_id = match body.get("userId") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => other
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| other.to_string()),
    };
    let system_id = trimmed_field(&body, "systemId")
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    if access_token.is_empty() {
        return import_error(StatusCode::BAD_REQUEST, "Access token is required");
    }
    if user_id.is_empty() {
        return import_error(StatusCode::BAD_REQUEST, "User id is required");
    }

    let credentials = nr_core::Credentials::from_value(json!({
        "accessToken": access_token,
        "providerSpecificData": { "userId": user_id, "systemId": system_id },
    }));
    let base = endpoint_base("zed-cloud", nr_exec_ide::zed_auth::ZED_CLOUD_BASE_URL);
    let user_info =
        match nr_exec_ide::zed_auth::fetch_zed_authenticated_user(&credentials, &base, None).await {
            Ok(info) => info,
            Err(e) => return import_error(StatusCode::UNAUTHORIZED, e.to_string()),
        };

    let organization_id = resolve_zed_organization_id(&body, &user_info);
    let email = user_info
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string);
    let display_name = ["name", "display_name", "username"]
        .iter()
        .find_map(|key| user_info.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Zed {user_id}"));

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable");
    };

    save_connection(
        &db,
        &json!({
            "provider": "zed",
            "authType": "oauth",
            "accessToken": access_token,
            "refreshToken": Value::Null,
            "expiresAt": Value::Null,
            "email": email,
            "displayName": display_name,
            "providerSpecificData": {
                "authMethod": "imported",
                "userId": user_id,
                "systemId": system_id,
                "organizationId": organization_id,
            },
            "testStatus": "active",
        }),
    )
}

// ─── xiaomi-mimo ────────────────────────────────────────────────────────────

/// `POST /api/oauth/xiaomi-mimo/api-key`.
///
/// A `sk-` key is validated against `/models` and stored; a session-only body
/// (`mimoPassToken` without a key) is stored as an OAuth-shaped connection so
/// the executor can do its own SSO handshake.
pub async fn xiaomi_mimo_api_key(Json(body): Json<Value>) -> Response {
    let key = string_field(&body, "apiKey")
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let pass_token = trimmed_field(&body, "mimoPassToken");
    let session_only = key.is_empty() && pass_token.is_some();

    if key.is_empty() && pass_token.is_none() {
        return import_error(StatusCode::BAD_REQUEST, "API key is required");
    }
    if !key.is_empty() && !key.starts_with("sk-") {
        return import_error(
            StatusCode::BAD_REQUEST,
            "Invalid key format — expected sk- prefix",
        );
    }

    let effective_base_url = trimmed_field(&body, "baseUrl")
        .unwrap_or_else(|| "https://api.xiaomimimo.com/v1".to_string())
        .trim_end_matches('/')
        .to_string();

    // Validate against /models; a network error still allows the import (the
    // key may be fine but the host blocked), it is just marked untested.
    let mut validated = false;
    let mut model_count = 0i64;
    if !key.is_empty() {
        let url = match env_endpoint("xiaomi-mimo-models") {
            Some(full) => {
                if !full.ends_with("/models") {
                    format!("{}/models", full.trim_end_matches('/'))
                } else {
                    full
                }
            }
            None => format!("{effective_base_url}/models"),
        };
        let mut req = nr_core::http::FetchRequest::get(url);
        req.headers.insert("Authorization".into(), format!("Bearer {key}"));
        req.headers.insert("X-Mimo-Source".into(), "mimocode-cli".into());
        req.header_timeout = Some(std::time::Duration::from_secs(10));
        if let Ok(response) = nr_core::http::proxy_fetch(req, None).await {
            if response.ok() {
                let data = response.json_body().await.unwrap_or(Value::Null);
                model_count = data
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as i64)
                    .unwrap_or(0);
                validated = true;
            }
        }
    }

    let uid = trimmed_field(&body, "uid");
    let region = trimmed_field(&body, "region");
    let mimo_user_id = trimmed_field(&body, "mimoUserId");
    let mimo_c_user_id = trimmed_field(&body, "mimoCUserId");

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "API key import failed");
    };

    // Dedup: same uid, same key, or same session identity in the same region.
    let existing = nr_db::get_provider_connections(&db, None)
        .unwrap_or_default()
        .into_iter()
        .find(|c| {
            if c.get("provider").and_then(Value::as_str) != Some("xiaomi-mimo") {
                return false;
            }
            let psd = c.get("providerSpecificData").cloned().unwrap_or(json!({}));
            let uid_match = uid
                .as_ref()
                .map(|u| c.get("email").and_then(Value::as_str) == Some(format!("{u}@xiaomi").as_str()))
                .unwrap_or(false);
            let key_match = !key.is_empty()
                && c.get("accessToken").and_then(Value::as_str) == Some(key.as_str());
            let session_match = session_only
                && mimo_user_id.is_some()
                && psd.get("mimoUserId").and_then(Value::as_str)
                    == mimo_user_id.as_deref()
                && region
                    .as_ref()
                    .map(|r| {
                        psd.get("region")
                            .and_then(Value::as_str)
                            .unwrap_or("cn")
                            == r.as_str()
                    })
                    .unwrap_or(true);
            uid_match || key_match || session_match
        });

    if let Some(existing) = existing {
        let id = existing.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let mut psd = existing
            .get("providerSpecificData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let pick_str = |incoming: Option<&String>, prior: &serde_json::Map<String, Value>, key: &str| {
            json!(incoming
                .cloned()
                .or_else(|| prior.get(key).and_then(Value::as_str).map(str::to_string)))
        };

        psd.insert(
            "uid".into(),
            pick_str(uid.as_ref(), &psd.clone(), "uid"),
        );
        psd.insert(
            "baseUrl".into(),
            json!(if key.is_empty() {
                psd.get("baseUrl").and_then(Value::as_str).map(str::to_string)
            } else {
                Some(effective_base_url.clone())
            }),
        );
        psd.insert(
            "region".into(),
            json!(region.clone().or_else(|| psd.get("region").and_then(Value::as_str).map(str::to_string)).unwrap_or_else(|| "cn".into())),
        );
        psd.insert(
            "authMethod".into(),
            json!(if session_only {
                "session"
            } else {
                psd.get("authMethod").and_then(Value::as_str).unwrap_or("api_key")
            }),
        );
        for (field, incoming) in [
            ("mimoPassToken", pass_token.as_ref()),
            ("mimoUserId", mimo_user_id.as_ref()),
            ("mimoCUserId", mimo_c_user_id.as_ref()),
        ] {
            let prior = psd.get(field).and_then(Value::as_str).map(str::to_string);
            psd.insert(field.into(), json!(incoming.cloned().or(prior)));
        }
        psd.insert("modelCount".into(), json!(model_count));

        let access_token = if key.is_empty() {
            existing.get("accessToken").cloned().unwrap_or(Value::Null)
        } else {
            json!(key)
        };

        let patch = json!({
            "accessToken": access_token,
            "providerSpecificData": Value::Object(psd),
            "testStatus": if validated {
                json!("active")
            } else {
                existing.get("testStatus").cloned().unwrap_or(json!("untested"))
            },
        });

        return match nr_db::update_provider_connection(&db, &id, &patch) {
            Ok(Some(conn)) => import_response(
                StatusCode::OK,
                json!({
                    "success": true,
                    "validated": validated,
                    "modelCount": model_count,
                    "updated": true,
                    "connection": {
                        "id": conn.get("id").cloned().unwrap_or(Value::Null),
                        "provider": conn.get("provider").cloned().unwrap_or(Value::Null),
                        "email": conn.get("email").cloned().unwrap_or(Value::Null),
                        "displayName": conn.get("displayName").cloned().unwrap_or(Value::Null),
                    },
                }),
            ),
            Ok(None) => import_error(StatusCode::INTERNAL_SERVER_ERROR, "API key import failed"),
            Err(e) => import_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
    }

    let display_name = match &uid {
        Some(uid) => format!("Xiaomi {uid}{}", if session_only { " (Session)" } else { "" }),
        None => "Xiaomi MiMo".to_string(),
    };

    let payload = json!({
        // "oauth" is the authType for imported credential connections; the list
        // card and filters key off it, so never invent new values.
        "provider": "xiaomi-mimo",
        "authType": if session_only { "oauth" } else { "api_key" },
        "accessToken": if key.is_empty() { Value::Null } else { json!(key) },
        "refreshToken": Value::Null,
        "expiresAt": iso_in_seconds(365 * 24 * 60 * 60),
        "email": uid.clone().map(|u| format!("{u}@xiaomi")),
        "displayName": display_name,
        "providerSpecificData": {
            "uid": uid,
            "baseUrl": effective_base_url,
            "authMethod": if session_only { "session" } else { "api_key" },
            "provider": if session_only { "Session Login" } else { "API Key" },
            "region": region.unwrap_or_else(|| "cn".into()),
            "modelCount": model_count,
            "mimoPassToken": pass_token,
            "mimoUserId": mimo_user_id,
            "mimoCUserId": mimo_c_user_id,
        },
        "testStatus": if validated {
            "active"
        } else if session_only {
            "active"
        } else {
            "untested"
        },
    });

    match nr_db::create_provider_connection(&db, &payload) {
        Ok(conn) => import_response(
            StatusCode::OK,
            json!({
                "success": true,
                "validated": validated,
                "modelCount": model_count,
                "connection": {
                    "id": conn.get("id").cloned().unwrap_or(Value::Null),
                    "provider": conn.get("provider").cloned().unwrap_or(Value::Null),
                    "email": conn.get("email").cloned().unwrap_or(Value::Null),
                    "displayName": conn.get("displayName").cloned().unwrap_or(Value::Null),
                },
            }),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "xiaomi-mimo api-key import failed");
            import_error(StatusCode::INTERNAL_SERVER_ERROR, "API key import failed")
        }
    }
}

// ─── xiaomi-mimo interactive login ──────────────────────────────────────────
//
// Two-call session: `login/start` opens a server-side session and walks the
// first two hops of MiMo Desktop's login surface, `login/status` polls that
// session. State lives in a process-local map (not in a cookie like the JS
// build, which carries it in `9r_mimo_login` because Next runs route handlers
// and `src/proxy.js` in separate bundles).
//
// shortcut: no `/__mimo_login/mimo/*` reverse proxy, so account.xiaomi.com
// traffic never flows through this process — a browser sign-in cannot deposit
// its passToken in our jar. `start` still resolves the cluster's SSO page (so
// region reachability is real) and `status` reports the identity if the jar
// holds one; otherwise the user imports the session through
// `POST /api/oauth/xiaomi-mimo/api-key` with `mimoPassToken`. Upgrade when the
// proxy lands.

const MIMO_API_UA: &str =
    "miNative PC/Normal Windows_NT/10.0.19045 SDKV/1.0.0 DEVT/PC DEVS/Windows APP/miaccount_desktop APPV/0.1.0";
const MIMO_SSO_UA: &str = "MiClaw/1.0";
const MIMO_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

const MIMO_CLUSTERS: &[(&str, &str)] = &[
    ("cn", "https://mimo-server-cn.xiaomimimo.com"),
    ("sgp", "https://mimo-server-sgp.xiaomimimo.com"),
    ("ams", "https://mimo-server-ams.xiaomimimo.com"),
    ("ru", "https://mimo-server-ru.xiaomimimo.com"),
    ("in", "https://mimo-server-in.xiaomimimo.com"),
];
const MIMO_DEFAULT_REGION: &str = "sgp";

#[derive(Clone, Debug)]
struct MimoCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
}

#[derive(Clone, Debug)]
struct MimoLoginSession {
    state: String,
    region: String,
    upstream_base: String,
    jar: Vec<MimoCookie>,
    created: std::time::Instant,
}

static MIMO_SESSIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, MimoLoginSession>>,
> = std::sync::LazyLock::new(Default::default);

fn mimo_region_base(region: &str) -> String {
    MIMO_CLUSTERS
        .iter()
        .find(|(name, _)| *name == region)
        .map(|(_, base)| *base)
        .unwrap_or_else(|| {
            MIMO_CLUSTERS
                .iter()
                .find(|(name, _)| *name == MIMO_DEFAULT_REGION)
                .map(|(_, base)| *base)
                .unwrap_or("https://mimo-server-sgp.xiaomimimo.com")
        })
        .to_string()
}

fn mimo_domain_matches(cookie_domain: &str, host: &str) -> bool {
    let domain = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// `cookieHeaderFor(sess, url)`: the cookies scoped to `url`.
fn mimo_cookie_header(jar: &[MimoCookie], url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return String::new();
    };
    let host = parsed.host_str().unwrap_or("");
    let path = parsed.path();
    jar.iter()
        .filter(|c| mimo_domain_matches(&c.domain, host) && path.starts_with(&c.path))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `absorbSetCookies(sess, res, requestUrl)` for one response.
fn mimo_absorb_set_cookies(session: &mut MimoLoginSession, headers: &http::HeaderMap, url: &str) {
    let Ok(parsed_url) = url::Url::parse(url) else {
        return;
    };
    let default_domain = parsed_url.host_str().unwrap_or("").to_string();
    let default_path = parsed_url
        .path()
        .rsplit_once('/')
        .map(|(dir, _)| if dir.is_empty() { "/" } else { dir })
        .unwrap_or("/")
        .to_string();

    for value in headers.get_all("set-cookie") {
        let Ok(raw) = value.to_str() else { continue };
        let mut parts = raw.split(';');
        let Some((name, cookie_value)) = parts
            .next()
            .and_then(|nv| nv.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        else {
            continue;
        };
        if name.is_empty() {
            continue;
        }

        let mut cookie = MimoCookie {
            name,
            value: cookie_value.clone(),
            domain: default_domain.clone(),
            path: default_path.clone(),
        };
        let mut expired = cookie_value == "EXPIRED";
        for attr in parts {
            let (key, val) = attr
                .split_once('=')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
                .unwrap_or((attr.trim().to_ascii_lowercase(), ""));
            match key.as_str() {
                "domain" if !val.is_empty() => {
                    cookie.domain = val.trim_start_matches('.').to_string()
                }
                "path" if !val.is_empty() => cookie.path = val.to_string(),
                "max-age" => {
                    if val.parse::<i64>().map(|v| v <= 0).unwrap_or(false) {
                        expired = true;
                    }
                }
                _ => {}
            }
        }

        session.jar.retain(|c| {
            !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        });
        if !expired && !cookie.value.is_empty() {
            session.jar.push(cookie);
        }
    }
}

/// `readSessionIdentity(sess)`: passToken from the account host jar.
fn mimo_session_identity(session: &MimoLoginSession) -> Option<(String, Option<String>, Option<String>)> {
    let pick = |name: &str| {
        session
            .jar
            .iter()
            .find(|c| c.name == name && mimo_domain_matches(&c.domain, "account.xiaomi.com"))
            .map(|c| c.value.clone())
    };
    let pass_token = pick("passToken").filter(|v| !v.is_empty() && v != "EXPIRED")?;
    Some((pass_token, pick("userId"), pick("cUserId")))
}

/// One manual-redirect hop, absorbing `Set-Cookie` into the session jar.
async fn mimo_hop(
    session: &mut MimoLoginSession,
    url: &str,
    user_agent: &str,
) -> Option<nr_core::http::UpstreamResponse> {
    let mut req = nr_core::http::FetchRequest::get(url);
    req.headers.insert("User-Agent".into(), user_agent.to_string());
    req.headers.insert(
        "Accept".into(),
        "text/html,application/json,*/*".into(),
    );
    let cookie = mimo_cookie_header(&session.jar, url);
    if !cookie.is_empty() {
        req.headers.insert("Cookie".into(), cookie);
    }
    req.header_timeout = Some(std::time::Duration::from_secs(15));

    let response = nr_core::http::proxy_fetch(req, None).await.ok()?;
    mimo_absorb_set_cookies(session, &response.headers, url);
    Some(response)
}

/// POST /api/oauth/xiaomi-mimo/login/start
pub async fn xiaomi_mimo_login_start(body: Option<Json<Value>>) -> Response {
    let requested = body
        .as_ref()
        .and_then(|Json(b)| b.get("region"))
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let region = MIMO_CLUSTERS
        .iter()
        .find(|(name, _)| *name == requested)
        .map(|(name, _)| *name)
        .unwrap_or(MIMO_DEFAULT_REGION)
        .to_string();

    let mut session = MimoLoginSession {
        state: uuid::Uuid::new_v4().to_string(),
        region: region.clone(),
        upstream_base: mimo_region_base(&region),
        jar: Vec::new(),
        created: std::time::Instant::now(),
    };

    // Hop 1: me -> account SSO (the callback carries this cluster's sts token).
    let me_url = format!("{}/api/user/xiaomi/me", session.upstream_base);
    let Some(me_response) = mimo_hop(&mut session, &me_url, MIMO_API_UA).await else {
        return import_error(StatusCode::BAD_GATEWAY, "Xiaomi login is unreachable");
    };
    let status = me_response.status;
    let sso_location = me_response
        .header("location")
        .unwrap_or("")
        .to_string();
    if !sso_location.contains("account.xiaomi.com") {
        return import_error(
            StatusCode::BAD_GATEWAY,
            format!("Unexpected me response ({status}) — no account redirect"),
        );
    }

    // Hop 2: serviceLogin -> the /fe/service/login SPA (seeds device cookies).
    let Some(login_response) = mimo_hop(&mut session, &sso_location, MIMO_SSO_UA).await else {
        return import_error(StatusCode::BAD_GATEWAY, "Xiaomi SSO is unreachable");
    };
    let login_status = login_response.status;
    let page_location = login_response.header("location").unwrap_or("").to_string();
    if page_location.is_empty() {
        return import_error(
            StatusCode::BAD_GATEWAY,
            format!("Unexpected serviceLogin response ({login_status})"),
        );
    }

    let page_url = url::Url::parse(&page_location)
        .or_else(|_| url::Url::parse("https://account.xiaomi.com").and_then(|b| b.join(&page_location)))
        .map(|u| u.to_string())
        .unwrap_or(page_location);

    let state = session.state.clone();
    MIMO_SESSIONS
        .lock()
        .expect("mimo session lock")
        .insert(state.clone(), session);

    import_response(
        StatusCode::OK,
        json!({ "success": true, "state": state, "pageUrl": page_url, "region": region }),
    )
}

/// GET /api/oauth/xiaomi-mimo/login/status
pub async fn xiaomi_mimo_login_status(Query(params): Query<HashMap<String, String>>) -> Response {
    let state = params.get("state").cloned().unwrap_or_default();

    let mut sessions = MIMO_SESSIONS.lock().expect("mimo session lock");
    let Some(session) = sessions.get(&state).cloned() else {
        return import_response(StatusCode::NOT_FOUND, json!({ "status": "expired" }));
    };
    if session.created.elapsed() > MIMO_SESSION_TTL {
        sessions.remove(&state);
        return import_response(StatusCode::NOT_FOUND, json!({ "status": "expired" }));
    }
    drop(sessions);

    let Some((pass_token, user_id, c_user_id)) = mimo_session_identity(&session) else {
        return import_response(
            StatusCode::OK,
            json!({ "status": "pending", "region": session.region }),
        );
    };

    // One-shot: the identity must not linger once the client has read it.
    MIMO_SESSIONS.lock().expect("mimo session lock").remove(&state);
    import_response(
        StatusCode::OK,
        json!({
            "status": "done",
            "region": session.region,
            "passToken": pass_token,
            "userId": user_id,
            "cUserId": c_user_id,
        }),
    )
}

// ─── perchai plan ───────────────────────────────────────────────────────────

/// `PERCH_PLAN_OPTIONS` of `src/lib/oauth/perchPlan.js`.
fn perch_plan_options() -> Value {
    json!([
        {
            "id": "starter",
            "label": "Starter (Free)",
            "roostChoice": "starter",
            "description": "Free tier with starter models (DeepSeek/GLM/MiniMax flash, Beam).",
            "free": true,
        },
        {
            "id": "pro",
            "label": "Pro",
            "roostChoice": "pro",
            "description": "Auto routing across the full frontier model list, plus Pro 1M context.",
            "free": false,
        },
    ])
}

/// `describePerchPlan(plan)`.
fn describe_perch_plan(plan: &str) -> String {
    match plan {
        "starter" => "Starter",
        "standard" => "Standard",
        "pro" => "Pro",
        "standard_max" => "Standard 1M",
        "pro_max" => "Pro 1M",
        other => other,
    }
    .to_string()
}

/// `resolvePerchConnection({ connectionId })`: by id, else the first active one.
fn resolve_perch_connection(db: &nr_db::Db, connection_id: Option<&str>) -> Option<Value> {
    let connections =
        nr_db::get_provider_connections(db, Some(&json!({ "provider": "perchai" }))).ok()?;
    let Some(connection_id) = connection_id.filter(|s| !s.is_empty()) else {
        return connections
            .iter()
            .find(|c| c.get("isActive").and_then(Value::as_bool) != Some(false))
            .cloned()
            .or_else(|| connections.first().cloned());
    };
    connections
        .into_iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(connection_id))
}

/// `getPerchConnectionToken(connection)`: `apiKey` then `accessToken`.
fn perch_token(connection: &Value) -> Option<String> {
    ["apiKey", "accessToken"]
        .iter()
        .find_map(|key| connection.get(*key).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn perch_app_url(connection: &Value) -> String {
    connection
        .get("providerSpecificData")
        .and_then(|psd| psd.get("appUrl"))
        .and_then(Value::as_str)
        .unwrap_or("https://app.perchai.app")
        .trim_end_matches('/')
        .to_string()
}

/// Probed plan state. `noticeVersion`/`proUrl` only come from the upstream
/// `cli-plan` payload, so the claim can echo back exactly what Perch offered.
struct PerchPlanProbe {
    required: bool,
    plan: Option<String>,
    notice: String,
    notice_version: Option<String>,
    pro_url: String,
}

/// `probePerchPlan(appUrl, token)` — GET the CLI's own `cli-plan` endpoint.
async fn probe_perch_plan(app_url: &str, token: &str) -> PerchPlanProbe {
    let url = format!("{}{}", app_url.trim_end_matches('/'), nr_exec_cli::perchai::PERCH_CLI_PLAN_PATH);
    let mut req = nr_core::http::FetchRequest::get(url);
    req.headers.insert("Accept".into(), "application/json".into());
    req.headers.insert("Authorization".into(), format!("Bearer {token}"));
    req.headers.insert("User-Agent".into(), "perchai-cli/2.4.118.0".into());
    req.headers.insert("x-perch-locale".into(), "en".into());
    req.header_timeout = Some(std::time::Duration::from_secs(15));

    let payload = match nr_core::http::proxy_fetch(req, None).await {
        Ok(response) if response.ok() => response.json_body().await.unwrap_or(Value::Null),
        // 404 means the deployment predates cli-plan: no gate information.
        _ => Value::Null,
    };

    let required = payload.get("planRequired") == Some(&Value::Bool(true));
    let starter = payload.get("starter");
    let plan = match starter {
        Some(_) => Some("starter".to_string()),
        None => payload.get("plan").and_then(Value::as_str).map(str::to_string),
    };
    let notice = if required {
        starter
            .and_then(|s| s.get("notice"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(nr_exec_cli::perchai::plan_notice)
    } else {
        String::new()
    };
    let notice_version = starter
        .and_then(|s| s.get("noticeVersion"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let pro_url = payload
        .get("proUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "https://chat.perchai.app".to_string());

    PerchPlanProbe { required, plan, notice, notice_version, pro_url }
}

/// `choosePerchStarterPlan(appUrl, token, noticeVersion)`.
async fn choose_perch_starter_plan(
    app_url: &str,
    token: &str,
    notice_version: Option<&str>,
) -> Result<bool, (u16, String, Option<String>)> {
    let Some(notice_version) = notice_version else {
        return Err((
            0,
            "Perch did not advertise a plan notice to accept. Pick the plan at chat.perchai.app."
                .to_string(),
            None,
        ));
    };

    let url = format!("{}{}", app_url.trim_end_matches('/'), nr_exec_cli::perchai::PERCH_CLI_PLAN_PATH);
    let mut req = nr_core::http::FetchRequest::post_json(
        url,
        {
            let mut headers = nr_core::http::HeaderList::new();
            headers.insert("Accept".into(), "application/json".into());
            headers.insert("Authorization".into(), format!("Bearer {token}"));
            headers.insert("User-Agent".into(), "perchai-cli/2.4.118.0".into());
            headers.insert("x-perch-locale".into(), "en".into());
            headers
        },
        &json!({ "plan": "starter", "noticeVersion": notice_version }),
    );
    req.header_timeout = Some(std::time::Duration::from_secs(15));

    let response = nr_core::http::proxy_fetch(req, None)
        .await
        .map_err(|e| (0, e.to_string(), None))?;
    let status = response.status;
    let data = response.json_body().await.unwrap_or(Value::Null);

    if !(200..300).contains(&status) || data.is_null() || data.get("ok") == Some(&Value::Bool(false)) {
        let error = data
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Perch rejected the plan selection (HTTP {status})"));
        let code = data
            .get("errorCode")
            .or_else(|| data.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err((status, error, code));
    }

    Ok(data.get("alreadyChosen") == Some(&Value::Bool(true)))
}

/// GET /api/oauth/perchai/plan/status
pub async fn perchai_plan_status(Query(params): Query<HashMap<String, String>>) -> Response {
    let connection_id = params.get("connectionId").cloned().filter(|s| !s.is_empty());
    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to check Perch AI plan");
    };

    let Some(connection) = resolve_perch_connection(&db, connection_id.as_deref()) else {
        return import_error(
            StatusCode::NOT_FOUND,
            "Perch AI connection not found. Connect a Perch AI account first.",
        );
    };
    let Some(token) = perch_token(&connection) else {
        return import_error(StatusCode::BAD_REQUEST, "Perch AI connection has no access token");
    };

    let app_url = perch_app_url(&connection);
    let probe = probe_perch_plan(&app_url, &token).await;

    let mut body = serde_json::Map::new();
    body.insert("ok".into(), json!(true));
    body.insert("required".into(), json!(probe.required));
    // Only the upstream answer counts: a plan saved in providerSpecificData is
    // the local preference, not the account's actual plan.
    body.insert("plan".into(), json!(probe.plan));
    body.insert(
        "planLabel".into(),
        json!(probe.plan.as_deref().map(describe_perch_plan)),
    );
    body.insert(
        "notice".into(),
        if probe.required { json!(probe.notice) } else { Value::Null },
    );
    body.insert("noticeVersion".into(), json!(probe.notice_version));
    body.insert(
        "errorCode".into(),
        if probe.required {
            json!(nr_exec_cli::perchai::PERCH_TIER_GATE_CODE)
        } else {
            Value::Null
        },
    );
    body.insert("signupUrl".into(), json!(probe.pro_url));
    body.insert("options".into(), perch_plan_options());
    body.insert(
        "connectionId".into(),
        connection.get("id").cloned().unwrap_or(Value::Null),
    );

    import_response(StatusCode::OK, Value::Object(body))
}

/// POST /api/oauth/perchai/plan/claim — `claimPerchPlan`.
pub async fn perchai_plan_claim(body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let plan = string_field(&body, "plan")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "starter".to_string());
    let connection_id = string_field(&body, "connectionId")
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if plan != "starter" && plan != "pro" {
        return import_response(
            StatusCode::BAD_REQUEST,
            json!({
                "error": format!("Unknown Perch plan \"{plan}\". Choose one of: starter, pro"),
                "options": perch_plan_options(),
            }),
        );
    }

    let Ok(db) = db_handle() else {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to set Perch AI plan");
    };
    let Some(connection) = resolve_perch_connection(&db, connection_id.as_deref()) else {
        return import_response(
            StatusCode::NOT_FOUND,
            json!({
                "error": "Perch AI connection not found. Connect a Perch AI account first.",
                "options": perch_plan_options(),
            }),
        );
    };
    let Some(token) = perch_token(&connection) else {
        return import_response(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "Perch AI connection has no access token",
                "options": perch_plan_options(),
            }),
        );
    };

    let app_url = perch_app_url(&connection);

    if plan != "starter" {
        // The terminal API only selects Starter; the CLI renders Pro as a link.
        return import_response(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "Perch only allows selecting the Starter plan from the terminal API. Open chat.perchai.app to upgrade to Pro.",
                "errorCode": nr_exec_cli::perchai::PERCH_TIER_GATE_CODE,
                "signupUrl": "https://chat.perchai.app",
                "plan": plan,
                "needsManualSelection": true,
                "options": perch_plan_options(),
            }),
        );
    }

    // Ask Perch first: persisting a choice before the server accepts it just
    // hides the failure behind a saved label.
    let probe = probe_perch_plan(&app_url, &token).await;
    let selection = choose_perch_starter_plan(&app_url, &token, probe.notice_version.as_deref()).await;

    let already_chosen = match selection {
        Ok(already_chosen) => already_chosen,
        Err((status, error, code)) => {
            nr_exec_cli::perchai::remember_plan(
                &token,
                &nr_exec_cli::perchai::PlanState {
                    required: true,
                    plan: None,
                    notice: probe.notice.clone(),
                },
            );
            let http_status = if status >= 400 {
                StatusCode::CONFLICT
            } else if status == 0 {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::BAD_GATEWAY
            };
            return import_response(
                http_status,
                json!({
                    "error": if probe.notice.is_empty() { error.clone() } else { probe.notice.clone() },
                    "detail": error,
                    "errorCode": code.unwrap_or_else(|| nr_exec_cli::perchai::PERCH_TIER_GATE_CODE.to_string()),
                    "signupUrl": probe.pro_url,
                    "plan": plan,
                    "needsManualSelection": true,
                    "connectionSaved": false,
                    "options": perch_plan_options(),
                }),
            );
        }
    };

    // Accepted: record the choice on the connection, then re-read the gate.
    let mut psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    psd.insert("perchPlan".into(), json!("starter"));
    psd.insert(
        "perchPlanLabel".into(),
        json!(describe_perch_plan("starter")),
    );
    psd.insert(
        "perchPlanSelectedAt".into(),
        json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    );
    psd.insert("perchPlanNotice".into(), Value::Null);

    let id = connection.get("id").and_then(Value::as_str).unwrap_or("");
    let saved = nr_db::update_provider_connection(
        &db,
        id,
        &json!({
            "providerSpecificData": Value::Object(psd),
            "lastError": Value::Null,
        }),
    )
    .ok()
    .flatten()
    .is_some();

    nr_exec_cli::perchai::clear_plan_cache();
    let confirmed = probe_perch_plan(&app_url, &token).await;
    if confirmed.required {
        return import_response(
            StatusCode::CONFLICT,
            json!({
                "error": if confirmed.notice.is_empty() {
                    nr_exec_cli::perchai::plan_notice()
                } else {
                    confirmed.notice.clone()
                },
                "errorCode": nr_exec_cli::perchai::PERCH_TIER_GATE_CODE,
                "signupUrl": confirmed.pro_url,
                "plan": plan,
                "needsManualSelection": true,
                "connectionSaved": saved,
                "options": perch_plan_options(),
            }),
        );
    }

    nr_exec_cli::perchai::remember_plan(
        &token,
        &nr_exec_cli::perchai::PlanState {
            required: false,
            plan: Some(json!("starter")),
            notice: String::new(),
        },
    );

    // `maskToken(token)`: `abc123…wxyz`, or a single ellipsis when short.
    let masked = if token.chars().count() <= 12 {
        "…".to_string()
    } else {
        let chars: Vec<char> = token.chars().collect();
        let head: String = chars[..6].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    };

    import_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "required": false,
            "plan": "starter",
            "planLabel": describe_perch_plan("starter"),
            "alreadyChosen": already_chosen,
            "appliedVia": nr_exec_cli::perchai::PERCH_CLI_PLAN_PATH,
            "connectionSaved": saved,
            "token": masked,
            "options": perch_plan_options(),
        }),
    )
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Env-var-mutating tests share one process, so they must not interleave.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        keys: Vec<String>,
    }

    impl EnvGuard {
        fn set(pairs: &[(&str, &str)]) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut keys = Vec::new();
            for (key, value) in pairs {
                std::env::set_var(key, value);
                keys.push((*key).to_string());
            }
            Self { _lock: lock, keys }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in &self.keys {
                std::env::remove_var(key);
            }
        }
    }

    /// The default DB is process-global, so tests that install one must not
    /// interleave. Returns the guard alongside the handle; hold it for the whole
    /// test (any test that reads back through the global handle depends on it).
    static DB_LOCK: Mutex<()> = Mutex::new(());

    fn test_db() -> (tempfile::TempDir, nr_db::Db, MutexGuard<'static, ()>) {
        let lock = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let db = nr_db::Db::open_in(dir.path()).unwrap();
        nr_db::set_default_db(Some(db.clone()));
        (dir, db, lock)
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[derive(Clone)]
    struct MockRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl MockRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap_or(Value::Null)
        }
    }

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<MockRequest>>>,
    }

    impl MockServer {
        fn paths(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.path.clone())
                .collect()
        }

        fn requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// One request per connection: `Connection: close` stops reqwest from
    /// reusing a socket the mock never reads twice.
    fn start_mock<F>(handler: F) -> MockServer
    where
        F: Fn(&MockRequest) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handler = Arc::new(handler);

        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { return };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let Some((head, body)) = read_request(&mut stream) else { continue };

                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();

                let request = MockRequest { method, path, headers, body };
                recorded.lock().unwrap().push(request.clone());
                let (status, payload) = handler(&request);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        MockServer { url: format!("http://{addr}"), requests }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut body = buf[head_end + 4..].to_vec();
        let length = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while body.len() < length {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some((head, String::from_utf8_lossy(&body).to_string()))
    }

    /// A JWT shaped `header.payload.signature` with no real signature.
    fn fake_jwt(claims: Value) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{payload}.signature_padding_for_length_over_fifty_characters")
    }

    // ─── cursor ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cursor_instructions_describe_both_required_fields() {
        let json = body_json(cursor_import_instructions().await).await;
        assert_eq!(json["provider"], json!("cursor"));
        assert_eq!(json["method"], json!("import_token"));
        assert_eq!(json["requiredFields"][0]["name"], json!("accessToken"));
        assert_eq!(json["requiredFields"][1]["name"], json!("machineId"));
        // Paths come from the oauth constants dump, not from literals here.
        assert!(json["instructions"]["steps"][3]
            .as_str()
            .unwrap()
            .contains("state.vscdb"));
    }

    #[tokio::test]
    async fn cursor_import_rejects_bad_input() {
        assert_eq!(
            cursor_import(Json(json!({}))).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            cursor_import(Json(json!({ "accessToken": "t" }))).await.status(),
            StatusCode::BAD_REQUEST
        );

        let token = fake_jwt(json!({ "email": "c@x" }));
        // A short token fails `validateImportToken`, which the JS catch maps to 500.
        let short = cursor_import(Json(json!({
            "accessToken": "too-short",
            "machineId": "0123456789abcdef0123456789abcdef"
        })))
        .await;
        assert_eq!(short.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = body_json(short).await;
        assert!(json["error"].as_str().unwrap().contains("too short"));

        // A non-UUID machine id is the other thrown validation error.
        let bad_uuid = cursor_import(Json(json!({
            "accessToken": token,
            "machineId": "not-a-uuid"
        })))
        .await;
        assert_eq!(bad_uuid.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body_json(bad_uuid).await["error"]
            .as_str()
            .unwrap()
            .contains("machine ID"));
    }

    #[tokio::test]
    async fn cursor_import_stores_a_connection() {
        let (_dir, db, _db_lock) = test_db();
        let token = fake_jwt(json!({ "email": "cursor@example.com", "sub": "user-1" }));
        let machine_id = "0123456789abcdef0123456789abcdef";

        let response = cursor_import(Json(json!({
            "accessToken": format!("  {token}  "),
            "machineId": format!(" {machine_id} "),
        })))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["success"], json!(true));
        assert_eq!(json["connection"]["provider"], json!("cursor"));
        assert_eq!(json["connection"]["email"], json!("cursor@example.com"));

        let connection = nr_db::get_provider_connection_by_id(
            &db,
            json["connection"]["id"].as_str().unwrap(),
        )
        .unwrap()
        .expect("connection row");
        assert_eq!(connection["authType"], json!("oauth"));
        assert_eq!(
            connection["providerSpecificData"]["machineId"],
            json!(machine_id),
            "the token and machine id are trimmed before storage"
        );
        assert_eq!(
            connection["providerSpecificData"]["authMethod"],
            json!("imported")
        );
    }

    // ─── kiro ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn kiro_import_cli_proxy_accepts_a_string_payload() {
        let (_dir, db, _db_lock) = test_db();
        let auth = json!({
            "auth_method": "external_idp",
            "access_token": fake_jwt(json!({ "upn": "dev@corp.example" })),
            "refresh_token": "rt-1",
            "client_id": "client-1",
            "token_endpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
            "profile_arn": "arn:aws:codewhisperer:us-east-1:1:profile/A",
            "scopes": ["offline_access", "sso"],
            "expires_in": 1800,
        });

        let response = kiro_import_cli_proxy(Json(json!({
            "cliProxyAuth": auth.to_string(),
        })))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["success"], json!(true));
        assert_eq!(json["connection"]["email"], json!("dev@corp.example"));

        let connection = nr_db::get_provider_connection_by_id(
            &db,
            json["connection"]["id"].as_str().unwrap(),
        )
        .unwrap()
        .expect("connection row");
        assert_eq!(
            connection["providerSpecificData"]["authMethod"],
            json!("external_idp")
        );
        assert_eq!(
            connection["providerSpecificData"]["scope"],
            json!("offline_access sso"),
            "an array of scopes joins with spaces, like normalizeScope"
        );
        assert_eq!(connection["providerSpecificData"]["provider"], json!("CLIProxyAPI"));
    }

    #[tokio::test]
    async fn kiro_import_cli_proxy_rejects_bad_input() {
        // Non-Microsoft token endpoint would be an SSRF pivot.
        let bad_endpoint = kiro_import_cli_proxy(Json(json!({
            "access_token": "a",
            "refresh_token": "r",
            "client_id": "c",
            "profile_arn": "arn:aws:codewhisperer:us-east-1:1:profile/A",
            "token_endpoint": "https://attacker.example/token",
            "scopes": "s",
        })))
        .await;
        assert_eq!(bad_endpoint.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(bad_endpoint).await["error"]
            .as_str()
            .unwrap()
            .contains("Microsoft login endpoint"));

        let wrong_method = kiro_import_cli_proxy(Json(json!({
            "auth_method": "builder_id",
            "access_token": "a",
        })))
        .await;
        assert_eq!(wrong_method.status(), StatusCode::BAD_REQUEST);

        assert_eq!(
            kiro_import_cli_proxy(Json(json!({ "cliProxyAuth": "not json" })))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn kiro_import_requires_a_refresh_token() {
        let response = kiro_import(Json(json!({ "refreshToken": "   " }))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await["error"],
            json!("Refresh token is required")
        );
    }

    #[tokio::test]
    async fn kiro_api_key_rejects_bad_input_before_validating() {
        assert_eq!(
            kiro_api_key(Json(json!({}))).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            kiro_api_key(Json(json!({ "apiKey": "  " }))).await.status(),
            StatusCode::BAD_REQUEST
        );
        // A region is interpolated into the upstream URL, so a bad one never gets that far.
        assert_eq!(
            kiro_api_key(Json(json!({ "apiKey": "k", "region": "evil.example/x" })))
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn kiro_social_authorize_builds_the_cognito_url() {
        let response = kiro_social_authorize(Query(HashMap::from([(
            "provider".to_string(),
            "google".to_string(),
        )])))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["provider"], json!("google"));
        let url = json["authUrl"].as_str().unwrap();
        assert!(url.contains("idp=Google"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=kiro%3A%2F%2Fkiro.kiroAgent"));
        assert!(url.contains(&format!("code_challenge={}", json["codeChallenge"].as_str().unwrap())));
        assert!(json["codeVerifier"].as_str().unwrap().len() >= 43);

        for provider in ["", "facebook"] {
            let response = kiro_social_authorize(Query(HashMap::from([(
                "provider".to_string(),
                provider.to_string(),
            )])))
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            kiro_social_authorize(Query(HashMap::new())).await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn kiro_social_exchange_requires_all_three_fields() {
        assert_eq!(
            kiro_social_exchange(Json(json!({ "code": "c" }))).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            kiro_social_exchange(Json(json!({ "code": "c", "codeVerifier": "v", "provider": "x" })))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    // ─── iflow ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn iflow_cookie_exchanges_bxauth_for_an_api_key() {
        let (_dir, db, _db_lock) = test_db();
        let server = start_mock(|request| {
            if request.method == "GET" {
                (
                    200,
                    json!({
                        "success": true,
                        "data": { "name": "my-key", "other": 1 },
                    })
                    .to_string(),
                )
            } else {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "apiKey": "sk-abcdefghijklmnop",
                            "name": "my-key",
                            "expireTime": 1_700_000_000_000i64,
                        },
                    })
                    .to_string(),
                )
            }
        });
        let _env = EnvGuard::set(&[("NR_OAUTH_ENDPOINT_IFLOW_APIKEY", &server.url)]);

        let response = iflow_cookie(Json(json!({
            "cookie": "BXAuth=tok-123; other=1",
        })))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["success"], json!(true));
        assert_eq!(json["connection"]["apiKey"], json!("sk-abcdefg..."));
        assert_eq!(json["connection"]["email"], json!("my-key"));

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        // The cookie is normalised with a trailing `;` and only BXAuth is kept.
        assert_eq!(requests[0].header("cookie"), Some("BXAuth=tok-123; other=1;"));
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].json()["name"], json!("my-key"));

        let connection = nr_db::get_provider_connection_by_id(
            &db,
            json["connection"]["id"].as_str().unwrap(),
        )
        .unwrap()
        .expect("connection row");
        assert_eq!(connection["authType"], json!("cookie"));
        assert_eq!(
            connection["providerSpecificData"]["cookie"],
            json!("BXAuth=tok-123;")
        );
    }

    #[tokio::test]
    async fn iflow_cookie_rejects_a_cookie_without_bxauth() {
        assert_eq!(
            iflow_cookie(Json(json!({}))).await.status(),
            StatusCode::BAD_REQUEST
        );
        let response = iflow_cookie(Json(json!({ "cookie": "SESSION=abc" }))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await["error"],
            json!("Cookie must contain BXAuth field")
        );
    }

    // ─── zed ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn zed_import_validates_against_the_cloud_api() {
        let (_dir, db, _db_lock) = test_db();
        let server = start_mock(|request| {
            assert_eq!(request.path, "/client/users/me");
            assert_eq!(request.header("authorization"), Some("user-1 zed-token"));
            assert_eq!(request.header("x-zed-system-id"), Some("sys-1"));
            (
                200,
                json!({
                    "email": "zed@example.com",
                    "name": "Zed User",
                    "organizations": [
                        { "id": "org-work" },
                        { "id": "org-personal", "is_personal": true },
                    ],
                })
                .to_string(),
            )
        });
        let _env = EnvGuard::set(&[("NR_OAUTH_ENDPOINT_ZED_CLOUD", &server.url)]);

        let response = zed_import(Json(json!({
            "accessToken": " zed-token ",
            "userId": "user-1",
            "systemId": "sys-1",
        })))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["success"], json!(true));
        assert_eq!(json["connection"]["email"], json!("zed@example.com"));
        assert_eq!(json["connection"]["displayName"], json!("Zed User"));

        let connection = nr_db::get_provider_connection_by_id(
            &db,
            json["connection"]["id"].as_str().unwrap(),
        )
        .unwrap()
        .expect("connection row");
        assert_eq!(
            connection["providerSpecificData"]["organizationId"],
            json!("org-personal"),
            "the personal organization wins when none is named"
        );
        assert_eq!(connection["providerSpecificData"]["authMethod"], json!("imported"));
    }

    #[tokio::test]
    async fn zed_import_maps_a_rejected_token_to_401() {
        let server = start_mock(|_| (401, json!({ "error": "unauthorized" }).to_string()));
        let _env = EnvGuard::set(&[("NR_OAUTH_ENDPOINT_ZED_CLOUD", &server.url)]);

        let response = zed_import(Json(json!({
            "accessToken": "bad",
            "userId": "user-1",
        })))
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        assert_eq!(
            zed_import(Json(json!({ "userId": "u" }))).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            zed_import(Json(json!({ "accessToken": "t", "userId": null })))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn zed_organization_prefers_an_explicit_value() {
        let body = json!({ "organizationId": "explicit" });
        let user = json!({ "default_organization_id": "profile" });
        assert_eq!(resolve_zed_organization_id(&body, &user), "explicit");

        assert_eq!(
            resolve_zed_organization_id(&json!({}), &user),
            "profile"
        );
        assert_eq!(
            resolve_zed_organization_id(&json!({}), &json!({ "organizations": [ { "id": ["first"] } ] })),
            "first"
        );
        assert_eq!(resolve_zed_organization_id(&json!({}), &json!({})), "");
    }

    // ─── xiaomi-mimo ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn xiaomi_api_key_stores_a_validated_key() {
        let (_dir, db, _db_lock) = test_db();
        let server = start_mock(|request| {
            assert_eq!(request.path, "/models");
            assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
            (
                200,
                json!({ "data": [{ "id": "mimo-a" }, { "id": "mimo-b" }] }).to_string(),
            )
        });
        let _env = EnvGuard::set(&[("NR_OAUTH_ENDPOINT_XIAOMI_MIMO_MODELS", &server.url)]);

        let response = xiaomi_mimo_api_key(Json(json!({
            "apiKey": "sk-test",
            "uid": "42",
            "baseUrl": "https://api.xiaomimimo.com/v1/",
        })))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["validated"], json!(true));
        assert_eq!(json["modelCount"], json!(2));
        assert_eq!(json["connection"]["email"], json!("42@xiaomi"));

        let connection = nr_db::get_provider_connection_by_id(
            &db,
            json["connection"]["id"].as_str().unwrap(),
        )
        .unwrap()
        .expect("connection row");
        // The trailing slash is trimmed from baseUrl.
        assert_eq!(
            connection["providerSpecificData"]["baseUrl"],
            json!("https://api.xiaomimimo.com/v1")
        );
        assert_eq!(connection["authType"], json!("api_key"));
    }

    #[tokio::test]
    async fn xiaomi_api_key_accepts_a_session_only_body_and_updates_in_place() {
        let (_dir, db, _db_lock) = test_db();

        let first = xiaomi_mimo_api_key(Json(json!({
            "mimoPassToken": "pass-1",
            "mimoUserId": "u-1",
            "region": "cn",
        })))
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let json = body_json(first).await;
        assert_eq!(json["validated"], json!(false));
        assert_eq!(json["connection"]["displayName"], json!("Xiaomi MiMo"));
        let id = json["connection"]["id"].as_str().unwrap().to_string();

        // Same session identity + region must update, not duplicate.
        let second = xiaomi_mimo_api_key(Json(json!({
            "mimoPassToken": "pass-2",
            "mimoUserId": "u-1",
            "region": "cn",
        })))
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        let json = body_json(second).await;
        assert_eq!(json["updated"], json!(true));
        assert_eq!(json["connection"]["id"], json!(id));

        let connection = nr_db::get_provider_connection_by_id(&db, &id).unwrap().unwrap();
        assert_eq!(
            connection["providerSpecificData"]["mimoPassToken"],
            json!("pass-2")
        );
        assert_eq!(
            nr_db::get_provider_connections(&db, Some(&json!({ "provider": "xiaomi-mimo" })))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn xiaomi_api_key_rejects_bad_input() {
        let response = xiaomi_mimo_api_key(Json(json!({}))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await["error"],
            json!("API key is required")
        );

        let response = xiaomi_mimo_api_key(Json(json!({ "apiKey": "nope" }))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("sk- prefix"));
    }

    #[tokio::test]
    async fn xiaomi_login_status_expires_an_unknown_state() {
        let response = xiaomi_mimo_login_status(Query(HashMap::from([(
            "state".to_string(),
            "never-issued".to_string(),
        )])))
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(response).await["status"], json!("expired"));
    }

    #[tokio::test]
    async fn xiaomi_login_start_reports_a_dead_upstream() {
        // Port 9 (discard) refuses connections: the `me` hop must fail closed.
        let _env = EnvGuard::set(&[("NR_OAUTH_ENDPOINT_MIMO_SGP", "http://127.0.0.1:9")]);
        let response = xiaomi_mimo_login_start(Some(Json(json!({ "region": "sgp" })))).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn xiaomi_region_falls_back_to_sgp() {
        assert_eq!(mimo_region_base("cn"), "https://mimo-server-cn.xiaomimimo.com");
        assert_eq!(
            mimo_region_base("nope"),
            "https://mimo-server-sgp.xiaomimimo.com"
        );
    }

    #[test]
    fn mimo_cookie_helpers_scope_by_domain_and_path() {
        let jar = vec![
            MimoCookie {
                name: "passToken".into(),
                value: "pt".into(),
                domain: "account.xiaomi.com".into(),
                path: "/".into(),
            },
            MimoCookie {
                name: "other".into(),
                value: "x".into(),
                domain: "mimo-server-cn.xiaomimimo.com".into(),
                path: "/".into(),
            },
        ];
        assert_eq!(
            mimo_cookie_header(&jar, "https://account.xiaomi.com/pass/serviceLogin"),
            "passToken=pt"
        );
        assert_eq!(mimo_cookie_header(&jar, "https://elsewhere.example/"), "");
        assert!(mimo_domain_matches(".xiaomi.com", "account.xiaomi.com"));
        assert!(!mimo_domain_matches("xiaomi.com", "notxiaomi.com"));
    }

    #[test]
    fn mimo_identity_needs_a_live_pass_token() {
        let mut session = MimoLoginSession {
            state: "s".into(),
            region: "sgp".into(),
            upstream_base: "https://mimo-server-sgp.xiaomimimo.com".into(),
            jar: Vec::new(),
            created: std::time::Instant::now(),
        };
        assert!(mimo_session_identity(&session).is_none());

        session.jar.push(MimoCookie {
            name: "passToken".into(),
            value: "EXPIRED".into(),
            domain: "account.xiaomi.com".into(),
            path: "/".into(),
        });
        assert!(mimo_session_identity(&session).is_none());

        session.jar[0].value = "live".into();
        session.jar.push(MimoCookie {
            name: "userId".into(),
            value: "42".into(),
            domain: "account.xiaomi.com".into(),
            path: "/".into(),
        });
        let identity = mimo_session_identity(&session).unwrap();
        assert_eq!(identity.0, "live");
        assert_eq!(identity.1.as_deref(), Some("42"));
    }

    // ─── perchai plan ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn perch_plan_status_reports_the_upstream_gate() {
        let (_dir, db, _db_lock) = test_db();
        nr_db::create_provider_connection(
            &db,
            &json!({
                "provider": "perchai",
                "authType": "oauth",
                "accessToken": "perch-token",
                "providerSpecificData": { "appUrl": "https://app.perchai.app" },
            }),
        )
        .unwrap();

        let server = start_mock(|request| {
            assert_eq!(request.path, "/api/perch-terminal/cli-plan");
            assert_eq!(request.header("authorization"), Some("Bearer perch-token"));
            (
                200,
                json!({
                    "ok": true,
                    "planRequired": true,
                    "starter": { "noticeVersion": "n-1", "notice": "pick a plan" },
                    "proUrl": "https://chat.perchai.app/pro",
                })
                .to_string(),
            )
        });
        let _env = EnvGuard::set(&[(
            "TEST_PERCH_APP_URL",
            &format!("{}/api/perch-terminal/cli-plan", server.url),
        )]);

        // The connection carries its own appUrl, so point that at the mock.
        let conns = nr_db::get_provider_connections(&db, Some(&json!({ "provider": "perchai" })))
            .unwrap();
        let id = conns[0]["id"].as_str().unwrap().to_string();
        nr_db::update_provider_connection(
            &db,
            &id,
            &json!({
                "providerSpecificData": { "appUrl": server.url },
            }),
        )
        .unwrap();

        let response = perchai_plan_status(Query(HashMap::new())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["ok"], json!(true));
        assert_eq!(json["required"], json!(true));
        assert_eq!(json["plan"], json!("starter"));
        assert_eq!(json["planLabel"], json!("Starter"));
        assert_eq!(
            json["errorCode"],
            json!(nr_exec_cli::perchai::PERCH_TIER_GATE_CODE)
        );
        assert_eq!(json["noticeVersion"], json!("n-1"));
        assert_eq!(json["signupUrl"], json!("https://chat.perchai.app/pro"));
        assert_eq!(json["options"][0]["id"], json!("starter"));
        assert_eq!(json["connectionId"], json!(id));
    }

    #[tokio::test]
    async fn perch_plan_status_404s_without_a_connection() {
        let (_dir, db, _db_lock) = test_db();
        let _ = db;
        let response = perchai_plan_status(Query(HashMap::new())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("Perch AI connection not found"));
    }

    #[tokio::test]
    async fn perch_plan_claim_rejects_an_unknown_plan() {
        let response = perchai_plan_claim(Some(Json(json!({ "plan": "platinum" })))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = body_json(response).await;
        assert!(json["error"].as_str().unwrap().contains("Unknown Perch plan"));
        assert_eq!(json["options"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn perch_plan_claim_sends_pro_to_the_web_app() {
        let (_dir, db, _db_lock) = test_db();
        nr_db::create_provider_connection(
            &db,
            &json!({
                "provider": "perchai",
                "authType": "oauth",
                "accessToken": "perch-token",
                "providerSpecificData": { "appUrl": "https://app.perchai.app" },
            }),
        )
        .unwrap();

        let response = perchai_plan_claim(Some(Json(json!({ "plan": "Pro" })))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = body_json(response).await;
        assert_eq!(json["needsManualSelection"], json!(true));
        assert_eq!(json["plan"], json!("pro"));
        assert_eq!(json["signupUrl"], json!("https://chat.perchai.app"));
    }

    #[tokio::test]
    async fn perch_plan_claim_accepts_starter_and_saves_it() {
        let (_dir, db, _db_lock) = test_db();
        let server = start_mock(|request| {
            if request.method == "POST" {
                assert_eq!(request.json()["plan"], json!("starter"));
                assert_eq!(request.json()["noticeVersion"], json!("n-1"));
                (200, json!({ "ok": true }).to_string())
            } else {
                // First probe: gate open. Second probe (after the claim): closed.
                (
                    200,
                    json!({ "ok": true, "starter": { "noticeVersion": "n-1" } }).to_string(),
                )
            }
        });

        nr_db::create_provider_connection(
            &db,
            &json!({
                "provider": "perchai",
                "authType": "oauth",
                "accessToken": "perch-token",
                "providerSpecificData": { "appUrl": server.url },
            }),
        )
        .unwrap();

        let response = perchai_plan_claim(None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["ok"], json!(true));
        assert_eq!(json["plan"], json!("starter"));
        assert_eq!(json["connectionSaved"], json!(true));
        assert_eq!(json["appliedVia"], json!(nr_exec_cli::perchai::PERCH_CLI_PLAN_PATH));
        assert_eq!(server.paths().len(), 3, "probe + claim + confirm probe");

        let conns = nr_db::get_provider_connections(&db, Some(&json!({ "provider": "perchai" })))
            .unwrap();
        assert_eq!(conns[0]["providerSpecificData"]["perchPlan"], json!("starter"));
        assert_eq!(
            conns[0]["providerSpecificData"]["perchPlanLabel"],
            json!("Starter")
        );
    }

    // ─── shared helpers ─────────────────────────────────────────────────────

    #[test]
    fn scope_and_expiry_helpers_match_the_js_shapes() {
        assert_eq!(normalize_scope(Some(&json!(["a", " b "]))), "a b");
        assert_eq!(normalize_scope(Some(&json!("  c  "))), "c");
        assert_eq!(normalize_scope(None), "");

        let explicit = resolve_expires_at(&json!({ "expired": "2030-01-02T03:04:05Z" }));
        assert!(explicit.starts_with("2030-01-02T03:04:05"));

        let by_seconds = resolve_expires_at(&json!({ "expires_in": 60 }));
        let parsed = chrono::DateTime::parse_from_rfc3339(&by_seconds).unwrap();
        let now = chrono::Utc::now();
        assert!((parsed.with_timezone(&chrono::Utc) - now).num_seconds() > 30);

        let by_jwt = resolve_expires_at(&json!({
            "access_token": fake_jwt(json!({ "exp": 1_900_000_000u64 })),
        }));
        assert!(by_jwt.starts_with("2030-"));

        // Nothing usable: the 1 h default.
        let fallback = resolve_expires_at(&json!({}));
        assert!(chrono::DateTime::parse_from_rfc3339(&fallback).is_ok());
    }

    #[test]
    fn microsoft_endpoints_are_allowlisted() {
        assert!(validate_microsoft_token_endpoint(
            "https://login.microsoftonline.com/t/oauth2/v2.0/token"
        )
        .is_ok());
        assert!(validate_microsoft_token_endpoint("http://login.microsoftonline.com/t").is_err());
        assert!(validate_microsoft_token_endpoint("https://evil.example/token").is_err());
        assert!(validate_microsoft_token_endpoint("").is_err());
    }

    #[test]
    fn masquerades_plan_labels_and_tokens() {
        assert_eq!(describe_perch_plan("starter"), "Starter");
        assert_eq!(describe_perch_plan("standard_max"), "Standard 1M");
        assert_eq!(describe_perch_plan("custom"), "custom");
    }

    #[test]
    fn kiro_social_url_targets_cognito() {
        let url = kiro_social_login_url("github", "challenge", "state-1");
        assert!(url.starts_with(KIRO_SOCIAL_HOST));
        assert!(url.contains("idp=Github"));
        assert!(url.contains("state=state-1"));
        assert!(url.contains("prompt=select_account"));
    }
}
