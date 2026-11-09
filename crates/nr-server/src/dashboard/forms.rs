//! Dashboard form handlers (POST / PUT / DELETE).
//!
//! Browser forms submit `application/x-www-form-urlencoded`. Each handler parses
//! the form into the same JSON body the JS frontend sends to the matching
//! `/api/*` route and calls that route handler, so validation, status codes and
//! error messages are identical to the API.
//!
//! htmx targets: list actions swap the whole `#*-list` container (create) or the
//! nearest row (delete); edit forms swap a `#edit-result` fragment.

use axum::extract::{Path, RawForm};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use maud::{html, PreEscaped};
use serde_json::{json, Value};

fn form_to_value(RawForm(bytes): RawForm) -> Value {
    let mut map = serde_json::Map::new();
    let s = String::from_utf8_lossy(&bytes);
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = urlencoding::decode(k).map(|c| c.into_owned()).unwrap_or_else(|_| k.to_string());
        let val = urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_else(|_| v.to_string());
        map.insert(key, Value::String(val));
    }
    Value::Object(map)
}

fn html_response(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn err_fragment(message: &str) -> Response {
    html_response(
        html! { div class="alert alert-error" { (PreEscaped(html_escape(message))) } }
            .into_string(),
    )
}

fn ok_fragment(message: &str) -> Response {
    html_response(
        html! { div class="alert alert-info" { (PreEscaped(html_escape(message))) } }
            .into_string(),
    )
}

/// 200 + `HX-Redirect` (htmx full page load) or a plain 303 for non-htmx posts.
fn redirect(headers: &HeaderMap, target: &str) -> Response {
    if headers.get("hx-request").is_some() {
        return (
            StatusCode::OK,
            [
                (
                    axum::http::HeaderName::from_static("hx-redirect"),
                    HeaderValue::from_str(target).unwrap_or(HeaderValue::from_static("/")),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            "",
        )
            .into_response();
    }
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, target.to_string()), (header::CACHE_CONTROL, "no-store".to_string())],
    )
        .into_response()
}

fn deleted(headers: &HeaderMap, back: &str) -> Response {
    if headers.get("hx-request").is_some() {
        // Empty body removes the targeted row.
        return (StatusCode::OK, "").into_response();
    }
    redirect(headers, back)
}

/// Turn an /api error response into an error fragment (keeps the API message).
async fn err_from(resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, 1_000_000).await.map(|b| b.to_vec()).unwrap_or_default();
    let msg = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("Request failed ({})", parts.status.as_u16()));
    err_fragment(&msg)
}

// ─── Login ──────────────────────────────────────────────────────────────────

/// POST /login — delegates to `POST /api/auth/login`; on success forwards the
/// session cookie and sends the browser to `/`.
pub async fn login_submit(
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    form: RawForm,
) -> Response {
    let body = form_to_value(form);
    let api_resp = crate::routes::auth::login_handler(peer, headers.clone(), Json(body)).await;
    let (parts, body) = api_resp.into_parts();
    if parts.status == StatusCode::OK {
        let mut out = Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::empty())
            .expect("login response builder");
        let out_headers = out.headers_mut();
        if let Some(cookie) = parts.headers.get(header::SET_COOKIE) {
            out_headers.insert(header::SET_COOKIE, cookie.clone());
        }
        if headers.get("hx-request").is_some() {
            out_headers.insert(
                axum::http::HeaderName::from_static("hx-redirect"),
                HeaderValue::from_static("/"),
            );
        } else {
            out_headers.insert(header::LOCATION, HeaderValue::from_static("/"));
            *out.status_mut() = StatusCode::SEE_OTHER;
        }
        return out;
    }

    let error = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(body, 1_000_000).await.map(|b| b.to_vec()).unwrap_or_default(),
    )
    .ok()
    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
    .unwrap_or_else(|| "Login failed".into());

    // htmx posts here targeting #login-result: return just the error fragment.
    if headers.get("hx-request").is_some() {
        return err_fragment(&error);
    }
    html_response(
        html! {
            div class="login-wrap" {
                div class="login-card" {
                    h1 { "9router" }
                    p class="sub" { "Sign in to the management dashboard." }
                    div class="alert alert-error" { (PreEscaped(html_escape(&error))) }
                    form method="post" action="/login" {
                        div class="form-row" {
                            label for="password" { "Password" }
                            input type="password" id="password" name="password" required autofocus;
                        }
                        button class="btn btn-primary" type="submit" { "Sign in" }
                    }
                }
            }
        }
        .into_string(),
    )
}

