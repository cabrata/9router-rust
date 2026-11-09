//! Integration & unit tests for nr-server routes (auth, cli-tools, oauth-flow, usage).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

use nr_server::routes::cli_tools::write_file_atomic;
use nr_server::routes::oauth_flow::{classify_device_poll, DevicePollState};

#[tokio::test]
async fn test_oauth_start_url_shape() {
    let app = nr_server::routes::routes();

    // Call GET /api/oauth/claude/authorize
    let req = Request::builder()
        .method("GET")
        .uri("/api/oauth/claude/authorize?redirect_uri=http://localhost:8080/callback")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();

    assert!(json.get("authUrl").is_some(), "Must have authUrl");
    let auth_url = json["authUrl"].as_str().unwrap();
    assert!(
        auth_url.contains("authorize"),
        "URL must point to authorize endpoint: {auth_url}"
    );
    assert!(auth_url.contains("client_id="), "URL must contain client_id");
    assert!(auth_url.contains("state="), "URL must contain state");
    assert!(
        auth_url.contains("code_challenge="),
        "URL must contain PKCE code_challenge"
    );

    assert!(json.get("state").is_some(), "Must return state");
    assert!(json.get("codeVerifier").is_some(), "Must return codeVerifier");
    assert!(json.get("codeChallenge").is_some(), "Must return codeChallenge");
    assert_eq!(
        json["redirectUri"].as_str().unwrap(),
        "http://localhost:8080/callback"
    );
}

#[tokio::test]
async fn test_oauth_device_code_flow_start() {
    let app = nr_server::routes::routes();

    // Call GET /api/oauth/github/device-code
    let req = Request::builder()
        .method("GET")
        .uri("/api/oauth/github/device-code")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();

    assert!(json.get("deviceCode").is_some());
    assert!(json.get("userCode").is_some());
    assert!(json.get("verificationUri").is_some());
    assert!(json.get("interval").is_some());
}

#[tokio::test]
async fn test_oauth_missing_provider_returns_501() {
    let app = nr_server::routes::routes();

    let req = Request::builder()
        .method("GET")
        .uri("/api/oauth/nonexistent_provider_xyz/authorize")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("nonexistent_provider_xyz"),
        "Error message must name the provider"
    );
}

#[test]
fn test_device_polling_state_machine() {
    // 1. Authorized response
    let authorized = serde_json::json!({
        "success": true,
        "connection": {
            "id": "conn_123",
            "provider": "github"
        }
    });
    assert_eq!(
        classify_device_poll(&authorized),
        DevicePollState::Authorized
    );

    // 2. Pending response
    let pending = serde_json::json!({
        "success": false,
        "error": "authorization_pending",
        "pending": true
    });
    assert_eq!(classify_device_poll(&pending), DevicePollState::Pending);

    // 3. Slow down response
    let slow_down = serde_json::json!({
        "success": false,
        "error": "slow_down",
        "pending": true
    });
    assert_eq!(classify_device_poll(&slow_down), DevicePollState::SlowDown);

    // 4. Fatal error (expired or invalid code)
    let expired = serde_json::json!({
        "success": false,
        "error": "expired_token",
        "fatal": true
    });
    assert_eq!(classify_device_poll(&expired), DevicePollState::Fatal);

    // 5. Access denied (fatal)
    let denied = serde_json::json!({
        "success": false,
        "error": "access_denied"
    });
    assert_eq!(classify_device_poll(&denied), DevicePollState::Fatal);
}

#[test]
fn test_cli_tool_atomic_write_and_backup() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");

    // 1. First write: initial content
    let initial_content = "model = \"test-model\"\n";
    write_file_atomic(&config_path, initial_content).unwrap();

    assert!(config_path.exists());
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        initial_content
    );

    // 2. Second write: overwrite with backup preservation
    let updated_content = "model = \"updated-model\"\nmodel_provider = \"9router\"\n";
    write_file_atomic(&config_path, updated_content).unwrap();

    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        updated_content
    );

    // Check backup file was created
    let backup_path = config_path.with_extension("toml.bak");
    assert!(
        backup_path.exists(),
        "Backup file must be created at {:?}",
        backup_path
    );
    assert_eq!(
        fs::read_to_string(&backup_path).unwrap(),
        initial_content,
        "Backup must hold previous content"
    );
}

#[tokio::test]
async fn test_auth_status_endpoint() {
    let app = nr_server::routes::routes();

    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/status")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();

    assert!(json.get("requireLogin").is_some());
    assert!(json.get("authMode").is_some());
    assert!(json.get("authenticated").is_some());
    assert_eq!(json["authenticated"].as_bool().unwrap(), false);
}

#[tokio::test]
async fn test_usage_chart_endpoint() {
    let app = nr_server::routes::routes();

    let req = Request::builder()
        .method("GET")
        .uri("/api/usage/chart?period=7d")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_usage_providers_endpoint() {
    let app = nr_server::routes::routes();

    let req = Request::builder()
        .method("GET")
        .uri("/api/usage/providers")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(json.get("providers").is_some());
}

#[tokio::test]
async fn test_usage_request_details_redaction() {
    let app = nr_server::routes::routes();

    let req = Request::builder()
        .method("GET")
        .uri("/api/usage/request-details?page=1&pageSize=10")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(json.get("details").is_some());
    assert_eq!(json["page"].as_u64().unwrap(), 1);
    assert_eq!(json["pageSize"].as_u64().unwrap(), 10);
}
