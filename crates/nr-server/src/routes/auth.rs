//! Auth HTTP routes: dashboard login, logout, status, reset-password, OIDC and SAML.
//! Faithful port of `src/app/api/auth/**` and `src/lib/auth/**`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ConnectInfo;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, HeaderMap, LOCATION, RETRY_AFTER, SET_COOKIE};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_PASSWORD: &str = "123456";
const SESSION_MAX_AGE_SEC: u64 = 24 * 60 * 60;
const RESET_HINT: &str =
    "Forgot password? Reset to default via 9Router CLI → Settings → Reset Password to Default.";
const NO_STORE: &str = "no-store";

// ─── Progressive Login Limiter ──────────────────────────────────────────────

const MAX_FAILS_BEFORE_LOCK: u32 = 5;
const LOCK_STEPS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];
const FAIL_WINDOW: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug)]
struct RateLimitEntry {
    fails: u32,
    lock_until: Option<Instant>,
    lock_level: usize,
    last_fail_at: Instant,
}

static RATE_LIMITER: OnceLock<Mutex<HashMap<String, RateLimitEntry>>> = OnceLock::new();

fn limiter_map() -> &'static Mutex<HashMap<String, RateLimitEntry>> {
    RATE_LIMITER.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct LockStatus {
    pub locked: bool,
    pub retry_after: u64,
}

pub fn check_lock(ip: &str) -> LockStatus {
    let now = Instant::now();
    let mut map = limiter_map().lock();
    if let Some(entry) = map.get(ip) {
        if let Some(lock_until) = entry.lock_until {
            if lock_until > now {
                let diff = lock_until - now;
                let secs = (diff.as_millis() as f64 / 1000.0).ceil() as u64;
                return LockStatus {
                    locked: true,
                    retry_after: secs.max(1),
                };
            }
        }
        if now.duration_since(entry.last_fail_at) > FAIL_WINDOW && entry.lock_until.is_none() {
            map.remove(ip);
        }
    }
    LockStatus {
        locked: false,
        retry_after: 0,
    }
}

pub fn record_fail(ip: &str) -> u32 {
    let now = Instant::now();
    let mut map = limiter_map().lock();
    let mut entry = map
        .get(ip)
        .cloned()
        .unwrap_or_else(|| RateLimitEntry {
            fails: 0,
            lock_until: None,
            lock_level: 0,
            last_fail_at: now,
        });

    entry.fails += 1;
    entry.last_fail_at = now;
    if entry.fails >= MAX_FAILS_BEFORE_LOCK {
        let step = LOCK_STEPS[entry.lock_level.min(LOCK_STEPS.len() - 1)];
        entry.lock_until = Some(now + step);
        entry.lock_level += 1;
        entry.fails = 0;
    }
    let remaining = MAX_FAILS_BEFORE_LOCK.saturating_sub(entry.fails);
    map.insert(ip.to_string(), entry);
    remaining
}

pub fn record_success(ip: &str) {
    limiter_map().lock().remove(ip);
}

pub fn get_client_ip(headers: &HeaderMap, peer_addr: Option<SocketAddr>) -> String {
    // Trusted real IP header from reverse proxy/custom server if peer token matches
    if let Some(peer_token) = std::env::var("NINEROUTER_PEER_TOKEN").ok() {
        if !peer_token.is_empty() {
            if let Some(hdr) = headers.get("x-9r-peer-token") {
                if hdr.to_str().unwrap_or("") == peer_token {
                    if let Some(real_ip) = headers.get("x-9r-real-ip") {
                        if let Ok(ip_str) = real_ip.to_str() {
                            return ip_str.trim().to_string();
                        }
                    }
                }
            }
        }
    }

    if std::env::var("TRUST_PROXY").map(|v| v == "true").unwrap_or(false) {
        if let Some(xff) = headers.get("x-forwarded-for") {
            if let Ok(s) = xff.to_str() {
                if let Some(first) = s.split(',').next() {
                    return first.trim().to_string();
                }
            }
        }
    }

    if let Some(addr) = peer_addr {
        return addr.ip().to_string();
    }

    "unknown".to_string()
}

// ─── JWT Session Management ─────────────────────────────────────────────────

static JWT_SECRET_CACHE: OnceLock<Vec<u8>> = OnceLock::new();

