//! Dashboard (HTML-over-the-wire) integration tests.
//!
//! Requests go through the real `nr_server::app()` stack against a temp
//! `DATA_DIR`, so the guard, the maud renderers and the shared `nr_db` repos
//! are covered end to end. Sessions are minted directly with
//! `create_dashboard_auth_token` (the same token the login route issues) so
//! tests do not depend on the stored password.
//!
//! The tests share one temp database, so run them single-threaded:
//! `cargo test -p nr-server --test dashboard_pages -- --test-threads=1`.

#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, HeaderValue, Request, StatusCode};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

static TEST_DIR: OnceLock<String> = OnceLock::new();
static TEST_DB_LOCK: Mutex<()> = Mutex::new(());

fn data_dir() -> String {
    TEST_DIR
        .get_or_init(|| {
            let dir = TempDir::new().expect("create temp dir");
            let path = dir.into_path();
            // Must be set before the first `nr_db::db()` call anywhere in the test
            // process: the login guard and every dashboard handler resolve the DB
            // through `nr_db::db()`, which pins DATA_DIR on first use.
            std::env::set_var("DATA_DIR", &path);
            path.display().to_string()
        })
        .clone()
}

/// Serializes the CRUD tests against the shared temp DB. Cargo runs test
/// binaries in parallel threads by default, which would race on the same rows.
fn db_guard() -> std::sync::MutexGuard<'static, ()> {
    data_dir();
    TEST_DB_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn app() -> axum::Router {
    // Resolve the shared DB first so DATA_DIR is honored.
    nr_server::app().layer(axum::Extension(db()))
}

fn cookie() -> String {
    let token = nr_server::routes::auth::create_dashboard_auth_token(None).expect("mint session");
    format!("auth_token={token}")
}

struct Page {
    status: StatusCode,
    body: String,
    headers: Vec<(String, String)>,
}

impl Page {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

async fn send(req: Request<Body>) -> Page {
    let resp = app().oneshot(req).await.expect("handler");
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000).await.expect("body");
    Page {
        status,
        body: String::from_utf8_lossy(&bytes).to_string(),
        headers,
    }
}

async fn get(uri: &str) -> Page {
    send(Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()).await
}

async fn get_as(uri: &str, cookie: &str) -> Page {
    send(
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn form(method: &str, uri: &str, cookie: Option<&str>, body: &str, htmx: bool) -> Page {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"));
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    if htmx {
        req = req.header("hx-request", "true");
    }
    send(req.body(Body::from(body.to_string())).unwrap()).await
}

fn db() -> nr_db::Db {
    data_dir();
    nr_db::db().expect("open test db")
}

fn set_require_login(require: bool) {
    nr_db::update_settings(&db(), serde_json::json!({ "requireLogin": require })).expect("update settings");
}

// ─── Static surface ─────────────────────────────────────────────────────────

#[tokio::test]
async fn login_page_renders() {
    let _db = db_guard();
    set_require_login(true);
    let page = get("/login").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.header("content-type").unwrap_or("").starts_with("text/html"));
    for marker in ["9router", "Password", "action=\"/login\"", "htmx.min.js", "dashboard.css"] {
        assert!(page.body.contains(marker), "login page missing {marker:?}");
    }
}

#[tokio::test]
async fn static_assets_served() {
    let js = get("/static/htmx.min.js").await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(js.header("content-type").unwrap_or("").contains("javascript"));
    assert!(js.body.contains("htmx"));

    let css = get("/static/dashboard.css").await;
    assert_eq!(css.status, StatusCode::OK);
    assert!(css.header("content-type").unwrap_or("").contains("text/css"));
    assert!(css.body.contains("--accent"));

    let fav = get("/favicon.ico").await;
    assert_eq!(fav.status, StatusCode::OK);
    assert!(fav.header("content-type").unwrap_or("").contains("svg"));

    assert_eq!(get("/static/nope.txt").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_redirect_to_login_when_unauthenticated() {
    let _db = db_guard();
    set_require_login(true);
    for path in ["/", "/providers", "/keys", "/combos", "/usage", "/oauth", "/settings"] {
        let page = get(path).await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "GET {path}");
        assert_eq!(page.header("location"), Some("/login"), "GET {path}");
    }
}

#[tokio::test]
async fn all_pages_render_when_authenticated() {
    let _db = db_guard();
    set_require_login(true);
    let cookie = cookie();
    let cases: &[(&str, &[&str])] = &[
        ("/", &["Server Status", "Quick links", "Providers", "API Keys"]),
        ("/providers", &["Connections", "Add provider"]),
        ("/keys", &["Create API key"]),
        ("/combos", &["Create combo", "Models"]),
        ("/usage", &["Requests (7d)", "Tokens per day", "By provider"]),
        ("/oauth", &["OAuth providers", "device-code", "Existing connections"]),
        ("/settings", &["Change password", "Require login", "Preferences"]),
    ];
    for (path, markers) in cases {
        let page = get_as(path, &cookie).await;
        assert_eq!(page.status, StatusCode::OK, "GET {path}");
        for m in *markers {
            assert!(page.body.contains(m), "GET {path}: missing {m:?}");
        }
        assert!(page.body.contains("hx-post") || page.body.contains("hx-get"), "GET {path}: htmx wired");
    }
}

#[tokio::test]
async fn login_post_sets_cookie_and_redirects() {
    let _db = db_guard();
    set_require_login(true);
    // Password is whatever earlier tests left behind; set a known one.
    let hash = bcrypt::hash("login-test-pass", 4).expect("hash");
    nr_db::update_settings(&db(), serde_json::json!({ "password": hash })).unwrap();
    let mut req = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"))
        .body(Body::from("password=login-test-pass"))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
    let page = send(req).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "body: {}", page.body);
    assert_eq!(page.header("location"), Some("/"));
    let set_cookie = page.header("set-cookie").unwrap_or("");
    assert!(set_cookie.starts_with("auth_token="), "cookie: {set_cookie}");
}

#[tokio::test]
async fn logout_clears_cookie() {
    let page = form("POST", "/logout", Some(&cookie()), "", false).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.header("location"), Some("/login"));
    assert!(page.header("set-cookie").unwrap_or("").contains("Max-Age=0"));
}

