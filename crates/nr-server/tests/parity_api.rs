//! HTTP parity tests for /api management routes against nr_server::app().
//!
//! Verifies response status codes, JSON payload shapes, headers, and CRUD workflows
//! against upstream JS specs in `../9router/src/app/api/**`.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tower::ServiceExt;

static TEST_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
static SUITE_LOCK: Mutex<()> = Mutex::const_new(());

fn init_env() -> &'static std::path::Path {
    TEST_DIR.get_or_init(|| {
        let dir = TempDir::new().expect("create temp dir");
        let path = dir.path().to_path_buf();
        let _ = dir.keep();
        std::env::set_var("DATA_DIR", &path);
        let _ = nr_db::db().expect("open default test db");
        path
    })
}

fn app() -> axum::Router {
    let _ = init_env();
    nr_server::app()
}

async fn call(method: &str, uri: &str, body: Option<Value>) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    builder = builder.header(CONTENT_TYPE, "application/json");

    let req = match body {
        Some(b) => builder.body(Body::from(serde_json::to_vec(&b).unwrap())),
        None => builder.body(Body::empty()),
    }
    .unwrap();

    let resp = app().oneshot(req).await.expect("oneshot call failed");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .expect("read response body");
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, val)
}

// ─── 1. Settings ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_settings_get_and_patch() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/settings
    let (status, headers, val) = call("GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert!(val.is_object(), "settings body must be an object: {val}");

    // Sensitive keys must be stripped
    assert!(val.get("password").is_none() || val["password"].is_null());
    assert!(val.get("oidcClientSecret").is_none() || val["oidcClientSecret"].is_null());

    // Safe computed flags
    assert!(val["oidcConfigured"].is_boolean());
    assert!(val["enableRequestLogs"].is_boolean());
    assert!(val["enableTranslator"].is_boolean());
    assert!(val["hasPassword"].is_boolean());

    // PATCH /api/settings
    let (patch_status, patch_headers, patch_val) = call(
        "PATCH",
        "/api/settings",
        Some(json!({
            "requireApiKey": false,
            "modelFallback": true
        })),
    )
    .await;
    assert_eq!(patch_status, StatusCode::OK);
    assert_eq!(
        patch_headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(patch_val["requireApiKey"], false);
    assert_eq!(patch_val["modelFallback"], true);
    assert!(patch_val.get("password").is_none() || patch_val["password"].is_null());

    // Verify persistence via GET
    let (get_status, _, get_val) = call("GET", "/api/settings", None).await;
    assert_eq!(get_status, StatusCode::OK);
    assert_eq!(get_val["requireApiKey"], false);
    assert_eq!(get_val["modelFallback"], true);

    // Protected key stripping: passing direct password must not set stored hash as-is
    let (strip_status, _, strip_val) = call(
        "PATCH",
        "/api/settings",
        Some(json!({ "password": "unhashed_direct_password" })),
    )
    .await;
    assert_eq!(strip_status, StatusCode::OK);
    assert!(strip_val.get("password").is_none() || strip_val["password"].is_null());
}

// ─── 2. Providers ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_providers_crud() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/providers
    let (status, _, val) = call("GET", "/api/providers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val["connections"].is_array(), "expected connections array");

    // Validation: missing provider
    let (bad_status, _, bad_val) = call("POST", "/api/providers", Some(json!({ "name": "Missing Provider" }))).await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_val["error"], "Invalid provider");

    // Validation: missing apiKey for apikey provider
    let (no_key_status, _, no_key_val) = call(
        "POST",
        "/api/providers",
        Some(json!({ "provider": "openai", "name": "No Key" })),
    )
    .await;
    assert_eq!(no_key_status, StatusCode::BAD_REQUEST);
    assert_eq!(no_key_val["error"], "API Key is required");

    // Create provider connection
    let (create_status, _, create_val) = call(
        "POST",
        "/api/providers",
        Some(json!({
            "provider": "openai",
            "name": "Parity OpenAI Test",
            "apiKey": "sk-parity-key-1234567890abcdef"
        })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    let conn = &create_val["connection"];
    assert!(conn.is_object(), "expected connection object");
    let conn_id = conn["id"].as_str().expect("connection id").to_string();
    assert_eq!(conn["provider"], "openai");
    assert_eq!(conn["name"], "Parity OpenAI Test");
    assert_eq!(conn["authType"], "apikey");

    // Verify in GET /api/providers
    let (list_status, _, list_val) = call("GET", "/api/providers", None).await;
    assert_eq!(list_status, StatusCode::OK);
    let conns = list_val["connections"].as_array().expect("connections");
    assert!(conns.iter().any(|c| c["id"] == conn_id));

    // DELETE /api/providers/{id}
    let (del_status, _, del_val) = call("DELETE", &format!("/api/providers/{conn_id}"), None).await;
    assert_eq!(del_status, StatusCode::OK);
    assert_eq!(del_val["message"], "Connection deleted successfully");

    // Verify deleted
    let (after_status, _, after_val) = call("GET", "/api/providers", None).await;
    assert_eq!(after_status, StatusCode::OK);
    let conns_after = after_val["connections"].as_array().expect("connections");
    assert!(!conns_after.iter().any(|c| c["id"] == conn_id));

    // DELETE non-existent provider -> 404
    let (nf_status, _, nf_val) = call("DELETE", "/api/providers/non-existent-id-9999", None).await;
    assert_eq!(nf_status, StatusCode::NOT_FOUND);
    assert_eq!(nf_val["error"], "Connection not found");
}

// ─── 3. Provider Nodes ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_provider_nodes_crud_and_validate() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/provider-nodes
    let (status, _, val) = call("GET", "/api/provider-nodes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val["nodes"].is_array(), "expected nodes array");

    // POST validation: missing name
    let (bad1_status, _, bad1_val) = call("POST", "/api/provider-nodes", Some(json!({ "prefix": "foo" }))).await;
    assert_eq!(bad1_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad1_val["error"], "Name is required");

    // POST validation: missing prefix
    let (bad2_status, _, bad2_val) = call("POST", "/api/provider-nodes", Some(json!({ "name": "foo" }))).await;
    assert_eq!(bad2_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad2_val["error"], "Prefix is required");

    // POST validation: invalid apiType for openai-compatible
    let (bad3_status, _, bad3_val) = call(
        "POST",
        "/api/provider-nodes",
        Some(json!({
            "name": "foo",
            "prefix": "foo",
            "type": "openai-compatible",
            "apiType": "invalid-type"
        })),
    )
    .await;
    assert_eq!(bad3_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad3_val["error"], "Invalid OpenAI compatible API type");

    // Create provider node
    let (create_status, _, create_val) = call(
        "POST",
        "/api/provider-nodes",
        Some(json!({
            "name": "Parity Node 1",
            "prefix": "parity-node",
            "type": "openai-compatible",
            "apiType": "chat",
            "baseUrl": "https://api.openai.com/v1"
        })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    let node = &create_val["node"];
    assert!(node.is_object(), "expected node object");
    let node_id = node["id"].as_str().expect("node id").to_string();
    assert_eq!(node["name"], "Parity Node 1");
    assert_eq!(node["prefix"], "parity-node");
    assert_eq!(node["apiType"], "chat");

    // Verify in GET /api/provider-nodes
    let (list_status, _, list_val) = call("GET", "/api/provider-nodes", None).await;
    assert_eq!(list_status, StatusCode::OK);
    let nodes = list_val["nodes"].as_array().expect("nodes");
    assert!(nodes.iter().any(|n| n["id"] == node_id));

    // POST /api/provider-nodes/validate: missing fields
    let (val_bad_status, _, val_bad_val) = call("POST", "/api/provider-nodes/validate", Some(json!({}))).await;
    assert_eq!(val_bad_status, StatusCode::BAD_REQUEST);
    assert_eq!(val_bad_val["error"], "Base URL and API key required");

    // POST /api/provider-nodes/validate: custom-embedding missing modelId -> 200 with valid: false
    let (val_embed_status, _, val_embed_res) = call(
        "POST",
        "/api/provider-nodes/validate",
        Some(json!({
            "baseUrl": "http://127.0.0.1:59999/v1",
            "apiKey": "sk-dummy-key",
            "type": "custom-embedding"
        })),
    )
    .await;
    assert_eq!(val_embed_status, StatusCode::OK);
    assert_eq!(val_embed_res["valid"], false);
    assert_eq!(
        val_embed_res["error"],
        "Model ID required for embedding validation"
    );

    // POST /api/provider-nodes/validate: openai-compatible returns boolean valid flag
    let (val_status, _, val_res) = call(
        "POST",
        "/api/provider-nodes/validate",
        Some(json!({
            "baseUrl": "http://127.0.0.1:59999/v1",
            "apiKey": "sk-dummy-key"
        })),
    )
    .await;
    assert_eq!(val_status, StatusCode::OK);
    assert!(val_res["valid"].is_boolean());
}

// ─── 4. Keys ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_keys_crud() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/keys
    let (status, _, val) = call("GET", "/api/keys", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val["keys"].is_array(), "expected keys array");

    // POST /api/keys validation: missing name
    let (bad_status, _, _) = call("POST", "/api/keys", Some(json!({}))).await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);

    // POST /api/keys creation
    let (create_status, _, create_val) = call(
        "POST",
        "/api/keys",
        Some(json!({ "name": "parity-management-key" })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    assert!(create_val.is_object());
    let key_id = create_val["id"].as_str().expect("key id").to_string();
    let secret = create_val["key"].as_str().expect("key secret");
    assert!(!secret.is_empty(), "key secret must not be empty");
    assert_eq!(create_val["name"], "parity-management-key");

    // Verify listed in GET /api/keys
    let (list_status, _, list_val) = call("GET", "/api/keys", None).await;
    assert_eq!(list_status, StatusCode::OK);
    let keys = list_val["keys"].as_array().expect("keys");
    let created_key = keys.iter().find(|k| k["id"] == key_id).expect("found key");
    assert_eq!(created_key["isActive"], true);

    // DELETE /api/keys/{id}
    let (del_status, _, del_val) = call("DELETE", &format!("/api/keys/{key_id}"), None).await;
    assert_eq!(del_status, StatusCode::OK);
    assert_eq!(del_val["message"], "Key deleted successfully");

    // Verify deleted
    let (after_status, _, after_val) = call("GET", "/api/keys", None).await;
    assert_eq!(after_status, StatusCode::OK);
    let keys_after = after_val["keys"].as_array().expect("keys");
    assert!(!keys_after.iter().any(|k| k["id"] == key_id));

    // DELETE non-existent key -> 404
    let (nf_status, _, nf_val) = call("DELETE", "/api/keys/non-existent-key-id", None).await;
    assert_eq!(nf_status, StatusCode::NOT_FOUND);
    assert_eq!(nf_val["error"], "Key not found");
}

// ─── 5. Combos ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_combos_crud() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/combos
    let (status, _, val) = call("GET", "/api/combos", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val["combos"].is_array(), "expected combos array");

    // POST /api/combos validation: missing name
    let (bad1_status, _, bad1_val) = call("POST", "/api/combos", Some(json!({}))).await;
    assert_eq!(bad1_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad1_val["error"], "Name is required");

    // POST /api/combos validation: invalid name format (contains spaces)
    let (bad2_status, _, bad2_val) = call(
        "POST",
        "/api/combos",
        Some(json!({ "name": "bad combo name with spaces" })),
    )
    .await;
    assert_eq!(bad2_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        bad2_val["error"],
        "Name can only contain letters, numbers, -, _ and ."
    );

    // POST /api/combos creation
    let (create_status, _, create_val) = call(
        "POST",
        "/api/combos",
        Some(json!({
            "name": "parity_combo_test",
            "models": ["openai/gpt-4o", "anthropic/claude-3-5-sonnet"]
        })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    assert!(create_val.is_object());
    let combo_id = create_val["id"].as_str().expect("combo id").to_string();
    assert_eq!(create_val["name"], "parity_combo_test");
    assert_eq!(
        create_val["models"],
        json!(["openai/gpt-4o", "anthropic/claude-3-5-sonnet"])
    );

    // Duplicate combo name -> 400
    let (dup_status, _, dup_val) = call(
        "POST",
        "/api/combos",
        Some(json!({ "name": "parity_combo_test" })),
    )
    .await;
    assert_eq!(dup_status, StatusCode::BAD_REQUEST);
    assert_eq!(dup_val["error"], "Combo name already exists");

    // Verify listed in GET /api/combos
    let (list_status, _, list_val) = call("GET", "/api/combos", None).await;
    assert_eq!(list_status, StatusCode::OK);
    let combos = list_val["combos"].as_array().expect("combos");
    assert!(combos.iter().any(|c| c["id"] == combo_id));

    // DELETE /api/combos/{id}
    let (del_status, _, del_val) = call("DELETE", &format!("/api/combos/{combo_id}"), None).await;
    assert_eq!(del_status, StatusCode::OK);
    assert_eq!(del_val["success"], true);

    // Verify deleted
    let (after_status, _, after_val) = call("GET", "/api/combos", None).await;
    assert_eq!(after_status, StatusCode::OK);
    let combos_after = after_val["combos"].as_array().expect("combos");
    assert!(!combos_after.iter().any(|c| c["id"] == combo_id));

    // DELETE non-existent combo -> 404
    let (nf_status, _, nf_val) = call("DELETE", "/api/combos/non-existent-combo-id", None).await;
    assert_eq!(nf_status, StatusCode::NOT_FOUND);
    assert_eq!(nf_val["error"], "Combo not found");
}

// ─── 6. Models ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_models_endpoints() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/models
    let (status, _, val) = call("GET", "/api/models", None).await;
    assert_eq!(status, StatusCode::OK);
    let models = val["models"].as_array().expect("models array");
    assert!(!models.is_empty(), "models array should not be empty");
    let first = &models[0];
    assert!(first.get("fullModel").is_some());
    assert!(first.get("routedModel").is_some());
    assert!(first.get("alias").is_some());
    assert!(first.get("caps").is_some());

    // GET /api/models/alias
    let (alias_status, _, alias_val) = call("GET", "/api/models/alias", None).await;
    assert_eq!(alias_status, StatusCode::OK);
    assert!(alias_val["aliases"].is_object(), "expected aliases object");

    // GET /api/models/availability
    let (avail_status, _, avail_val) = call("GET", "/api/models/availability", None).await;
    assert_eq!(avail_status, StatusCode::OK);
    assert!(avail_val["models"].is_array(), "expected availability models array");
    assert!(avail_val["unavailableCount"].is_number(), "expected unavailableCount");

    // GET /api/models/custom
    let (custom_status, _, custom_val) = call("GET", "/api/models/custom", None).await;
    assert_eq!(custom_status, StatusCode::OK);
    assert!(custom_val["models"].is_array(), "expected custom models array");

    // GET /api/models/disabled (all)
    let (dis_status, _, dis_val) = call("GET", "/api/models/disabled", None).await;
    assert_eq!(dis_status, StatusCode::OK);
    assert!(dis_val["disabled"].is_object(), "expected disabled map");

    // GET /api/models/disabled?providerAlias=openai
    let (dis_p_status, _, dis_p_val) = call("GET", "/api/models/disabled?providerAlias=openai", None).await;
    assert_eq!(dis_p_status, StatusCode::OK);
    assert!(dis_p_val["ids"].is_array(), "expected ids array for provider");
}

// ─── 7. Pricing ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_pricing_endpoints() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/pricing
    let (status, _, val) = call("GET", "/api/pricing", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val.is_object(), "pricing must be an object");

    // GET /api/pricing/defaults
    let (def_status, _, def_val) = call("GET", "/api/pricing/defaults", None).await;
    assert_eq!(def_status, StatusCode::OK);
    assert!(def_val.is_object(), "pricing defaults must be an object");
    assert!(def_val.as_object().unwrap().len() > 0, "defaults should not be empty");
}

// ─── 8. Proxy Pools ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_proxy_pools_crud() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/proxy-pools
    let (status, _, val) = call("GET", "/api/proxy-pools", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val["proxyPools"].is_array(), "expected proxyPools array");

    // POST /api/proxy-pools validation: missing name
    let (bad1_status, _, bad1_val) = call(
        "POST",
        "/api/proxy-pools",
        Some(json!({ "proxyUrl": "http://127.0.0.1:8080" })),
    )
    .await;
    assert_eq!(bad1_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad1_val["error"], "Name is required");

    // POST /api/proxy-pools validation: missing proxyUrl
    let (bad2_status, _, bad2_val) = call(
        "POST",
        "/api/proxy-pools",
        Some(json!({ "name": "Pool Missing URL" })),
    )
    .await;
    assert_eq!(bad2_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad2_val["error"], "Proxy URL is required");

    // POST /api/proxy-pools creation
    let (create_status, _, create_val) = call(
        "POST",
        "/api/proxy-pools",
        Some(json!({
            "name": "Parity Test Pool",
            "proxyUrl": "http://127.0.0.1:8080"
        })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    let pool = &create_val["proxyPool"];
    assert!(pool.is_object(), "expected proxyPool object");
    let pool_id = pool["id"].as_str().expect("pool id").to_string();
    assert_eq!(pool["name"], "Parity Test Pool");
    assert_eq!(pool["proxyUrl"], "http://127.0.0.1:8080");

    // GET /api/proxy-pools?includeUsage=true
    let (usage_status, _, usage_val) = call("GET", "/api/proxy-pools?includeUsage=true", None).await;
    assert_eq!(usage_status, StatusCode::OK);
    let pools = usage_val["proxyPools"].as_array().expect("proxyPools");
    let found = pools.iter().find(|p| p["id"] == pool_id);
    assert!(found.is_some(), "created pool should be listed");
    assert!(found.unwrap().get("boundConnectionCount").is_some());
}

// ─── 9. Usage ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_usage_endpoints() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/usage/stats: invalid period
    let (bad_stats, _, bad_val) = call("GET", "/api/usage/stats?period=invalid_period", None).await;
    assert_eq!(bad_stats, StatusCode::BAD_REQUEST);
    assert_eq!(bad_val["error"], "Invalid period");

    // GET /api/usage/stats: valid period
    let (stats_status, _, stats_val) = call("GET", "/api/usage/stats?period=7d", None).await;
    assert_eq!(stats_status, StatusCode::OK);
    assert!(stats_val.is_object(), "stats should be an object");

    // GET /api/usage/stats: default (no query param)
    let (def_stats_status, _, def_stats_val) = call("GET", "/api/usage/stats", None).await;
    assert_eq!(def_stats_status, StatusCode::OK);
    assert!(def_stats_val.is_object());

    // GET /api/usage/history
    let (hist_status, _, hist_val) = call("GET", "/api/usage/history", None).await;
    assert_eq!(hist_status, StatusCode::OK);
    assert!(hist_val.is_object(), "history should be an object");

    // GET /api/usage/chart: invalid period
    let (bad_chart, _, chart_err) = call("GET", "/api/usage/chart?period=bad_period", None).await;
    assert_eq!(bad_chart, StatusCode::BAD_REQUEST);
    assert_eq!(chart_err["error"], "Invalid period");

    // GET /api/usage/chart: valid period
    let (chart_status, _, chart_val) = call("GET", "/api/usage/chart?period=7d", None).await;
    assert_eq!(chart_status, StatusCode::OK);
    assert!(chart_val.is_object() || chart_val.is_array());
}

// ─── 10. Tunnel Status ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_tunnel_status() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/tunnel/status
    let (status, _, val) = call("GET", "/api/tunnel/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val.is_object(), "tunnel status must be an object");
    assert!(val.get("tunnel").is_some(), "missing tunnel field: {val}");
    assert!(val.get("tailscale").is_some(), "missing tailscale field: {val}");
    assert!(val.get("download").is_some(), "missing download field: {val}");
}

// ─── 11. Headroom Status ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_headroom_status() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/headroom/status
    let (status, _, val) = call("GET", "/api/headroom/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val.is_object(), "headroom status must be an object");
    assert!(val["installed"].is_boolean());
    assert!(val["running"].is_boolean());
    assert!(val["url"].is_string());
    assert!(val["canStart"].is_boolean());
}

// ─── 12. Version ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_version() {
    let _guard = SUITE_LOCK.lock().await;

    // GET /api/version
    let (status, _, val) = call("GET", "/api/version", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(val.is_object(), "version must be an object");
    assert_eq!(
        val["currentVersion"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "currentVersion must match CARGO_PKG_VERSION"
    );
    assert!(val["hasUpdate"].is_boolean());
}