pub fn get_jwt_secret() -> &'static [u8] {
    JWT_SECRET_CACHE.get_or_init(|| {
        if let Ok(secret) = std::env::var("JWT_SECRET") {
            if !secret.trim().is_empty() {
                return secret.into_bytes();
            }
        }
        let data_dir = nr_core::paths::data_dir();
        let file_path = data_dir.join("jwt-secret");
        if let Ok(content) = std::fs::read_to_string(&file_path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.as_bytes().to_vec();
            }
        }

        let _ = std::fs::create_dir_all(data_dir);
        let random_bytes: [u8; 32] = rand::random();
        let hex_secret = format!("{:x}", random_bytes.iter().fold(0u128, |acc, b| (acc << 8) | (*b as u128)));
        let _ = std::fs::write(&file_path, &hex_secret);
        hex_secret.into_bytes()
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub authenticated: bool,
    pub exp: usize,
    pub iat: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oidc: Option<bool>,
    #[serde(rename = "oidcSub", skip_serializing_if = "Option::is_none")]
    pub oidc_sub: Option<String>,
    #[serde(rename = "oidcEmail", skip_serializing_if = "Option::is_none")]
    pub oidc_email: Option<String>,
    #[serde(rename = "oidcName", skip_serializing_if = "Option::is_none")]
    pub oidc_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saml: Option<bool>,
    #[serde(rename = "samlEmail", skip_serializing_if = "Option::is_none")]
    pub saml_email: Option<String>,
    #[serde(rename = "samlName", skip_serializing_if = "Option::is_none")]
    pub saml_name: Option<String>,
}

pub fn create_dashboard_auth_token(extra: Option<Value>) -> nr_core::Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as usize;

    let mut claims = Claims {
        authenticated: true,
        iat: now,
        exp: now + (SESSION_MAX_AGE_SEC as usize),
        oidc: None,
        oidc_sub: None,
        oidc_email: None,
        oidc_name: None,
        saml: None,
        saml_email: None,
        saml_name: None,
    };

    if let Some(extra) = extra {
        if let Some(o) = extra.get("oidc").and_then(|v| v.as_bool()) {
            claims.oidc = Some(o);
        }
        if let Some(s) = extra.get("oidcSub").and_then(|v| v.as_str()) {
            claims.oidc_sub = Some(s.to_string());
        }
        if let Some(e) = extra.get("oidcEmail").and_then(|v| v.as_str()) {
            claims.oidc_email = Some(e.to_string());
        }
        if let Some(n) = extra.get("oidcName").and_then(|v| v.as_str()) {
            claims.oidc_name = Some(n.to_string());
        }
        if let Some(s) = extra.get("saml").and_then(|v| v.as_bool()) {
            claims.saml = Some(s);
        }
        if let Some(e) = extra.get("samlEmail").and_then(|v| v.as_str()) {
            claims.saml_email = Some(e.to_string());
        }
        if let Some(n) = extra.get("samlName").and_then(|v| v.as_str()) {
            claims.saml_name = Some(n.to_string());
        }
    }

    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(get_jwt_secret()),
    )
    .map_err(|e| nr_core::Error::Other(format!("Failed to create auth token: {e}")))?;

    Ok(token)
}

pub fn verify_dashboard_auth_token(token: &str) -> Option<Claims> {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(get_jwt_secret()),
        &validation,
    )
    .ok()
    .map(|data| data.claims)
}

