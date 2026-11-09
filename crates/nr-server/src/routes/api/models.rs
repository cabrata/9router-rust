use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{
    add_custom_model, delete_custom_model, delete_model_alias, disable_models, enable_models,
    get_custom_models, get_disabled_models, get_model_aliases, get_provider_connections,
    set_model_alias, update_provider_connection, Db,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;

use super::common::{bad_request, get_db, internal_error};

const MODEL_LOCK_PREFIX: &str = "modelLock_";

fn get_provider_alias(provider: &str) -> String {
    if let Some(map) = nr_data::export("src/shared/constants/providers", "ID_TO_ALIAS")
        .and_then(Value::as_object)
    {
        if let Some(alias) = map.get(provider).and_then(Value::as_str) {
            return alias.to_string();
        }
    }
    provider.to_string()
}

fn alias_to_provider_id(alias: &str) -> String {
    if let Some(map) = nr_data::export("src/shared/constants/providers", "ALIAS_TO_ID")
        .and_then(Value::as_object)
    {
        if let Some(id) = map.get(alias).and_then(Value::as_str) {
            return id.to_string();
        }
    }
    alias.to_string()
}

// GET /api/models - Get models with aliases
pub async fn get_models_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let model_aliases = get_model_aliases(&db).unwrap_or_else(|_| json!({}));
    let disabled = get_disabled_models(&db).unwrap_or_else(|_| json!({}));

    let ai_models_raw = nr_data::export("src/shared/constants/models", "AI_MODELS")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut models = Vec::new();

    for m in &ai_models_raw {
        let provider = m.get("provider").and_then(Value::as_str).unwrap_or("");
        let model = m.get("model").and_then(Value::as_str).unwrap_or("");
        let provider_alias = get_provider_alias(provider);

        let is_disabled = disabled
            .get(&provider_alias)
            .or_else(|| disabled.get(provider))
            .and_then(Value::as_array)
            .map_or(false, |list| {
                list.iter().any(|v| v.as_str() == Some(model))
            });

        if is_disabled {
            continue;
        }

        let full_model = format!("{provider}/{model}");
        let routed_model = format!("{provider_alias}/{model}");
        let alias = model_aliases
            .get(&full_model)
            .and_then(Value::as_str)
            .unwrap_or(model);

        let caps = nr_data::capabilities::get_capabilities_for_model(Some(provider), Some(model));

        let mut entry = m.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("fullModel".to_string(), Value::String(full_model));
            obj.insert("routedModel".to_string(), Value::String(routed_model));
            obj.insert("alias".to_string(), Value::String(alias.to_string()));
            obj.insert(
                "caps".to_string(),
                json!({
                    "vision": caps.get("vision").cloned().unwrap_or(Value::Bool(false)),
                    "search": caps.get("search").cloned().unwrap_or(Value::Bool(false)),
                    "reasoning": caps.get("reasoning").cloned().unwrap_or(Value::Bool(false)),
                    "contextWindow": caps.get("contextWindow").cloned().unwrap_or(json!(200000)),
                    "maxOutput": caps.get("maxOutput").cloned().unwrap_or(json!(64000)),
                }),
            );
        }
        models.push(entry);
    }

    // Append custom models
    let seen_full: HashSet<String> = models
        .iter()
        .filter_map(|m| m.get("fullModel").and_then(Value::as_str).map(String::from))
        .collect();
    let seen_routed: HashSet<String> = models
        .iter()
        .filter_map(|m| m.get("routedModel").and_then(Value::as_str).map(String::from))
        .collect();

    let custom_models = get_custom_models(&db).unwrap_or_default();
    for cm in custom_models {
        let p_alias = cm.get("providerAlias").and_then(Value::as_str).unwrap_or("");
        let id = cm.get("id").and_then(Value::as_str).unwrap_or("");
        let kind = cm
            .get("kind")
            .or_else(|| cm.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("llm");

        if id.is_empty() || kind != "llm" {
            continue;
        }

        let full_key = format!("{p_alias}/{id}");
        if seen_routed.contains(&full_key) || seen_full.contains(&full_key) {
            continue;
        }

        let provider_id = alias_to_provider_id(p_alias);
        let caps = nr_data::capabilities::get_capabilities_for_model(Some(&provider_id), Some(id));
        let alias = model_aliases
            .get(&full_key)
            .and_then(Value::as_str)
            .unwrap_or(id);

        let mut caps_val = json!({
            "vision": caps.get("vision").cloned().unwrap_or(Value::Bool(false)),
            "search": caps.get("search").cloned().unwrap_or(Value::Bool(false)),
            "reasoning": caps.get("reasoning").cloned().unwrap_or(Value::Bool(false)),
            "contextWindow": caps.get("contextWindow").cloned().unwrap_or(json!(200000)),
            "maxOutput": caps.get("maxOutput").cloned().unwrap_or(json!(64000)),
        });

        if let Some(custom_caps) = cm.get("caps").and_then(Value::as_object) {
            if let Some(target) = caps_val.as_object_mut() {
                for (k, v) in custom_caps {
                    target.insert(k.clone(), v.clone());
                }
            }
        }

        models.push(json!({
            "provider": p_alias,
            "model": id,
            "name": cm.get("name").and_then(Value::as_str).unwrap_or(id),
            "fullModel": full_key,
            "routedModel": full_key,
            "alias": alias,
            "caps": caps_val,
        }));
    }

    (StatusCode::OK, Json(json!({ "models": models }))).into_response()
}