// ─── Providers ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn providers_crud_roundtrip() {
    let _db = db_guard();
    set_require_login(true);
    let cookie = cookie();

    let created = form(
        "POST",
        "/providers",
        Some(&cookie),
        "provider=ollama-local&name=Dash+Ollama&apiKey=",
        true,
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "create: {}", created.body);
    assert!(created.body.contains("id=\"providers-list\""), "fragment container");
    assert!(created.body.contains("Dash Ollama"), "new row rendered");

    let conn_id = find_id("providers", "name", "Dash Ollama").expect("connection persisted");
    let edit = get_as(&format!("/providers/{conn_id}"), &cookie).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains("Edit provider"));
    assert!(edit.body.contains("Dash Ollama"));

    let updated = form(
        "PUT",
        &format!("/providers/{conn_id}"),
        Some(&cookie),
        "name=Dash+Ollama+Renamed&priority=5&isActive=true&apiKey=",
        true,
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "update: {}", updated.body);
    assert_eq!(updated.header("hx-redirect"), Some("/providers"));
    let renamed = nr_db::get_provider_connection_by_id(&db(), &conn_id).unwrap().unwrap();
    assert_eq!(renamed["name"], Value::String("Dash Ollama Renamed".into()));

    let deleted = form("DELETE", &format!("/providers/{conn_id}"), Some(&cookie), "", true).await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert!(deleted.body.is_empty(), "htmx row removal body");
    assert!(
        nr_db::get_provider_connection_by_id(&db(), &conn_id).unwrap().is_none(),
        "connection deleted"
    );
}

#[tokio::test]
async fn create_provider_without_key_fails_with_api_message() {
    let _db = db_guard();
    set_require_login(true);
    let page = form("POST", "/providers", Some(&cookie()), "provider=openai&name=NoKey", true).await;
    assert_eq!(page.status, StatusCode::BAD_REQUEST);
    assert!(page.body.contains("alert-error"));
    assert!(page.body.contains("API Key is required"), "body: {}", page.body);
}

// ─── Keys ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn keys_crud_roundtrip() {
    let _db = db_guard();
    set_require_login(true);
    let cookie = cookie();

    let bad = form("POST", "/keys", Some(&cookie), "name=&rpm=60", true).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.body.contains("Name is required"));

    let created = form("POST", "/keys", Some(&cookie), "name=dash-key&rpm=120", true).await;
    assert_eq!(created.status, StatusCode::OK, "create: {}", created.body);
    assert!(created.body.contains("dash-key"));
    assert!(created.body.contains("120"));

    let key_id = find_id("keys", "name", "dash-key").expect("key persisted");
    let edit = get_as(&format!("/keys/{key_id}"), &cookie).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains("Edit API key"));

    let updated = form(
        "PUT",
        &format!("/keys/{key_id}"),
        Some(&cookie),
        "name=dash-key&rpm=300&isActive=false",
        true,
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "update: {}", updated.body);
    let rpm = nr_db::get_api_key_by_id(&db(), &key_id)
        .unwrap()
        .map(|k| k.rpm)
        .unwrap_or(0);
    assert_eq!(rpm, 300);

    let revoked = form("DELETE", &format!("/keys/{key_id}"), Some(&cookie), "", true).await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert!(nr_db::get_api_key_by_id(&db(), &key_id).unwrap().is_none());
}