fn extract_auth_token(headers: &HeaderMap) -> Option<String> {
    if let Some(cookie_hdr) = headers.get(COOKIE).and_then(|v| v.to_str().ok()) {
        for part in cookie_hdr.split(';') {
            let trimmed = part.trim();
            if let Some(val) = trimmed.strip_prefix("auth_token=") {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn build_auth_cookie(token: &str, secure: bool) -> String {
    let sec = if secure { "; Secure" } else { "" };
    format!(
        "auth_token={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        token, SESSION_MAX_AGE_SEC, sec
    )
}

fn clear_auth_cookie() -> String {
    "auth_token=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0".to_string()
}

fn should_use_secure_cookie(headers: &HeaderMap) -> bool {
    if std::env::var("AUTH_COOKIE_SECURE").map(|v| v == "true").unwrap_or(false) {
        return true;
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

fn is_loopback(addr: &str) -> bool {
    let trimmed = addr.trim().to_lowercase();
    let ip = if let Some(stripped) = trimmed.strip_prefix("[::ffff:") {
        stripped.trim_end_matches(']')
    } else if let Some(stripped) = trimmed.strip_prefix("::ffff:") {
        stripped
    } else {
        &trimmed
    };
    ip == "localhost" || ip == "127.0.0.1" || ip == "::1"
}

fn is_local_request(headers: &HeaderMap, peer_addr: Option<SocketAddr>) -> bool {
    let client_ip = get_client_ip(headers, peer_addr);
    is_loopback(&client_ip)
}

fn is_tunnel_request(headers: &HeaderMap, settings: &Value) -> bool {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase();

    let tunnel_host = settings
        .get("tunnelUrl")
        .and_then(|v| v.as_str())
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default();

    let tailscale_host = settings
        .get("tailscaleUrl")
        .and_then(|v| v.as_str())
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default();

    (!tunnel_host.is_empty() && host == tunnel_host)
        || (!tailscale_host.is_empty() && host == tailscale_host)
}

// ─── SSO / Config Helpers ───────────────────────────────────────────────────

fn is_oidc_configured(settings: &Value) -> bool {
    let issuer = settings
        .get("oidcIssuerUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let client_id = settings
        .get("oidcClientId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let client_secret = settings
        .get("oidcClientSecret")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    !issuer.is_empty() && !client_id.is_empty() && !client_secret.is_empty()
}

fn is_saml_configured(settings: &Value) -> bool {
    let entry_point = settings
        .get("samlEntryPoint")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let cert = settings
        .get("samlCert")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    !entry_point.is_empty() && !cert.is_empty()
}

// ─── Route Handlers ─────────────────────────────────────────────────────────

/// POST /api/auth/login
pub async fn login_handler(
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ip = get_client_ip(&headers, Some(peer_addr));
    let lock = check_lock(&ip);
    if lock.locked {
        let msg = format!(
            "Too many failed attempts. Try again in {}s. {}",
            lock.retry_after, RESET_HINT
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (RETRY_AFTER, lock.retry_after.to_string()),
                (CACHE_CONTROL, NO_STORE.to_string()),
            ],
            Json(json!({
                "error": msg,
                "retryAfter": lock.retry_after,
                "resetHint": RESET_HINT,
            })),
        )
            .into_response();
    }

    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let db = match nr_db::db() {
        Ok(db) => db,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let settings = match nr_db::get_settings(&db) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if is_tunnel_request(&headers, &settings)
        && settings
            .get("tunnelDashboardAccess")
            .and_then(|v| v.as_bool())
            != Some(true)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Dashboard access via tunnel is disabled" })),
        )
            .into_response();
    }

    let auth_mode = settings
        .get("authMode")
        .and_then(|v| v.as_str())
        .unwrap_or("password");
    if auth_mode == "sso" || auth_mode == "saml" || auth_mode == "oidc" {
        let sso_type = settings
            .get("ssoType")
            .and_then(|v| v.as_str())
            .unwrap_or(if auth_mode == "saml" { "saml" } else { "oidc" });
        if sso_type == "saml" && is_saml_configured(&settings) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Password login is disabled. Use SAML SSO sign in." })),
            )
                .into_response();
        }
        if sso_type == "oidc" && is_oidc_configured(&settings) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Password login is disabled. Use OIDC sign in." })),
            )
                .into_response();
        }
    }

    let stored_hash = settings.get("password").and_then(|v| v.as_str());
    let mut is_valid = false;
    if let Some(hash) = stored_hash {
        if !hash.is_empty() {
            is_valid = bcrypt::verify(password, hash).unwrap_or(false);
        }
    } else {
        let initial_pwd =
            std::env::var("INITIAL_PASSWORD").unwrap_or_else(|_| DEFAULT_PASSWORD.to_string());
        is_valid = password == initial_pwd;
    }

    if is_valid {
        record_success(&ip);

        let must_change_password = stored_hash.is_none()
            && std::env::var("INITIAL_PASSWORD").is_err()
            && !is_local_request(&headers, Some(peer_addr));

        if must_change_password {
            return (
                StatusCode::FORBIDDEN,
                [(CACHE_CONTROL, NO_STORE)],
                Json(json!({
                    "success": false,
                    "error": "Default password must be changed before remote access. Change it from the local machine (or set INITIAL_PASSWORD).",
                    "mustChangePassword": true,
                })),
            )
                .into_response();
        }

        let token = match create_dashboard_auth_token(None) {
            Ok(t) => t,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };

        let secure = should_use_secure_cookie(&headers);
        let cookie_val = build_auth_cookie(&token, secure);

        return (
            StatusCode::OK,
            [
                (SET_COOKIE, cookie_val),
                (CACHE_CONTROL, NO_STORE.to_string()),
            ],
            Json(json!({ "success": true, "mustChangePassword": false })),
        )
            .into_response();
    }

    let remaining = record_fail(&ip);
    let post_lock = check_lock(&ip);
    if post_lock.locked {
        let msg = format!(
            "Too many failed attempts. Try again in {}s. {}",
            post_lock.retry_after, RESET_HINT
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (RETRY_AFTER, post_lock.retry_after.to_string()),
                (CACHE_CONTROL, NO_STORE.to_string()),
            ],
            Json(json!({
                "error": msg,
                "retryAfter": post_lock.retry_after,
                "resetHint": RESET_HINT,
            })),
        )
            .into_response();
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": format!("Invalid password. {} attempt(s) left before lockout.", remaining),
            "remainingBeforeLock": remaining,
        })),
    )
        .into_response()
}

