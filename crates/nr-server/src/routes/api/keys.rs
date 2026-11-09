use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{get_api_key_by_id, get_api_keys, normalize_api_key_limits, ApiKey, Db, KeyAccess};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

use super::common::{bad_request, get_consistent_machine_id, get_db, internal_error, not_found};

pub const ENDPOINT_KINDS: &[&str] = &[
    "chat",
    "embeddings",
    "images",
    "tts",
    "stt",
    "video",
    "search",
    "fetch",
    "systemone",
];

const KEY_ACCESS_MAX_ENTRIES: usize = 200;
const KEY_ACCESS_MAX_ENTRY_LENGTH: usize = 256;

fn api_key_to_json(key: &ApiKey) -> Value {
    json!({
        "id": key.id,
        "key": key.key,
        "name": key.name,
        "machineId": key.machine_id,
        "isActive": key.is_active,
        "expiredAt": key.expired_at,
        "limitConcurrent": key.limit_concurrent,
        "rpm": key.rpm,
        "createdAt": key.created_at,
        "allowedModels": key.allowed_models,
        "allowedEndpoints": key.allowed_endpoints,
        "access": {
            "restricted": key.access.restricted,
            "allow": key.access.allow,
        }
    })
}

// GET /api/keys - List API keys with monitoring
pub async fn get_keys(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let keys = match get_api_keys(&db) {
        Ok(k) => k,
        Err(e) => return internal_error(format!("Failed to fetch keys: {e}")),
    };

    let mut keys_json = Vec::with_capacity(keys.len());
    for key in keys {
        let mut val = api_key_to_json(&key);
        let monitoring = nr_db::get_api_key_monitoring(&db, &key.id).unwrap_or_else(|_| {
            json!({
                "activeRequests": 0,
                "queuedRequests": 0,
                "requestsLastMinute": 0,
                "requestsLast24h": 0,
                "errorsLast24h": 0,
                "lastRequestAt": Value::Null,
            })
        });
        val["monitoring"] = monitoring;
        keys_json.push(val);
    }

    (StatusCode::OK, Json(json!({ "keys": keys_json }))).into_response()
}

// POST /api/keys - Create API key
pub async fn post_keys(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => return bad_request("Name is required"),
    };

    let limits = match normalize_api_key_limits(&body) {
        Ok(l) => l,
        Err(e) => return bad_request(e.to_string()),
    };

    let machine_id = get_consistent_machine_id(None);
    let mut key = match nr_db::create_api_key(&db, Some(name.clone()), machine_id.clone(), None) {
        Ok(k) => k,
        Err(e) => return internal_error(format!("Failed to create key: {e}")),
    };

    // Apply limits if provided
    let mut update_sql = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(exp) = limits.get("expiredAt") {
        update_sql.push("expiredAt = ?");
        if exp.is_null() {
            key.expired_at = None;
            params.push(Box::new(Option::<String>::None));
        } else if let Some(s) = exp.as_str() {
            key.expired_at = Some(s.to_string());
            params.push(Box::new(Some(s.to_string())));
        }
    }

    if let Some(limit_concurrent) = limits.get("limitConcurrent").and_then(|v| v.as_u64()) {
        update_sql.push("limitConcurrent = ?");
        key.limit_concurrent = limit_concurrent as u32;
        params.push(Box::new(limit_concurrent as i64));
    }

    if let Some(rpm) = limits.get("rpm").and_then(|v| v.as_u64()) {
        update_sql.push("rpm = ?");
        key.rpm = rpm as u32;
        params.push(Box::new(rpm as i64));
    }

    if !update_sql.is_empty() {
        let sql = format!("UPDATE apiKeys SET {} WHERE id = ?", update_sql.join(", "));
        params.push(Box::new(key.id.clone()));
        let rusqlite_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let _ = db.execute_with_params(&sql, rusqlite_params.as_slice());
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "key": key.key,
            "name": key.name,
            "id": key.id,
            "machineId": key.machine_id,
            "expiredAt": key.expired_at,
            "limitConcurrent": key.limit_concurrent,
            "rpm": key.rpm,
        })),
    )
        .into_response()
}

// GET /api/keys/:id - Get single API key
pub async fn get_key_by_id_handler(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_api_key_by_id(&db, &id) {
        Ok(Some(key)) => (StatusCode::OK, Json(json!({ "key": api_key_to_json(&key) }))).into_response(),
        Ok(None) => not_found("Key not found"),
        Err(e) => internal_error(format!("Failed to fetch key: {e}")),
    }
}

fn validate_key_access_input(val: &Value) -> Result<KeyAccess, String> {
    let obj = val.as_object().ok_or_else(|| "access must be an object { restricted, allow }".to_string())?;
    for k in obj.keys() {
        if k != "restricted" && k != "allow" {
            return Err(format!("access: unexpected field(s) {k}"));
        }
    }
    let restricted = obj
        .get("restricted")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| "access.restricted must be a boolean".to_string())?;

    let allow_val = obj.get("allow").unwrap_or(&Value::Array(vec![]));
    let allow_arr = allow_val
        .as_array()
        .ok_or_else(|| "access.allow must be an array of strings".to_string())?;

    if allow_arr.len() > KEY_ACCESS_MAX_ENTRIES {
        return Err(format!("access.allow has more than {KEY_ACCESS_MAX_ENTRIES} entries"));
    }

    let mut allow = Vec::new();
    let mut seen = HashSet::new();
    for item in allow_arr {
        let s = item
            .as_str()
            .ok_or_else(|| "access.allow must be an array of strings".to_string())?;
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > KEY_ACCESS_MAX_ENTRY_LENGTH {
            return Err(format!(
                "access.allow entries must be at most {KEY_ACCESS_MAX_ENTRY_LENGTH} characters"
            ));
        }
        if seen.insert(trimmed.to_lowercase()) {
            allow.push(trimmed.to_string());
        }
    }

    Ok(KeyAccess {
        restricted,
        allow: if restricted { allow } else { vec![] },
    })
}

