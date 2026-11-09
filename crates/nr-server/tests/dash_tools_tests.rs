//! Integration tests for /dashboard tools pages in dash_tools.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

fn cookie() -> String {
    let token = nr_server::routes::auth::create_dashboard_auth_token(None).expect("token");
    format!("auth_token={token}")
}

#[tokio::test]
async fn test_cli_tools_page_and_detail() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    // GET /dashboard/cli-tools
    let req = Request::builder()
        .uri("/dashboard/cli-tools")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("CLI Tools Integration"));
    assert!(html.contains("Claude Code"));

    // GET /dashboard/cli-tools/claude
    let req = Request::builder()
        .uri("/dashboard/cli-tools/claude")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("ANTHROPIC_BASE_URL"));

    // Nonexistent tool -> 404
    let req = Request::builder()
        .uri("/dashboard/cli-tools/no_such_tool")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cli_tools_apply_and_restore() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    let body = "model=sonnet&apiKey=sk_test&baseUrl=http%3A%2F%2Flocalhost%3A20128%2Fv1";
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/cli-tools/claude/apply")
        .header(header::COOKIE, &auth)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/cli-tools/claude/restore")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_mitm_page_and_actions() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    // GET /dashboard/mitm
    let req = Request::builder()
        .uri("/dashboard/mitm")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("MITM Proxy Status"));
    assert!(html.contains("Antigravity IDE"));

    // POST /dashboard/mitm/toggle
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/mitm/toggle")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // POST /dashboard/mitm/alias
    let body = "tool=antigravity&model=test-model&target=openai%2Fgpt-4o";
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/mitm/alias")
        .header(header::COOKIE, &auth)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_translator_page_and_test() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    // GET /dashboard/translator
    let req = Request::builder()
        .uri("/dashboard/translator")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Pipeline Architecture (7 Stages)"));

    // POST /dashboard/translator/test
    let body = "source_format=openai&target_format=claude&body=%7B%22model%22%3A%22gpt-4o%22%2C%22messages%22%3A%5B%7B%22role%22%3A%22user%22%2C%22content%22%3A%22hi%22%7D%5D%7D";
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/translator/test")
        .header(header::COOKIE, &auth)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Translated Output"));
}

#[tokio::test]
async fn test_token_saver_page_and_save() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    // GET /dashboard/token-saver
    let req = Request::builder()
        .uri("/dashboard/token-saver")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Real-Time Knowledge"));

    // POST /dashboard/token-saver
    let body = "rtkEnabled=on&cavemanEnabled=on&cavemanLevel=ultra&ponytailEnabled=on&ponytailLevel=full";
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/token-saver")
        .header(header::COOKIE, &auth)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Token saver settings saved successfully."));
}

#[tokio::test]
async fn test_pxpipe_page_and_actions() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    // GET /dashboard/pxpipe
    let req = Request::builder()
        .uri("/dashboard/pxpipe")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Pxpipe Sidecar Status"));

    // POST /dashboard/pxpipe/start
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/pxpipe/start")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_skills_page() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    let req = Request::builder()
        .uri("/dashboard/skills")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("9Router (Entry)"));
    assert!(html.contains("9router-chat"));
    assert!(html.contains("START HERE"));
}

#[tokio::test]
async fn test_basic_chat_page_and_send() {
    let app = nr_server::dashboard::dash_tools::router();
    let auth = cookie();

    // GET /dashboard/basic-chat
    let req = Request::builder()
        .uri("/dashboard/basic-chat")
        .header(header::COOKIE, &auth)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Basic Chat Test Interface"));

    // POST /dashboard/basic-chat/send
    let body = "model=anthropic%2Fclaude-3-7-sonnet&message=Hello+World";
    let req = Request::builder()
        .method("POST")
        .uri("/dashboard/basic-chat/send")
        .header(header::COOKIE, &auth)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Hello World"));
}