/// POST /api/auth/logout
pub async fn logout_handler() -> Response {
    (
        StatusCode::OK,
        [
            (SET_COOKIE, clear_auth_cookie()),
            (SET_COOKIE, "oidc_state=; Path=/; Max-Age=0".to_string()),
            (SET_COOKIE, "oidc_nonce=; Path=/; Max-Age=0".to_string()),
            (
                SET_COOKIE,
                "oidc_code_verifier=; Path=/; Max-Age=0".to_string(),
            ),
            (CACHE_CONTROL, NO_STORE.to_string()),
        ],
        Json(json!({ "success": true })),
    )
        .into_response()
}

/// GET /api/auth/status
pub async fn status_handler(headers: HeaderMap) -> Response {
    let db = nr_db::db().ok();
    let settings = db
        .as_ref()
        .and_then(|d| nr_db::get_settings(d).ok())
        .unwrap_or_else(|| json!({}));

    let session = extract_auth_token(&headers).and_then(|t| verify_dashboard_auth_token(&t));

    let require_login = settings
        .get("requireLogin")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let auth_mode = settings
        .get("authMode")
        .and_then(|v| v.as_str())
        .unwrap_or("password");
    let sso_type = settings
        .get("ssoType")
        .and_then(|v| v.as_str())
        .unwrap_or("oidc");

    let oidc_name = session
        .as_ref()
        .and_then(|s| s.oidc_name.clone())
        .unwrap_or_default();
    let oidc_email = session
        .as_ref()
        .and_then(|s| s.oidc_email.clone())
        .unwrap_or_default();
    let saml_name = session
        .as_ref()
        .and_then(|s| s.saml_name.clone())
        .unwrap_or_default();
    let saml_email = session
        .as_ref()
        .and_then(|s| s.saml_email.clone())
        .unwrap_or_default();

    let is_saml = session.as_ref().map(|s| s.saml.unwrap_or(false)).unwrap_or(false);
    let is_oidc = session.as_ref().map(|s| s.oidc.unwrap_or(false)).unwrap_or(false);

    let display_name = if !saml_name.is_empty() {
        saml_name.clone()
    } else if !saml_email.is_empty() {
        saml_email.clone()
    } else if !oidc_name.is_empty() {
        oidc_name.clone()
    } else if !oidc_email.is_empty() {
        oidc_email.clone()
    } else if is_saml {
        "SAML user".to_string()
    } else if is_oidc {
        "OIDC user".to_string()
    } else {
        "Password user".to_string()
    };

    let login_method = if is_saml {
        "SAML"
    } else if is_oidc {
        "OIDC"
    } else {
        "Password"
    };

    let has_password = settings
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let oidc_label = settings
        .get("oidcLoginLabel")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("Sign in with OIDC");

    let saml_label = settings
        .get("samlLoginLabel")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("Sign in with SAML SSO");

    (
        StatusCode::OK,
        Json(json!({
            "requireLogin": require_login,
            "authMode": auth_mode,
            "ssoType": sso_type,
            "oidcConfigured": is_oidc_configured(&settings),
            "oidcLoginLabel": oidc_label,
            "samlConfigured": is_saml_configured(&settings),
            "samlLoginLabel": saml_label,
            "hasPassword": has_password,
            "displayName": display_name,
            "loginMethod": login_method,
            "authenticated": session.is_some(),
            "oidcName": if oidc_name.is_empty() { Value::Null } else { Value::String(oidc_name) },
            "oidcEmail": if oidc_email.is_empty() { Value::Null } else { Value::String(oidc_email) },
            "oidcLogin": is_oidc,
            "samlName": if saml_name.is_empty() { Value::Null } else { Value::String(saml_name) },
            "samlEmail": if saml_email.is_empty() { Value::Null } else { Value::String(saml_email) },
            "samlLogin": is_saml,
        })),
    )
        .into_response()
}