// PUT /api/keys/:id - Update API key
pub async fn put_key_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let existing = match get_api_key_by_id(&db, &id) {
        Ok(Some(k)) => k,
        Ok(None) => return not_found("Key not found"),
        Err(e) => return internal_error(format!("Failed to fetch key: {e}")),
    };

    let limits = match normalize_api_key_limits(&body) {
        Ok(l) => l,
        Err(e) => return bad_request(e.to_string()),
    };

    let mut update_clauses = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(is_active) = body.get("isActive").and_then(|v| v.as_bool()) {
        update_clauses.push("isActive = ?");
        params.push(Box::new(if is_active { 1 } else { 0 }));
    }

    if let Some(name_val) = body.get("name") {
        match name_val.as_str() {
            Some(n) if !n.trim().is_empty() => {
                update_clauses.push("name = ?");
                params.push(Box::new(n.trim().to_string()));
            }
            _ => return bad_request("Invalid name"),
        }
    }

    if let Some(models_val) = body.get("allowedModels") {
        let arr = match models_val.as_array() {
            Some(a) if a.len() <= 1000 => a,
            _ => return bad_request("allowedModels must be an array of strings"),
        };
        let mut clean = Vec::new();
        let mut seen = HashSet::new();
        for item in arr {
            match item.as_str() {
                Some(s) if !s.trim().is_empty() && s.len() <= 300 => {
                    let trimmed = s.trim().to_string();
                    if seen.insert(trimmed.clone()) {
                        clean.push(trimmed);
                    }
                }
                _ => return bad_request("allowedModels must be an array of strings"),
            }
        }
        update_clauses.push("allowedModels = ?");
        params.push(Box::new(serde_json::to_string(&clean).unwrap_or_default()));
    }

    if let Some(endpoints_val) = body.get("allowedEndpoints") {
        let arr = match endpoints_val.as_array() {
            Some(a) if a.len() <= 1000 => a,
            _ => {
                return bad_request(format!(
                    "allowedEndpoints must be a subset of: {}",
                    ENDPOINT_KINDS.join(", ")
                ))
            }
        };
        let mut clean = Vec::new();
        let mut seen = HashSet::new();
        for item in arr {
            match item.as_str() {
                Some(s) if ENDPOINT_KINDS.contains(&s) => {
                    if seen.insert(s.to_string()) {
                        clean.push(s.to_string());
                    }
                }
                _ => {
                    return bad_request(format!(
                        "allowedEndpoints must be a subset of: {}",
                        ENDPOINT_KINDS.join(", ")
                    ))
                }
            }
        }
        update_clauses.push("allowedEndpoints = ?");
        params.push(Box::new(serde_json::to_string(&clean).unwrap_or_default()));
    }

    if let Some(access_val) = body.get("access") {
        match validate_key_access_input(access_val) {
            Ok(access) => {
                update_clauses.push("accessRestricted = ?");
                params.push(Box::new(if access.restricted { 1 } else { 0 }));
                update_clauses.push("accessAllow = ?");
                params.push(Box::new(serde_json::to_string(&access.allow).unwrap_or_default()));
            }
            Err(e) => return bad_request(e),
        }
    }

    if let Some(exp) = limits.get("expiredAt") {
        update_clauses.push("expiredAt = ?");
        if exp.is_null() {
            params.push(Box::new(Option::<String>::None));
        } else if let Some(s) = exp.as_str() {
            params.push(Box::new(Some(s.to_string())));
        }
    }

    if let Some(num) = limits.get("limitConcurrent").and_then(|v| v.as_i64()) {
        update_clauses.push("limitConcurrent = ?");
        params.push(Box::new(num));
    }

    if let Some(num) = limits.get("rpm").and_then(|v| v.as_i64()) {
        update_clauses.push("rpm = ?");
        params.push(Box::new(num));
    }

    if !update_clauses.is_empty() {
        let sql = format!("UPDATE apiKeys SET {} WHERE id = ?", update_clauses.join(", "));
        params.push(Box::new(id.clone()));
        let rusqlite_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        if let Err(e) = db.execute_with_params(&sql, rusqlite_params.as_slice()) {
            return internal_error(format!("Failed to update key: {e}"));
        }
    }

    match get_api_key_by_id(&db, &id) {
        Ok(Some(updated)) => (StatusCode::OK, Json(json!({ "key": api_key_to_json(&updated) }))).into_response(),
        Ok(None) => (StatusCode::OK, Json(json!({ "key": api_key_to_json(&existing) }))).into_response(),
        Err(e) => internal_error(format!("Failed to reload key: {e}")),
    }
}

// DELETE /api/keys/:id - Delete API key
pub async fn delete_key_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match nr_db::delete_api_key(&db, &id) {
        Ok(true) => (
            StatusCode::OK,
            Json(json!({ "message": "Key deleted successfully" })),
        )
            .into_response(),
        Ok(false) => not_found("Key not found"),
        Err(e) => internal_error(format!("Failed to delete key: {e}")),
    }
}
