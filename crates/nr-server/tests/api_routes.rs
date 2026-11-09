//! Integration tests for the /api management routes.

use std::sync::OnceLock;

use axum::{Extension, Router};
use nr_db::Db;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

static TEST_DB: OnceLock<Db> = OnceLock::new();

fn test_db() -> Db {
    TEST_DB
        .get_or_init(|| {
            let dir = TempDir::new().expect("create temp dir");
            let leaked = dir.into_path();
            std::env::set_var("DATA_DIR", &leaked);
            Db::open_in(&leaked).expect("open test db")
        })
        .clone()
}

fn router() -> Router {
    let db = test_db();
    nr_server::api_routes().layer(Extension(db))
}

async fn call(method: &str, uri: &str, body: Option<Value>) -> (axum::http::StatusCode, Value) {
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let req = match body {
        Some(b) => req.body(axum::body::Body::from(serde_json::to_vec(&b).unwrap())),
        None => req.body(axum::body::Body::empty()),
    }
    .unwrap();
    let resp = router().oneshot(req).await.expect("call handler");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn health_and_init() {
    let (s, j) = call("GET", "/api/health", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["ok"], Value::Bool(true));

    let (s, _) = call("GET", "/api/init", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn keys_crud() {
    // Create
    let (s, j) = call("POST", "/api/keys", Some(json!({ "name": "test-key" }))).await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {j}");
    let key_id = j["id"].as_str().expect("id").to_string();
    let key_secret = j["key"].as_str().expect("key").to_string();
    assert!(!key_secret.is_empty());

    // Missing name is 400
    let (s, _) = call("POST", "/api/keys", Some(json!({}))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // List
    let (s, j) = call("GET", "/api/keys", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let listed = j["keys"].as_array().expect("keys array");
    assert!(listed.iter().any(|k| k["id"].as_str() == Some(&key_id)));
    assert!(listed[0].get("monitoring").is_some(), "monitoring attached");

    // Get single
    let (s, j) = call("GET", &format!("/api/keys/{key_id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["key"]["id"], Value::String(key_id.clone()));

    // Update
    let (s, j) = call(
        "PUT",
        &format!("/api/keys/{key_id}"),
        Some(json!({ "name": "renamed", "isActive": false, "rpm": 42 })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "update: {j}");
    assert_eq!(j["key"]["name"], Value::String("renamed".into()));
    assert_eq!(j["key"]["isActive"], Value::Bool(false));
    assert_eq!(j["key"]["rpm"], Value::Number(42.into()));

    // Invalid name
    let (s, _) = call("PUT", &format!("/api/keys/{key_id}"), Some(json!({ "name": " " }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // 404 on unknown id
    let (s, _) = call("GET", "/api/keys/does-not-exist", None).await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);

    // Delete
    let (s, _) = call("DELETE", &format!("/api/keys/{key_id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let (s, _) = call("GET", &format!("/api/keys/{key_id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn combos_crud() {
    let (s, j) = call(
        "POST",
        "/api/combos",
        Some(json!({ "name": "test-combo", "models": ["gpt-4"] })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {j}");
    let id = j["id"].as_str().expect("id").to_string();
    assert_eq!(j["name"], Value::String("test-combo".into()));

    // Bad name
    let (s, _) = call("POST", "/api/combos", Some(json!({ "name": "bad name!" }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // Duplicate
    let (s, _) = call(
        "POST",
        "/api/combos",
        Some(json!({ "name": "test-combo", "models": ["gpt-4"] })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // List
    let (s, j) = call("GET", "/api/combos", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["combos"].as_array().expect("combos").iter().any(|c| c["id"].as_str() == Some(&id)));

    // Get / Update
    let (s, j) = call("GET", &format!("/api/combos/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["id"], Value::String(id.clone()));

    let (s, j) = call(
        "PUT",
        &format!("/api/combos/{id}"),
        Some(json!({ "models": ["gpt-4", "claude-3"] })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["models"], json!(["gpt-4", "claude-3"]));

    // 404 unknown
    let (s, _) = call("GET", "/api/combos/does-not-exist", None).await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);

    // Delete
    let (s, _) = call("DELETE", &format!("/api/combos/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn combos_presets() {
    // Missing source
    let (s, _) = call("GET", "/api/combos/presets", None).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // Bad source
    let (s, _) = call("GET", "/api/combos/presets?source=unknown", None).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    let (s, j) = call("GET", "/api/combos/presets?source=claude", None).await;
    assert_eq!(s, axum::http::StatusCode::OK, "presets: {j}");
    assert!(j["items"].is_array());
    assert!(j["toCreate"].as_u64().is_some());

    let (s, j) = call("POST", "/api/combos/presets", Some(json!({ "source": "claude" }))).await;
    assert_eq!(s, axum::http::StatusCode::OK, "post presets: {j}");
    assert!(j["created"].is_array());
    assert!(j["skipped"].is_array());
}

#[tokio::test]
async fn settings_get_patch() {
    let (s, j) = call("GET", "/api/settings", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["hasPassword"].is_boolean(), "has hasPassword");
    assert!(j.get("password").is_none(), "password stripped");
    assert!(j.get("oidcClientSecret").is_none(), "oidc secret stripped");
    assert!(j["enableRequestLogs"].is_boolean());

    // Protected keys are stripped from the body
    let (s, j) = call(
        "PATCH",
        "/api/settings",
        Some(json!({ "customPrompt": "hello", "password": "pwn" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "patch: {j}");
    assert_eq!(j["customPrompt"], Value::String("hello".into()));
    assert!(j.get("password").is_none());

    let (s, j) = call("GET", "/api/settings", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["customPrompt"], Value::String("hello".into()));

    // require-login reflects the defaults
    let (s, j) = call("GET", "/api/settings/require-login", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["requireLogin"], Value::Bool(true));
}

#[tokio::test]
async fn providers_crud() {
    let payload = json!({
        "provider": "openai",
        "apiKey": "sk-test",
        "name": "test-conn",
    });
    let (s, j) = call("POST", "/api/providers", Some(payload)).await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {j}");
    let id = j["connection"]["id"].as_str().expect("id").to_string();
    assert!(
        j["connection"].get("apiKey").is_none(),
        "apiKey stripped from response: {j}"
    );

    // List
    let (s, j) = call("GET", "/api/providers", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["connections"].as_array().expect("conns").iter().any(|c| c["id"].as_str() == Some(&id)));

    // Get single
    let (s, j) = call("GET", &format!("/api/providers/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["connection"]["id"], Value::String(id.clone()));

    // Update
    let (s, j) = call(
        "PUT",
        &format!("/api/providers/{id}"),
        Some(json!({ "name": "renamed-conn", "priority": 5 })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "update: {j}");
    assert_eq!(j["connection"]["name"], Value::String("renamed-conn".into()));
    assert_eq!(j["connection"]["priority"], Value::Number(5.into()));

    // 404
    let (s, _) = call("GET", "/api/providers/does-not-exist", None).await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);

    // Delete
    let (s, _) = call("DELETE", &format!("/api/providers/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn proxy_pools_crud() {
    let (s, j) = call(
        "POST",
        "/api/proxy-pools",
        Some(json!({ "name": "pool-1", "proxyUrl": "http://127.0.0.1:7890" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {j}");
    let id = j["proxyPool"]["id"].as_str().expect("id").to_string();

    // Missing proxyUrl
    let (s, _) = call("POST", "/api/proxy-pools", Some(json!({ "name": "x" }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // Get
    let (s, j) = call("GET", &format!("/api/proxy-pools/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["proxyPool"]["name"], Value::String("pool-1".into()));

    // List with usage
    let (s, j) = call("GET", "/api/proxy-pools?includeUsage=true", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["proxyPools"].is_array());

    // Update
    let (s, j) = call(
        "PUT",
        &format!("/api/proxy-pools/{id}"),
        Some(json!({ "name": "pool-1b", "isActive": false })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["proxyPool"]["name"], Value::String("pool-1b".into()));
    assert_eq!(j["proxyPool"]["isActive"], Value::Bool(false));

    // Delete
    let (s, _) = call("DELETE", &format!("/api/proxy-pools/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);

    // 404 after delete
    let (s, _) = call("GET", &format!("/api/proxy-pools/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pricing_and_models() {
    let (s, j) = call("GET", "/api/pricing", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j.is_object());

    let (s, j) = call("GET", "/api/pricing/defaults", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j.is_object());

    let (s, _) = call("GET", "/api/models", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);

    let (s, j) = call("GET", "/api/models/alias", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["aliases"].is_object());

    // Set + read back + delete an alias
    let (s, _) = call(
        "PUT",
        "/api/models/alias",
        Some(json!({ "model": "openai/gpt-4", "alias": "gpt4" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);

    let (_, j) = call("GET", "/api/models/alias", None).await;
    assert_eq!(j["aliases"]["gpt4"], Value::String("openai/gpt-4".into()));

    let (s, _) = call("DELETE", "/api/models/alias?alias=gpt4", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn provider_nodes_crud() {
    let (s, j) = call(
        "POST",
        "/api/provider-nodes",
        Some(json!({
            "name": "My Node",
            "prefix": "mynode",
            "apiType": "chat",
            "baseUrl": "http://localhost:8080/v1",
        })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {j}");
    let id = j["node"]["id"].as_str().expect("id").to_string();
    assert!(id.starts_with("openai-compatible-chat-"));

    // Bad apiType
    let (s, _) = call(
        "POST",
        "/api/provider-nodes",
        Some(json!({ "name": "X", "prefix": "x", "apiType": "bogus" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // List
    let (s, j) = call("GET", "/api/provider-nodes", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["nodes"].as_array().expect("nodes").iter().any(|n| n["id"].as_str() == Some(&id)));

    // Update
    let (s, j) = call(
        "PUT",
        &format!("/api/provider-nodes/{id}"),
        Some(json!({ "name": "Renamed", "prefix": "renamed", "baseUrl": "http://localhost:9090/v1", "apiType": "responses" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "update: {j}");
    assert_eq!(j["node"]["name"], Value::String("Renamed".into()));

    // Delete
    let (s, _) = call("DELETE", &format!("/api/provider-nodes/{id}"), None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn tags_and_version() {
    let (s, j) = call("GET", "/api/tags", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["models"].is_array());

    let (s, j) = call("GET", "/api/version", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["currentVersion"].is_string());
    assert!(j["hasUpdate"].is_boolean());
}

#[tokio::test]
async fn locale_validation() {
    let (s, _) = call("POST", "/api/locale", Some(json!({ "locale": "not-a-locale" }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    let (s, j) = call("POST", "/api/locale", Some(json!({ "locale": "vi" }))).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["locale"], Value::String("vi".into()));
}