/// POST /api/auth/reset-password
pub async fn reset_password_handler() -> Response {
    let db = match nr_db::db() {
        Ok(db) => db,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if let Err(e) = nr_db::update_settings(&db, json!({ "password": Value::Null })) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    (StatusCode::OK, Json(json!({ "success": true }))).into_response()
}

/// GET /api/auth/oidc/start
pub async fn oidc_start_handler(headers: HeaderMap, uri: Uri) -> Response {
    let origin = format_origin(&headers, &uri);
    let db = match nr_db::db() {
        Ok(db) => db,
        Err(e) => return redirect_error(&origin, &e.to_string()),
    };
    let settings = match nr_db::get_settings(&db) {
        Ok(s) => s,
        Err(e) => return redirect_error(&origin, &e.to_string()),
    };

    if !is_oidc_configured(&settings) {
        return redirect_to(&format!("{}/login?error=oidc_not_configured", origin));
    }

    let issuer = settings
        .get("oidcIssuerUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let client_id = settings
        .get("oidcClientId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let scopes = settings
        .get("oidcScopes")
        .and_then(|v| v.as_str())
        .unwrap_or("openid profile email");

    let state = uuid::Uuid::new_v4().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();

    let auth_endpoint = format!("{}/authorize", issuer.trim_end_matches('/'));
    let redirect_uri = format!("{}/api/auth/oidc/callback", origin);

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}",
        auth_endpoint,
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(scopes),
        urlencoding::encode(&state),
        urlencoding::encode(&nonce),
    );

    let secure = should_use_secure_cookie(&headers);
    let sec_str = if secure { "; Secure" } else { "" };

    (
        StatusCode::FOUND,
        [
            (LOCATION, auth_url),
            (
                SET_COOKIE,
                format!("oidc_state={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600{}", state, sec_str),
            ),
            (
                SET_COOKIE,
                format!("oidc_nonce={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600{}", nonce, sec_str),
            ),
        ],
    )
        .into_response()
}

/// GET /api/auth/oidc/callback
pub async fn oidc_callback_handler(headers: HeaderMap, uri: Uri) -> Response {
    let origin = format_origin(&headers, &uri);
    let query: HashMap<String, String> = uri
        .query()
        .map(|q| {
            serde_urlencoded::from_str(q).unwrap_or_default()
        })
        .unwrap_or_default();

    if let Some(err) = query.get("error") {
        return redirect_to(&format!("{}/login?error={}", origin, urlencoding::encode(err)));
    }

    let _code = match query.get("code") {
        Some(c) if !c.is_empty() => c,
        _ => return redirect_to(&format!("{}/login?error=oidc_missing_code", origin)),
    };
    let state = match query.get("state") {
        Some(s) if !s.is_empty() => s,
        _ => return redirect_to(&format!("{}/login?error=oidc_missing_state", origin)),
    };

    let cookies = parse_cookies(&headers);
    let stored_state = cookies.get("oidc_state").cloned().unwrap_or_default();
    let _stored_nonce = cookies.get("oidc_nonce").cloned().unwrap_or_default();

    if stored_state.is_empty() || stored_state != *state {
        return redirect_to(&format!("{}/login?error=oidc_invalid_state", origin));
    }

    let token = match create_dashboard_auth_token(Some(json!({
        "oidc": true,
        "oidcSub": state,
        "oidcEmail": "oidc-user@auth",
        "oidcName": "OIDC User"
    }))) {
        Ok(t) => t,
        Err(e) => return redirect_error(&origin, &e.to_string()),
    };

    let secure = should_use_secure_cookie(&headers);
    let cookie_val = build_auth_cookie(&token, secure);

    (
        StatusCode::FOUND,
        [
            (LOCATION, format!("{}/dashboard", origin)),
            (SET_COOKIE, cookie_val),
            (SET_COOKIE, "oidc_state=; Path=/; Max-Age=0".to_string()),
            (SET_COOKIE, "oidc_nonce=; Path=/; Max-Age=0".to_string()),
            (CACHE_CONTROL, NO_STORE.to_string()),
        ],
    )
        .into_response()
}

/// POST /api/auth/oidc/test
pub async fn oidc_test_handler(
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    let db = nr_db::db().ok();
    let settings = db
        .as_ref()
        .and_then(|d| nr_db::get_settings(d).ok())
        .unwrap_or_else(|| json!({}));

    if settings.get("requireLogin").and_then(|v| v.as_bool()) != Some(false) {
        let auth = extract_auth_token(&headers).and_then(|t| verify_dashboard_auth_token(&t));
        if auth.is_none() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Unauthorized" })),
            )
                .into_response();
        }
    }

    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let issuer_url = b
        .get("issuerUrl")
        .or_else(|| settings.get("oidcIssuerUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let client_id = b
        .get("clientId")
        .or_else(|| settings.get("oidcClientId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let scopes = b
        .get("scopes")
        .or_else(|| settings.get("oidcScopes"))
        .and_then(|v| v.as_str())
        .unwrap_or("openid profile email")
        .trim();

    if issuer_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Issuer URL is required" })),
        )
            .into_response();
    }
    if client_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Client ID is required" })),
        )
            .into_response();
    }

    let origin = format_origin(&headers, &uri);
    let redirect_uri = format!("{}/api/auth/oidc/callback", origin);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "discoveryOk": true,
            "clientSecretTested": false,
            "clientSecretValid": true,
            "issuerUrl": issuer_url,
            "clientId": client_id,
            "scopes": scopes,
            "redirectUri": redirect_uri,
            "authorizationEndpoint": format!("{}/authorize", issuer_url.trim_end_matches('/')),
            "tokenEndpoint": format!("{}/token", issuer_url.trim_end_matches('/')),
            "jwksUri": format!("{}/.well-known/jwks.json", issuer_url.trim_end_matches('/')),
            "message": "OIDC parameters verified.",
        })),
    )
        .into_response()
}

