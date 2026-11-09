//! Integration tests for /dashboard/providers, /dashboard/media-providers, and /dashboard/proxy-pools.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use std::sync::OnceLock;
use tower::ServiceExt;

static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

fn app() -> axum::Router {
    DIR.get_or_init(|| {
        let td = tempfile::TempDir::new().expect("tempdir");
        std::env::set_var("DATA_DIR", td.path());
        td
    });
    nr_server::app()
}

fn auth_cookie() -> String {
    let _ = app();
    let token = nr_server::routes::auth::create_dashboard_auth_token(None).unwrap();
    format!("auth_token={token}")
}

async fn req_get(uri: &str, cookie: Option<&str>) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder().uri(uri);
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app().oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, location, String::from_utf8_lossy(&bytes).into_owned())
}

async fn req_post_form(uri: &str, cookie: Option<&str>, form: &str) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    let req = builder.body(Body::from(form.to_string())).unwrap();
    let resp = app().oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, location, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn test_providers_unauthenticated_fallback_redirect() {
    let (st, loc, _) = req_get("/dashboard/providers?x=1", None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/providers?x=1"));
}

#[tokio::test]
async fn test_providers_list_authenticated() {
    let c = auth_cookie();
    let (st, _, body) = req_get("/dashboard/providers", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("AI Provider Connections"));
    assert!(body.contains("OpenAI Compatible"));
    assert!(body.contains("Anthropic Compatible"));
    assert!(body.contains("Supported Providers Catalog"));
}

#[tokio::test]
async fn test_providers_search_and_filter() {
    let c = auth_cookie();
    let (st, _, body) = req_get("/dashboard/providers?q=openai&status=all", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("OpenAI"));
}

#[tokio::test]
async fn test_providers_new_page() {
    let c = auth_cookie();
    let (st, _, body) = req_get("/dashboard/providers/new", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Add New Provider Connection"));
    assert!(body.contains("AI Provider"));
    assert!(body.contains("Authentication Type"));
}

#[tokio::test]
async fn test_create_provider_connection_and_detail_view() {
    let c = auth_cookie();
    let form = "provider=openai&name=IntegrationTestKey&apiKey=sk-test-123456&priority=1&isActive=on";
    let (st, loc, _) = req_post_form("/dashboard/providers", Some(&c), form).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/dashboard/providers"));

    // Check detail page for OpenAI
    let (st, _, body) = req_get("/dashboard/providers/openai", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("IntegrationTestKey"));
    assert!(body.contains("Model Catalog &amp; Overrides"));
}

#[tokio::test]
async fn test_create_compatible_provider() {
    let c = auth_cookie();
    let form = "type=openai-compatible&name=LocalOllama&prefix=ollama&baseUrl=http%3A%2F%2Flocalhost%3A11434%2Fv1&apiKey=ollama-secret";
    let (st, loc, _) = req_post_form("/dashboard/providers/compatible", Some(&c), form).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/dashboard/providers"));

    let (st, _, body) = req_get("/dashboard/providers", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("LocalOllama"));
}

#[tokio::test]
async fn test_media_providers_hub_and_kinds() {
    let c = auth_cookie();

    // Hub page
    let (st, _, body) = req_get("/dashboard/media-providers", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Media Providers"));
    assert!(body.contains("Text To Speech"));
    assert!(body.contains("Speech To Text"));
    assert!(body.contains("Text to Image"));
    assert!(body.contains("Video"));

    // Kind page: TTS
    let (st, _, body) = req_get("/dashboard/media-providers/tts", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Text To Speech Providers"));
    assert!(body.contains("/v1/audio/speech"));

    // Kind detail page: TTS OpenAI
    let (st, _, body) = req_get("/dashboard/media-providers/tts/openai", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("OpenAI (Text To Speech)"));
    assert!(body.contains("API Usage Example"));
}

#[tokio::test]
async fn test_proxy_pools_lifecycle() {
    let c = auth_cookie();

    // 1. Initial page render
    let (st, _, body) = req_get("/dashboard/proxy-pools", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Outbound Proxy Pools &amp; Relays"));

    // 2. Create proxy pool
    let form = "name=TestUSProxy&proxyUrl=http%3A%2F%2Fproxy.us.example.com%3A8080&noProxy=localhost&isActive=on&strictProxy=off";
    let (st, loc, _) = req_post_form("/dashboard/proxy-pools", Some(&c), form).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/dashboard/proxy-pools"));

    // 3. Verify created pool appears in list
    let (st, _, body) = req_get("/dashboard/proxy-pools", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("TestUSProxy"));
    assert!(body.contains("http://proxy.us.example.com:8080"));

    // 4. Test deploy vercel relay
    let vercel_form = "vercelToken=tok_test_12345&projectName=relay-test";
    let (st, loc, _) = req_post_form("/dashboard/proxy-pools/deploy/vercel", Some(&c), vercel_form).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/dashboard/proxy-pools"));
}