// ─── Combos ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn combos_crud_roundtrip() {
    let _db = db_guard();
    set_require_login(true);
    let cookie = cookie();

    let created = form(
        "POST",
        "/combos",
        Some(&cookie),
        "name=dash-combo&models=openai%3Agpt-4o%0Aanthropic%3Aclaude-sonnet-4-5",
        true,
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "create: {}", created.body);
    assert!(created.body.contains("dash-combo"));
    assert!(created.body.contains("openai:gpt-4o"), "models rendered");
    assert!(created.body.contains("→"), "arrow join");

    let combo_id = find_id("combos", "name", "dash-combo").expect("combo persisted");
    let edit = get_as(&format!("/combos/{combo_id}"), &cookie).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains("Edit combo"));
    assert!(edit.body.contains("openai:gpt-4o"));

    let updated = form(
        "PUT",
        &format!("/combos/{combo_id}"),
        Some(&cookie),
        "name=dash-combo&models=openai%3Agpt-4o-mini",
        true,
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "update: {}", updated.body);
    let models = nr_db::get_combo_by_id(&db(), &combo_id)
        .unwrap()
        .map(|c| c.models)
        .unwrap_or_default();
    assert_eq!(models, vec!["openai:gpt-4o-mini".to_string()]);

    let deleted = form("DELETE", &format!("/combos/{combo_id}"), Some(&cookie), "", true).await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert!(nr_db::get_combo_by_id(&db(), &combo_id).unwrap().is_none());
}

// ─── Settings ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn settings_toggle_is_persisted() {
    let _db = db_guard();
    set_require_login(true);
    let cookie = cookie();

    let page = form("POST", "/settings", Some(&cookie), "requireLogin=false", true).await;
    assert_eq!(page.status, StatusCode::OK, "body: {}", page.body);
    assert!(page.body.contains("Settings saved."));
    let settings = nr_db::get_settings(&db()).unwrap();
    assert_eq!(settings["requireLogin"], Value::Bool(false));

    set_require_login(true);
    let back = form("POST", "/settings", Some(&cookie), "requireLogin=true", true).await;
    assert!(back.body.contains("Settings saved."));
    assert_eq!(nr_db::get_settings(&db()).unwrap()["requireLogin"], Value::Bool(true));
}

#[tokio::test]
async fn password_change_hashes_and_rejects_wrong_current() {
    let _db = db_guard();
    set_require_login(true);
    let cookie = cookie();

    // Set an initial password.
    let first = form("POST", "/settings/password", Some(&cookie), "newPassword=first-pass-1", true).await;
    assert_eq!(first.status, StatusCode::OK, "body: {}", first.body);
    assert!(first.body.contains("Password changed."));
    let hash = nr_db::get_settings(&db()).unwrap()["password"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(bcrypt::verify("first-pass-1", &hash).unwrap_or(false));

    // Wrong current password is rejected.
    let wrong = form(
        "POST",
        "/settings/password",
        Some(&cookie),
        "currentPassword=wrong&newPassword=second-pass",
        true,
    )
    .await;
    assert!(wrong.body.contains("Invalid current password"), "body: {}", wrong.body);

    // Correct current password succeeds.
    let right = form(
        "POST",
        "/settings/password",
        Some(&cookie),
        "currentPassword=first-pass-1&newPassword=second-pass",
        true,
    )
    .await;
    assert!(right.body.contains("Password changed."), "body: {}", right.body);
    let hash = nr_db::get_settings(&db()).unwrap()["password"].as_str().unwrap_or_default().to_string();
    assert!(bcrypt::verify("second-pass", &hash).unwrap_or(false));

    // Restore the default-password state for any later test.
    nr_db::update_settings(&db(), serde_json::json!({ "password": Value::Null })).unwrap();
}

// ─── Usage + OAuth ──────────────────────────────────────────────────────────

#[tokio::test]
async fn usage_page_renders_stats_and_chart() {
    set_require_login(true);
    let page = get_as("/usage", &cookie()).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Requests (7d)"));
    assert!(page.body.contains("Prompt tokens"));
    assert!(page.body.contains("Tokens per day (7d)"));
    assert!(page.body.contains("class=\"bar\""), "inline bar chart markup");
}

#[tokio::test]
async fn oauth_page_links_device_code_flows() {
    set_require_login(true);
    let page = get_as("/oauth", &cookie()).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/api/oauth/anthropic/device-code"));
    assert!(page.body.contains("api/oauth/github/device-code"));
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Look up a record id by a JSON field, across the API-shaped lists the pages use.
fn find_id(kind: &str, field: &str, value: &str) -> Option<String> {
    let db = db();
    let items: Vec<Value> = match kind {
        "providers" => nr_db::get_provider_connections(&db, None).ok()?,
        "keys" => nr_db::get_api_keys(&db)
            .ok()?
            .into_iter()
            .map(|k| serde_json::json!({ "id": k.id, "name": k.name }))
            .collect(),
        "combos" => nr_db::get_combos(&db)
            .ok()?
            .into_iter()
            .map(|c| serde_json::json!({ "id": c.id, "name": c.name }))
            .collect(),
        _ => return None,
    };
    items
        .iter()
        .find(|item| item.get(field).and_then(Value::as_str) == Some(value))
        .and_then(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
}
