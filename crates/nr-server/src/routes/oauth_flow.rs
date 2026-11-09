//! OAuth flow HTTP routes: dynamic `/api/oauth/[provider]/[action]` endpoints plus
//! provider-specific import routes.
//! Faithful port of `src/app/api/oauth/**`.
//!
//! Backed by `nr_oauth` (`OAuthProvider` trait + flows helpers). If a provider's
//! flow function is not ported yet, we return a clear `501` naming the provider
//! rather than panicking or returning a bare 400.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use std::collections::HashMap;

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
    "Claude Opus 5.5-cn",
    "Claude Opus 5.5-intl",
    "qoder",
    "qoder-cn",
    "grok-cli",
    "muse",
    "glm",
    "nous",
];

fn not_implemented(provider: &str, action: &str) -> Response {
    error_response(
        StatusCode::NOT_IMPLEMENTED,
        format!("Provider `{provider}` does not yet support action `{action}` in this build"),
    )
}

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
        _ => {
            // All remaining actions in JS are provider-specific (start-proxy,
            // poll-status, stop-proxy, ide-status, lease-status) and rely on
            // local callback proxies / OS detection that is not ported here.
            Ok(not_implemented(provider, action))
        }
    }
}

async fn handle_authorize(provider: &str, params: HashMap<String, String>) -> nr_core::Result<Response> {
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

            // xAI proxy sessions are local-only; not ported in this build.
            Ok(not_implemented(provider, "manual-code"))
        }
        _ => {
            // All remaining actions (lease-connect, register-session, ...) rely
            // on local proxy servers / OS state that is not ported here.
            let _ = query;
            Ok(not_implemented(provider, action))
        }
    }
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

    if code.is_empty() {
        return Ok(error_response(StatusCode::BAD_REQUEST, "Missing code"));
    }

    let redirect = redirect_uri.unwrap_or_else(|| "http://localhost:8080/callback".to_string());

    let mapped = exchange_code(
        provider,
        &code,
        &redirect,
        code_verifier.as_deref(),
        state.as_deref(),
        None,
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
}