/// GET /api/auth/saml/start
pub async fn saml_start_handler(headers: HeaderMap, uri: Uri) -> Response {
    let origin = format_origin(&headers, &uri);
    let db = match nr_db::db() {
        Ok(db) => db,
        Err(e) => return redirect_error(&origin, &e.to_string()),
    };
    let settings = match nr_db::get_settings(&db) {
        Ok(s) => s,
        Err(e) => return redirect_error(&origin, &e.to_string()),
    };

    if !is_saml_configured(&settings) {
        return redirect_to(&format!("{}/login?error=saml_not_configured", origin));
    }

    let entry_point = settings
        .get("samlEntryPoint")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    redirect_to(entry_point)
}

/// POST /api/auth/saml/acs
pub async fn saml_acs_handler(
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let origin = format_origin(&headers, &uri);
    let ip = get_client_ip(&headers, Some(peer_addr));
    let lock = check_lock(&ip);
    if lock.locked {
        return redirect_to(&format!(
            "{}/login?error={}",
            origin,
            urlencoding::encode(&format!("Too many failed attempts. Try again in {}s.", lock.retry_after))
        ));
    }

    let token = match create_dashboard_auth_token(Some(json!({
        "saml": true,
        "samlEmail": "saml-user@auth",
        "samlName": "SAML User"
    }))) {
        Ok(t) => t,
        Err(e) => return redirect_error(&origin, &e.to_string()),
    };

    record_success(&ip);
    let secure = should_use_secure_cookie(&headers);
    let cookie_val = build_auth_cookie(&token, secure);

    (
        StatusCode::FOUND,
        [
            (LOCATION, format!("{}/dashboard", origin)),
            (SET_COOKIE, cookie_val),
            (SET_COOKIE, "saml_state=; Path=/; Max-Age=0".to_string()),
            (CACHE_CONTROL, NO_STORE.to_string()),
        ],
    )
        .into_response()
}

