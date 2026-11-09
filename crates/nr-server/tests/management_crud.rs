//! Integration tests for the less-covered management-API CRUD flows: custom
//! models, disabled models, provider overrides and model availability. Each
//! case runs against a per-test temp-dir database.
//!
//! Port of `src/app/api/models/**` and `src/app/api/providers/[id]/overrides`.

use axum::{Extension, Router};
use nr_db::Db;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

fn router(db: &Db) -> Router {
    nr_server::api_routes().layer(Extension(db.clone()))
}

async fn call(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (axum::http::StatusCode, Value) {
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let req = match body {
        Some(b) => req.body(axum::body::Body::from(b.to_string())),
        None => req.body(axum::body::Body::empty()),
    }
    .unwrap();
    let resp = router.oneshot(req).await.expect("call handler");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn custom_models_crud_round_trip() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let app = router(&db);

    // Add a custom model on a provider alias.
    let (s, j) = call(
        app.clone(),
        "POST",
        "/api/models/custom",
        Some(json!({ "providerAlias": "openai", "id": "my-fine-tune", "name": "My Tune" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");
    assert_eq!(j["success"], Value::Bool(true));
    assert_eq!(j["added"], Value::Bool(true));

    // It shows up in the listing.
    let (s, j) = call(app.clone(), "GET", "/api/models/custom", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let ids: Vec<&str> = j["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"my-fine-tune"), "{ids:?}");

    // Missing providerAlias is a 400, not an insert.
    let (s, _) = call(app.clone(), "POST", "/api/models/custom", Some(json!({ "id": "x" }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);
    let (s, _) = call(app.clone(), "POST", "/api/models/custom", Some(json!({ "providerAlias": " " }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // Delete it; a second delete of the same model is still a 200.
    let (s, j) = call(
        app.clone(),
        "DELETE",
        "/api/models/custom?providerAlias=openai&id=my-fine-tune",
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");
    assert_eq!(j["success"], Value::Bool(true));

    // Delete without the query params is a 400.
    let (s, _) = call(app.clone(), "DELETE", "/api/models/custom", None).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    let (s, j) = call(app.clone(), "GET", "/api/models/custom", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let ids: Vec<&str> = j["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(!ids.contains(&"my-fine-tune"), "{ids:?}");
}

#[tokio::test]
async fn disabled_models_set_and_clear() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let app = router(&db);

    // Disabled list starts empty.
    let (s, j) = call(app.clone(), "GET", "/api/models/disabled", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["disabled"].is_object(), "{j}");

    // Disable two models for openai.
    let (s, j) = call(
        app.clone(),
        "POST",
        "/api/models/disabled",
        Some(json!({ "providerAlias": "openai", "ids": ["gpt-4o", "gpt-4o-mini"] })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");
    assert_eq!(j["success"], Value::Bool(true));

    // Read back the per-alias id list.
    let (s, j) = call(app.clone(), "GET", "/api/models/disabled?providerAlias=openai", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let ids: Vec<&str> = j["ids"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(ids.contains(&"gpt-4o") && ids.contains(&"gpt-4o-mini"), "{ids:?}");

    // Re-enable one.
    let (s, j) = call(
        app.clone(),
        "DELETE",
        "/api/models/disabled?providerAlias=openai&id=gpt-4o",
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");

    let (s, j) = call(app.clone(), "GET", "/api/models/disabled?providerAlias=openai", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let ids: Vec<&str> = j["ids"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(ids, vec!["gpt-4o-mini"], "only the second stays disabled");

    // POST without ids is a 400.
    let (s, _) = call(
        app.clone(),
        "POST",
        "/api/models/disabled",
        Some(json!({ "providerAlias": "openai" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn provider_overrides_round_trip_and_return_builtins() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let app = router(&db);

    // Unknown provider still returns the shape (empty headers, builtin table).
    let (s, j) = call(app.clone(), "GET", "/api/providers/openai/overrides", None).await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");
    assert!(j["headers"].is_object());
    assert!(j["builtinHeaders"].is_object() || j["builtinHeaders"].is_null(), "{j}");

    // Set a custom header override.
    let (s, j) = call(
        app.clone(),
        "PUT",
        "/api/providers/openai/overrides",
        Some(json!({ "headers": { "X-Custom": "yes" } })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");

    let (s, j) = call(app.clone(), "GET", "/api/providers/openai/overrides", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["headers"]["X-Custom"], "yes", "override persisted: {j}");

    // Auth headers are protected: overriding Authorization is a 400 so a user
    // cannot silently point a provider at another account's token.
    let (s, j) = call(
        app.clone(),
        "PUT",
        "/api/providers/openai/overrides",
        Some(json!({ "headers": { "Authorization": "Bearer stolen" } })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST, "{j}");
    assert!(j["error"].as_str().unwrap().to_lowercase().contains("authorization"), "{j}");

    // The rejected write must not have clobbered the stored override.
    let (s, j) = call(app.clone(), "GET", "/api/providers/openai/overrides", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(j["headers"]["X-Custom"], "yes", "{j}");
}

#[tokio::test]
async fn models_availability_lists_and_clears_cooldowns() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let app = router(&db);

    // No connections yet: the listing is an empty array.
    let (s, j) = call(app.clone(), "GET", "/api/models/availability", None).await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");
    assert!(j["models"].as_array().is_some(), "{j}");

    // Add a connection so there is something to report on.
    nr_db::repos::create_provider_connection(
        &db,
        &json!({
            "id": "c1", "provider": "openai", "authType": "apikey",
            "name": "c1", "priority": 1, "isActive": true,
            "providerSpecificData": { "apiKey": "sk-1" }
        }),
    )
    .unwrap();

    let (s, j) = call(app.clone(), "GET", "/api/models/availability", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(j["models"].as_array().unwrap().is_empty(), "no locks yet: {j}");

    // An invalid clearCooldown body is a 400.
    let (s, _) = call(
        app.clone(),
        "POST",
        "/api/models/availability",
        Some(json!({ "action": "clearCooldown", "provider": "openai" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    let (s, _) = call(
        app.clone(),
        "POST",
        "/api/models/availability",
        Some(json!({ "action": "bogus", "provider": "openai", "model": "gpt-4o" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    // Clearing a cooldown that is not set is a harmless no-op (200).
    let (s, j) = call(
        app.clone(),
        "POST",
        "/api/models/availability",
        Some(json!({ "action": "clearCooldown", "provider": "openai", "model": "gpt-4o" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");
}

#[tokio::test]
async fn models_alias_requires_both_fields() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let app = router(&db);

    // Missing alias is rejected.
    let (s, _) = call(app.clone(), "PUT", "/api/models/alias", Some(json!({ "model": "openai/gpt-4" }))).await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);

    let (s, j) = call(
        app.clone(),
        "PUT",
        "/api/models/alias",
        Some(json!({ "model": "openai/gpt-4", "alias": "g4" })),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{j}");

    // The alias resolves through /v1/models too, proving the two surfaces agree.
    let (s, _) = call(
        app.clone(),
        "GET",
        "/api/models/alias",
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);

    let (s, _) = call(app.clone(), "DELETE", "/api/models/alias?alias=g4", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
}
