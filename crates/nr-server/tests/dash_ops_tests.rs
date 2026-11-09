//! Integration tests for /dashboard operations pages in dash_ops.rs.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use std::sync::OnceLock;
use tower::ServiceExt;

static TEST_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

fn app() -> axum::Router {
    TEST_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();
        std::env::set_var("DATA_DIR", &path);
        std::mem::forget(dir);
        path
    });
    nr_server::app()
}

fn auth_cookie() -> String {
    let _ = app();
    let token = nr_server::routes::auth::create_dashboard_auth_token(None).expect("create token");
    format!("auth_token={token}")
}

async fn request(method: &str, uri: &str, cookie: Option<&str>) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    let resp = app()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("response");
    let st = resp.status();
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), 5_000_000)
        .await
        .expect("bytes");
    (st, loc, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn endpoint_guarded_and_renders() {
    let (st, loc, _) = request("GET", "/dashboard/endpoint", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

    let cookie = auth_cookie();
    let (st, _, body) = request("GET", "/dashboard/endpoint", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("API Endpoint"));
    assert!(body.contains("http://localhost:20128/v1"));
    assert!(body.contains("Quick Setup"));
    assert!(body.contains("cURL"));
    assert!(body.contains("Python (OpenAI SDK)"));
}

#[tokio::test]
async fn endpoint_health_check() {
    let (st, _, body) = request("GET", "/dashboard/endpoint/health", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("System Health Check"));
    assert!(body.contains("Healthy"));
}

#[tokio::test]
async fn usage_unauthed_bounces_to_usage_and_authed_renders() {
    let (st, loc, _) = request("GET", "/dashboard/usage", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/usage")));

    let cookie = auth_cookie();
    let (st, _, body) = request("GET", "/dashboard/usage", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Usage"));
    assert!(body.contains("Requests"));
    assert!(body.contains("Prompt tokens"));
    assert!(body.contains("Completion tokens"));
    assert!(body.contains("Tokens per day"));
    assert!(body.contains("By provider"));
    assert!(body.contains("By model"));
}

#[tokio::test]
async fn quota_guarded_and_renders() {
    let (st, loc, _) = request("GET", "/dashboard/quota", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

    let cookie = auth_cookie();
    let (st, _, body) = request("GET", "/dashboard/quota", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Quota &amp; Limits") || body.contains("Quota & Limits"));
    assert!(body.contains("Claude 5h Auto-Ping"));
    assert!(body.contains("Codex 5h Auto-Ping"));
    assert!(body.contains("09:30"));
}

#[tokio::test]
async fn quota_ping_endpoint() {
    let cookie = auth_cookie();
    let (st, _, body) = request("POST", "/dashboard/quota/ping", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Quota ping dispatched"));
}

#[tokio::test]
async fn request_logs_guarded_and_renders() {
    let (st, loc, _) = request("GET", "/dashboard/request-logs", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

    let cookie = auth_cookie();
    let (st, _, body) = request("GET", "/dashboard/request-logs", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Request Logs"));
    assert!(body.contains("Search"));
    assert!(body.contains("Status"));
    assert!(body.contains("API Key"));
    assert!(body.contains("Model"));
    assert!(body.contains("Duration"));
}

#[tokio::test]
async fn request_logs_clear_endpoint() {
    let cookie = auth_cookie();
    let (st, _, body) = request("DELETE", "/dashboard/request-logs", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("cleared"));
}

#[tokio::test]
async fn console_log_guarded_and_renders() {
    let (st, loc, _) = request("GET", "/dashboard/console-log", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

    let cookie = auth_cookie();
    let (st, _, body) = request("GET", "/dashboard/console-log", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Console Output"));
    assert!(body.contains("Live Stream"));
    assert!(body.contains("EventSource"));
}

#[tokio::test]
async fn console_log_clear_endpoint() {
    let cookie = auth_cookie();
    let (st, _, body) = request("DELETE", "/dashboard/console-log", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("cleared"));
}

#[tokio::test]
async fn profile_guarded_and_renders() {
    let (st, loc, _) = request("GET", "/dashboard/profile", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

    let cookie = auth_cookie();
    let (st, _, body) = request("GET", "/dashboard/profile", Some(&cookie)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Account &amp; Session") || body.contains("Account & Session"));
    assert!(body.contains("Password &amp; Access") || body.contains("Password & Access"));
    assert!(body.contains("SSO Configuration"));
    assert!(body.contains("/api/auth/oidc/callback"));
}
