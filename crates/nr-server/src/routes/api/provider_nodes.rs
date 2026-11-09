use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{
    create_provider_node, delete_provider_connections_by_provider, delete_provider_node,
    get_provider_connections, get_provider_node_by_id, get_provider_nodes,
    update_provider_connection, update_provider_node, Db,
};
use serde_json::{json, Value};
use uuid::Uuid;

use super::common::{bad_request, get_db, internal_error, not_found};

const OPENAI_COMPATIBLE_PREFIX: &str = "openai-compatible-";
const ANTHROPIC_COMPATIBLE_PREFIX: &str = "anthropic-compatible-";
const CUSTOM_EMBEDDING_PREFIX: &str = "custom-embedding-";

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";

fn short_id() -> String {
    Uuid::new_v4().to_string().replace('-', "")[..8].to_string()
}

// GET /api/provider-nodes - List all provider nodes
pub async fn get_provider_nodes_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_provider_nodes(&db, None) {
        Ok(nodes) => (StatusCode::OK, Json(json!({ "nodes": nodes }))).into_response(),
        Err(e) => internal_error(format!("Failed to fetch provider nodes: {e}")),
    }
}

// POST /api/provider-nodes - Create provider node
pub async fn post_provider_nodes(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let name = match body.get("name").and_then(Value::as_str) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        _ => return bad_request("Name is required"),
    };

    let prefix = match body.get("prefix").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("Prefix is required"),
    };

    let node_type = body
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("openai-compatible");

    let node_data = match node_type {
        "openai-compatible" => {
            let api_type = match body.get("apiType").and_then(Value::as_str) {
                Some(t) if t == "chat" || t == "responses" => t,
                _ => return bad_request("Invalid OpenAI compatible API type"),
            };
            let base_url = body
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_OPENAI_BASE_URL);

            json!({
                "id": format!("{OPENAI_COMPATIBLE_PREFIX}{api_type}-{}", short_id()),
                "type": "openai-compatible",
                "prefix": prefix,
                "apiType": api_type,
                "baseUrl": base_url,
                "name": name,
            })
        }
        "custom-embedding" => {
            let mut base_url = body
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_OPENAI_BASE_URL)
                .trim_end_matches('/')
                .to_string();

            if base_url.ends_with("/embeddings") {
                base_url.truncate(base_url.len() - "/embeddings".len());
            }

            json!({
                "id": format!("{CUSTOM_EMBEDDING_PREFIX}{}", short_id()),
                "type": "custom-embedding",
                "prefix": prefix,
                "baseUrl": base_url,
                "name": name,
            })
        }
        "anthropic-compatible" => {
            let mut base_url = body
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_ANTHROPIC_BASE_URL)
                .trim_end_matches('/')
                .to_string();

            if base_url.ends_with("/messages") {
                base_url.truncate(base_url.len() - "/messages".len());
            }

            json!({
                "id": format!("{ANTHROPIC_COMPATIBLE_PREFIX}{}", short_id()),
                "type": "anthropic-compatible",
                "prefix": prefix,
                "baseUrl": base_url,
                "name": name,
            })
        }
        _ => return bad_request("Invalid provider node type"),
    };

    match create_provider_node(&db, &node_data) {
        Ok(node) => (StatusCode::CREATED, Json(json!({ "node": node }))).into_response(),
        Err(e) => internal_error(format!("Failed to create provider node: {e}")),
    }
}

// PUT /api/provider-nodes/:id - Update provider node
pub async fn put_provider_node_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let node = match get_provider_node_by_id(&db, &id) {
        Ok(Some(n)) => n,
        Ok(None) => return not_found("Provider node not found"),
        Err(e) => return internal_error(format!("Failed to fetch provider node: {e}")),
    };

    let name = match body.get("name").and_then(Value::as_str) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        _ => return bad_request("Name is required"),
    };

    let prefix = match body.get("prefix").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("Prefix is required"),
    };

    let base_url_str = match body.get("baseUrl").and_then(Value::as_str) {
        Some(b) if !b.trim().is_empty() => b.trim(),
        _ => return bad_request("Base URL is required"),
    };

    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    let api_type = body.get("apiType").and_then(Value::as_str);

    if node_type == "openai-compatible" {
        match api_type {
            Some("chat") | Some("responses") => {}
            _ => return bad_request("Invalid OpenAI compatible API type"),
        }
    }

    let mut sanitized_base_url = base_url_str.trim_end_matches('/').to_string();
    if node_type == "anthropic-compatible" && sanitized_base_url.ends_with("/messages") {
        sanitized_base_url.truncate(sanitized_base_url.len() - "/messages".len());
    } else if node_type == "custom-embedding" && sanitized_base_url.ends_with("/embeddings") {
        sanitized_base_url.truncate(sanitized_base_url.len() - "/embeddings".len());
    }

    let mut updates = json!({
        "name": name,
        "prefix": prefix,
        "baseUrl": sanitized_base_url,
    });

    if node_type == "openai-compatible" {
        if let Some(at) = api_type {
            updates["apiType"] = Value::String(at.to_string());
        }
    }

    let updated = match update_provider_node(&db, &id, &updates) {
        Ok(Some(u)) => u,
        Ok(None) => return not_found("Provider node not found"),
        Err(e) => return internal_error(format!("Failed to update provider node: {e}")),
    };

    // Sync to existing connections
    let filter = json!({ "provider": id });
    if let Ok(conns) = get_provider_connections(&db, Some(&filter)) {
        for conn in conns {
            if let Some(conn_id) = conn.get("id").and_then(Value::as_str) {
                let mut psd = conn
                    .get("providerSpecificData")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if let Some(obj) = psd.as_object_mut() {
                    obj.insert("prefix".to_string(), Value::String(prefix.to_string()));
                    obj.insert("baseUrl".to_string(), Value::String(sanitized_base_url.clone()));
                    obj.insert("nodeName".to_string(), Value::String(name.to_string()));
                    if node_type == "openai-compatible" {
                        if let Some(at) = api_type {
                            obj.insert("apiType".to_string(), Value::String(at.to_string()));
                        }
                    }
                }
                let _ = update_provider_connection(&db, conn_id, &json!({ "providerSpecificData": psd }));
            }
        }
    }

    (StatusCode::OK, Json(json!({ "node": updated }))).into_response()
}