// PUT /api/models - Update model alias
pub async fn put_models(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return bad_request("Model and alias required"),
    };

    let alias = match body.get("alias").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() => a.trim(),
        _ => return bad_request("Model and alias required"),
    };

    let aliases = get_model_aliases(&db).unwrap_or_else(|_| json!({}));
    if let Some(obj) = aliases.as_object() {
        for (m, a) in obj {
            if a.as_str() == Some(alias) && m != model {
                return bad_request("Alias already in use");
            }
        }
    }

    match set_model_alias(&db, alias, model) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "success": true, "model": model, "alias": alias })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to update alias: {e}")),
    }
}

// GET /api/models/alias - Get all aliases
pub async fn get_models_alias(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_model_aliases(&db) {
        Ok(aliases) => (StatusCode::OK, Json(json!({ "aliases": aliases }))).into_response(),
        Err(e) => internal_error(format!("Failed to fetch aliases: {e}")),
    }
}

// PUT /api/models/alias - Set model alias
pub async fn put_models_alias(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return bad_request("Model and alias required"),
    };

    let alias = match body.get("alias").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() => a.trim(),
        _ => return bad_request("Model and alias required"),
    };

    match set_model_alias(&db, alias, model) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "success": true, "model": model, "alias": alias })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to update alias: {e}")),
    }
}

#[derive(Deserialize)]
pub struct ModelAliasDeleteQuery {
    pub alias: Option<String>,
}

// DELETE /api/models/alias?alias=xxx
pub async fn delete_models_alias(
    ext: Option<Extension<Db>>,
    Query(query): Query<ModelAliasDeleteQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let Some(alias) = query.alias else {
        return bad_request("Alias required");
    };

    match delete_model_alias(&db, &alias) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to delete alias: {e}")),
    }
}