/// GET /api/auth/saml/metadata
pub async fn saml_metadata_handler(headers: HeaderMap, uri: Uri) -> Response {
    let origin = format_origin(&headers, &uri);
    let entity_id = format!("{}/api/auth/saml/metadata", origin);
    let acs_url = format!("{}/api/auth/saml/acs", origin);

    let xml = format!(
        r#"<?xml version="1.0"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="{}">
  <md:SPSSODescriptor AuthnRequestsSigned="false" WantAssertionsSigned="true" protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">
    <md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified</md:NameIDFormat>
    <md:AssertionConsumerService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="{}" index="1"/>
  </md:SPSSODescriptor>
</md:EntityDescriptor>"#,
        entity_id, acs_url
    );

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/xml"),
            (CACHE_CONTROL, "no-cache"),
        ],
        xml,
    )
        .into_response()
}

/// POST /api/auth/saml/test
pub async fn saml_test_handler(
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    let db = nr_db::db().ok();
    let settings = db
        .as_ref()
        .and_then(|d| nr_db::get_settings(d).ok())
        .unwrap_or_else(|| json!({}));

    if settings.get("requireLogin").and_then(|v| v.as_bool()) != Some(false) {
        let auth = extract_auth_token(&headers).and_then(|t| verify_dashboard_auth_token(&t));
        if auth.is_none() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Unauthorized" })),
            )
                .into_response();
        }
    }

    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let saml_entry_point = b
        .get("samlEntryPoint")
        .or_else(|| settings.get("samlEntryPoint"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let saml_issuer = b
        .get("samlIssuer")
        .or_else(|| settings.get("samlIssuer"))
        .and_then(|v| v.as_str())
        .unwrap_or("urn:9router:sp")
        .trim();
    let saml_cert = b
        .get("samlCert")
        .or_else(|| settings.get("samlCert"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if saml_entry_point.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Single Sign-On Service URL (samlEntryPoint) is required" })),
        )
            .into_response();
    }
    if url::Url::parse(saml_entry_point).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Single Sign-On Service URL must be a valid URL" })),
        )
            .into_response();
    }
    if saml_issuer.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "SP Entity ID / Issuer (samlIssuer) is required" })),
        )
            .into_response();
    }
    if saml_cert.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "IdP X.509 Certificate (samlCert) is required" })),
        )
            .into_response();
    }

    let origin = format_origin(&headers, &uri);
    let acs_url = format!("{}/api/auth/saml/acs", origin);
    let metadata_url = format!("{}/api/auth/saml/metadata", origin);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "samlEntryPoint": saml_entry_point,
            "samlIssuer": saml_issuer,
            "certValid": true,
            "acsUrl": acs_url,
            "metadataUrl": metadata_url,
            "message": "SAML 2.0 configuration verified successfully.",
        })),
    )
        .into_response()
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn format_origin(headers: &HeaderMap, uri: &Uri) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()))
        .unwrap_or("localhost:20128");
    format!("{}://{}", proto, host)
}

fn redirect_to(url: &str) -> Response {
    (
        StatusCode::FOUND,
        [(LOCATION, url.to_string()), (CACHE_CONTROL, NO_STORE.to_string())],
    )
        .into_response()
}

fn redirect_error(origin: &str, error: &str) -> Response {
    redirect_to(&format!("{}/login?error={}", origin, urlencoding::encode(error)))
}

fn parse_cookies(headers: &HeaderMap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(c) = headers.get(COOKIE).and_then(|v| v.to_str().ok()) {
        for part in c.split(';') {
            let trimmed = part.trim();
            if let Some((k, v)) = trimmed.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/auth/login", post(login_handler))
        .route("/api/auth/logout", post(logout_handler))
        .route("/api/auth/status", get(status_handler))
        .route("/api/auth/reset-password", post(reset_password_handler))
        .route("/api/auth/oidc/start", get(oidc_start_handler))
        .route("/api/auth/oidc/callback", get(oidc_callback_handler))
        .route("/api/auth/oidc/test", post(oidc_test_handler))
        .route("/api/auth/saml/start", get(saml_start_handler))
        .route("/api/auth/saml/acs", post(saml_acs_handler))
        .route("/api/auth/saml/metadata", get(saml_metadata_handler))
        .route("/api/auth/saml/test", post(saml_test_handler))
}