// ─── Providers ──────────────────────────────────────────────────────────────

pub async fn create_provider(headers: HeaderMap, form: RawForm) -> Response {
    let body = form_to_value(form);
    let resp = crate::routes::api::providers::post_providers(None, Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    if headers.get("hx-request").is_some() {
        return html_response(super::pages::providers_list_fragment());
    }
    redirect(&headers, "/providers")
}

pub async fn update_provider(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        // Blank key means "keep the existing one".
        if obj.get("apiKey").and_then(Value::as_str) == Some("") {
            obj.remove("apiKey");
        }
        if let Some(active) = obj.get("isActive").and_then(Value::as_str) {
            obj.insert("isActive".into(), Value::Bool(active == "true"));
        }
        if let Some(p) = obj.get("priority").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) {
            obj.insert("priority".into(), json!(p));
        }
        // The API rejects unknown readonly fields.
        obj.remove("provider");
    }
    let resp = crate::routes::api::providers::put_provider_by_id(None, Path(id), Json(body)).await;
    if resp.status().is_success() {
        return redirect(&headers, "/providers");
    }
    err_from(resp).await
}

pub async fn delete_provider(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::providers::delete_provider_by_id(None, Path(id)).await;
    if resp.status().is_success() {
        return deleted(&headers, "/providers");
    }
    err_from(resp).await
}

// ─── Keys ───────────────────────────────────────────────────────────────────

pub async fn create_key(headers: HeaderMap, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if let Some(rpm) = obj.get("rpm").and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok()) {
            obj.insert("rpm".into(), json!(rpm));
        }
    }
    let resp = crate::routes::api::keys::post_keys(None, Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    if headers.get("hx-request").is_some() {
        return html_response(super::pages::keys_list_fragment());
    }
    redirect(&headers, "/keys")
}

pub async fn update_key(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if let Some(active) = obj.get("isActive").and_then(Value::as_str) {
            obj.insert("isActive".into(), Value::Bool(active == "true"));
        }
        for k in ["rpm", "limitConcurrent"] {
            if let Some(v) = obj.get(k).and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok()) {
                obj.insert(k.into(), json!(v));
            }
        }
    }
    let resp = crate::routes::api::keys::put_key_by_id(None, Path(id), Json(body)).await;
    if resp.status().is_success() {
        return redirect(&headers, "/keys");
    }
    err_from(resp).await
}

pub async fn delete_key(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::keys::delete_key_by_id(None, Path(id)).await;
    if resp.status().is_success() {
        return deleted(&headers, "/keys");
    }
    err_from(resp).await
}

// ─── Combos ─────────────────────────────────────────────────────────────────

fn models_from_form(obj: &mut serde_json::Map<String, Value>) {
    if let Some(models) = obj.get("models").and_then(Value::as_str) {
        let list: Vec<&str> = models.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
        obj.insert("models".into(), json!(list));
    }
}

pub async fn create_combo(headers: HeaderMap, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        models_from_form(obj);
    }
    let resp = crate::routes::api::combos::post_combos(None, Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    if headers.get("hx-request").is_some() {
        return html_response(super::pages::combos_list_fragment());
    }
    redirect(&headers, "/combos")
}

pub async fn update_combo(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        models_from_form(obj);
    }
    let resp = crate::routes::api::combos::put_combo_by_id(None, Path(id), Json(body)).await;
    if resp.status().is_success() {
        return redirect(&headers, "/combos");
    }
    err_from(resp).await
}

pub async fn delete_combo(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::combos::delete_combo_by_id(None, Path(id)).await;
    if resp.status().is_success() {
        return deleted(&headers, "/combos");
    }
    err_from(resp).await
}

// ─── Settings ───────────────────────────────────────────────────────────────

pub async fn update_settings(_headers: HeaderMap, form: RawForm) -> Response {
    let body = form_to_value(form);
    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        if let Some(v) = obj.get("requireLogin").and_then(Value::as_str) {
            obj.insert("requireLogin".into(), Value::Bool(v == "true"));
        }
    }
    let resp = crate::routes::api::settings::patch_settings(None, Json(body)).await;
    if resp.status().is_success() {
        return ok_fragment("Settings saved.");
    }
    err_from(resp).await
}

pub async fn change_password(_headers: HeaderMap, form: RawForm) -> Response {
    let body = form_to_value(form);
    let resp = crate::routes::api::settings::patch_settings(None, Json(body)).await;
    if resp.status().is_success() {
        return ok_fragment("Password changed.");
    }
    err_from(resp).await
}