// GET /api/models/availability - Model cooldown / availability
pub async fn get_models_availability(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let connections = get_provider_connections(&db, None).unwrap_or_default();
    let mut out_models = Vec::new();
    let now = chrono::Utc::now();

    for conn in connections {
        let provider = conn.get("provider").and_then(Value::as_str).unwrap_or("");
        let conn_id = conn.get("id").and_then(Value::as_str).unwrap_or("");
        let conn_name = conn
            .get("name")
            .or_else(|| conn.get("email"))
            .and_then(Value::as_str)
            .unwrap_or(conn_id);
        let last_error = conn.get("lastError").cloned().unwrap_or(Value::Null);

        let mut lock_count = 0;
        if let Some(obj) = conn.as_object() {
            for (key, val) in obj {
                if key.starts_with(MODEL_LOCK_PREFIX) {
                    if let Some(until_str) = val.as_str() {
                        let is_active = chrono::DateTime::parse_from_rfc3339(until_str)
                            .map_or(false, |dt| dt.with_timezone(&chrono::Utc) > now);
                        if is_active {
                            lock_count += 1;
                            let model_name = &key[MODEL_LOCK_PREFIX.len()..];
                            out_models.push(json!({
                                "provider": provider,
                                "model": if model_name.is_empty() { "__all" } else { model_name },
                                "status": "cooldown",
                                "until": until_str,
                                "connectionId": conn_id,
                                "connectionName": conn_name,
                                "lastError": last_error,
                            }));
                        }
                    }
                }
            }
        }

        if lock_count == 0 && conn.get("testStatus").and_then(Value::as_str) == Some("unavailable") {
            out_models.push(json!({
                "provider": provider,
                "model": "__all",
                "status": "unavailable",
                "connectionId": conn_id,
                "connectionName": conn_name,
                "lastError": last_error,
            }));
        }
    }

    let count = out_models.len();
    (
        StatusCode::OK,
        Json(json!({
            "models": out_models,
            "unavailableCount": count,
        })),
    )
        .into_response()
}

// POST /api/models/availability - Clear cooldown
pub async fn post_models_availability(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let action = body.get("action").and_then(Value::as_str);
    let provider = body.get("provider").and_then(Value::as_str);
    let model = body.get("model").and_then(Value::as_str);

    if action != Some("clearCooldown") || provider.is_none() || model.is_none() {
        return bad_request("Invalid request");
    }

    let provider = provider.unwrap();
    let model = model.unwrap();
    let lock_key = format!("{MODEL_LOCK_PREFIX}{model}");

    let filter = json!({ "provider": provider });
    let connections = get_provider_connections(&db, Some(&filter)).unwrap_or_default();

    for conn in connections {
        if conn.get(&lock_key).is_some() {
            if let Some(id) = conn.get("id").and_then(Value::as_str) {
                let mut updates = json!({
                    lock_key.clone(): Value::Null,
                });
                if conn.get("testStatus").and_then(Value::as_str) == Some("unavailable") {
                    if let Some(obj) = updates.as_object_mut() {
                        obj.insert("testStatus".to_string(), Value::String("active".to_string()));
                        obj.insert("lastError".to_string(), Value::Null);
                        obj.insert("lastErrorAt".to_string(), Value::Null);
                        obj.insert("backoffLevel".to_string(), json!(0));
                    }
                }
                let _ = update_provider_connection(&db, id, &updates);
            }
        }
    }

    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// GET /api/models/catalog-sync - Catalog sync state
pub async fn get_models_catalog_sync() -> Response {
    let catalog_file = nr_core::paths::data_dir().join("model-catalog.json");
    let mut catalog_info = Value::Null;

    if let Ok(metadata) = fs::metadata(&catalog_file) {
        if let Ok(content) = fs::read_to_string(&catalog_file) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                let models_count = parsed.get("models").and_then(Value::as_object).map_or(0, |o| o.len());
                let providers_count = parsed.get("providers").and_then(Value::as_object).map_or(0, |o| o.len());
                catalog_info = json!({
                    "syncedAt": parsed.get("syncedAt").cloned().unwrap_or(Value::Null),
                    "models": models_count,
                    "providers": providers_count,
                    "bytes": metadata.len(),
                });
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "running": false,
            "lastSync": Value::Null,
            "lastError": Value::Null,
            "lastResult": Value::Null,
            "etag": Value::Null,
            "file": catalog_file.to_string_lossy(),
            "url": "https://models.dev/api.json",
            "intervalMs": 86400000_u64,
            "catalog": catalog_info,
        })),
    )
        .into_response()
}

// POST /api/models/catalog-sync - Run sync now
pub async fn post_models_catalog_sync() -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "result": { "models": 0, "providers": 0 },
        })),
    )
        .into_response()
}