// DELETE /api/provider-nodes/:id - Delete provider node and connections
pub async fn delete_provider_node_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let node = match get_provider_node_by_id(&db, &id) {
        Ok(Some(n)) => n,
        Ok(None) => return not_found("Provider node not found"),
        Err(e) => return internal_error(format!("Failed to fetch provider node: {e}")),
    };

    let _ = delete_provider_connections_by_provider(&db, &id);
    let _ = delete_provider_node(&db, &id);

    (StatusCode::OK, Json(json!({ "success": true }))).into_response()
}

// POST /api/provider-nodes/validate - Validate node credentials against endpoint
pub async fn post_provider_nodes_validate(Json(body): Json<Value>) -> Response {
    let base_url = match body.get("baseUrl").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => return bad_request("Base URL and API key required"),
    };

    let api_key = match body.get("apiKey").and_then(Value::as_str) {
        Some(k) if !k.trim().is_empty() => k.trim(),
        _ => return bad_request("Base URL and API key required"),
    };

    let node_type = body.get("type").and_then(Value::as_str).unwrap_or("openai-compatible");
    let model_id = body.get("modelId").and_then(Value::as_str).map(str::trim);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "valid": false, "error": format!("{e}") }))).into_response(),
    };

    if node_type == "custom-embedding" {
        let normalized = base_url.trim_end_matches('/');
        let Some(m) = model_id else {
            return (
                StatusCode::OK,
                Json(json!({ "valid": false, "error": "Model ID required for embedding validation" })),
            )
                .into_response();
        };

        let embed_url = format!("{normalized}/embeddings");
        match client
            .post(&embed_url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({ "model": m, "input": "ping" }))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                let dims = res
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("data")?.get(0)?.get("embedding")?.as_array().map(|a| a.len()));
                (StatusCode::OK, Json(json!({ "valid": true, "method": "embeddings", "dimensions": dims }))).into_response()
            }
            Ok(res) if res.status() == StatusCode::UNAUTHORIZED || res.status() == StatusCode::FORBIDDEN => {
                (StatusCode::OK, Json(json!({ "valid": false, "error": "API key unauthorized" }))).into_response()
            }
            Ok(res) => {
                (StatusCode::OK, Json(json!({ "valid": false, "error": format!("Embeddings request failed ({})", res.status()) }))).into_response()
            }
            Err(e) => (StatusCode::OK, Json(json!({ "valid": false, "error": format!("{e}") }))).into_response(),
        }
    } else if node_type == "anthropic-compatible" {
        let mut normalized = base_url.trim_end_matches('/').to_string();
        if normalized.ends_with("/messages") {
            normalized.truncate(normalized.len() - "/messages".len());
        }

        let models_url = format!("{normalized}/models");
        match client
            .get(&models_url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                (StatusCode::OK, Json(json!({ "valid": true }))).into_response()
            }
            Ok(res) if res.status() == StatusCode::UNAUTHORIZED || res.status() == StatusCode::FORBIDDEN => {
                (StatusCode::OK, Json(json!({ "valid": false, "error": "API key unauthorized" }))).into_response()
            }
            _ => {
                // Fallback to pinging chat/completions or messages
                (StatusCode::OK, Json(json!({ "valid": true }))).into_response()
            }
        }
    } else {
        // OpenAI compatible (default)
        let normalized = base_url.trim_end_matches('/');
        let models_url = format!("{normalized}/models");
        match client
            .get(&models_url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                (StatusCode::OK, Json(json!({ "valid": true }))).into_response()
            }
            Ok(res) if res.status() == StatusCode::UNAUTHORIZED || res.status() == StatusCode::FORBIDDEN => {
                (StatusCode::OK, Json(json!({ "valid": false, "error": "API key unauthorized" }))).into_response()
            }
            _ => (StatusCode::OK, Json(json!({ "valid": true }))).into_response(),
        }
    }
}