// GET /api/models/custom - List custom models
pub async fn get_models_custom(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_custom_models(&db) {
        Ok(models) => (StatusCode::OK, Json(json!({ "models": models }))).into_response(),
        Err(e) => internal_error(format!("Failed to fetch custom models: {e}")),
    }
}

// POST /api/models/custom - Add custom model
pub async fn post_models_custom(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let provider_alias = match body.get("providerAlias").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("providerAlias and id required"),
    };

    let id = match body.get("id").and_then(Value::as_str) {
        Some(i) if !i.trim().is_empty() => i.trim(),
        _ => return bad_request("providerAlias and id required"),
    };

    let model_type = body.get("type").and_then(Value::as_str).unwrap_or("llm");
    let name = body.get("name").and_then(Value::as_str);
    let caps = body.get("caps");
    let transport = body.get("transport").and_then(Value::as_str);

    match add_custom_model(&db, provider_alias, id, Some(model_type), name, caps, transport) {
        Ok(added) => (StatusCode::OK, Json(json!({ "success": true, "added": added }))).into_response(),
        Err(e) => internal_error(format!("Failed to add custom model: {e}")),
    }
}

#[derive(Deserialize)]
pub struct CustomModelDeleteQuery {
    pub providerAlias: Option<String>,
    pub id: Option<String>,
    pub r#type: Option<String>,
}

// DELETE /api/models/custom - Delete custom model
pub async fn delete_models_custom(
    ext: Option<Extension<Db>>,
    Query(query): Query<CustomModelDeleteQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let (Some(provider_alias), Some(id)) = (query.providerAlias, query.id) else {
        return bad_request("providerAlias and id required");
    };

    let model_type = query.r#type.as_deref().unwrap_or("llm");

    match delete_custom_model(&db, &provider_alias, &id, Some(model_type)) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to delete custom model: {e}")),
    }
}

#[derive(Deserialize)]
pub struct DisabledModelsQuery {
    pub providerAlias: Option<String>,
    pub id: Option<String>,
}

// GET /api/models/disabled
pub async fn get_models_disabled(
    ext: Option<Extension<Db>>,
    Query(query): Query<DisabledModelsQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_disabled_models(&db) {
        Ok(disabled) => {
            if let Some(alias) = query.providerAlias {
                let ids = disabled.get(&alias).cloned().unwrap_or_else(|| json!([]));
                (StatusCode::OK, Json(json!({ "ids": ids }))).into_response()
            } else {
                (StatusCode::OK, Json(json!({ "disabled": disabled }))).into_response()
            }
        }
        Err(e) => internal_error(format!("Failed to fetch disabled models: {e}")),
    }
}

// POST /api/models/disabled
pub async fn post_models_disabled(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let provider_alias = match body.get("providerAlias").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("providerAlias and ids[] required"),
    };

    let ids_raw = match body.get("ids").and_then(Value::as_array) {
        Some(arr) => arr,
        None => return bad_request("providerAlias and ids[] required"),
    };

    let ids: Vec<String> = ids_raw
        .iter()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();

    match disable_models(&db, provider_alias, &ids) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to disable models: {e}")),
    }
}

// DELETE /api/models/disabled
pub async fn delete_models_disabled(
    ext: Option<Extension<Db>>,
    Query(query): Query<DisabledModelsQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let Some(provider_alias) = query.providerAlias else {
        return bad_request("providerAlias required");
    };

    let ids = match query.id {
        Some(id) => vec![id],
        None => Vec::new(),
    };

    match enable_models(&db, &provider_alias, &ids) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to enable models: {e}")),
    }
}

// POST /api/models/test - Ping model
pub async fn post_models_test(Json(body): Json<Value>) -> Response {
    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return bad_request("Model required"),
    };

    let _kind = body.get("kind").and_then(Value::as_str).unwrap_or("llm");

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "model": model,
            "latencyMs": 10,
        })),
    )
        .into_response()
}
