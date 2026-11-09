use axum::extract::Extension;
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{get_settings, update_settings, Db};
use serde_json::{json, Map, Value};
use std::time::Instant;

use super::common::{bad_request, get_db, internal_error, unauthorized};

const PROTECTED_SETTING_KEYS: &[&str] = &["password", "mitmSudoEncrypted"];
const DEFAULT_PASSWORD: &str = "123456";

fn is_cli_request(headers: &HeaderMap) -> bool {
    headers.contains_key("x-9r-cli-token")
}

pub fn verify_dashboard_password(settings: &Value, password: &str) -> bool {
    if password.is_empty() {
        return false;
    }

    if let Some(stored_hash) = settings.get("password").and_then(|v| v.as_str()) {
        if !stored_hash.is_empty() {
            return bcrypt::verify(password, stored_hash).unwrap_or(false);
        }
    }

    let initial = std::env::var("INITIAL_PASSWORD").unwrap_or_else(|_| DEFAULT_PASSWORD.to_string());
    password == initial
}

// GET /api/settings - Get settings
pub async fn get_settings_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let settings = match get_settings(&db) {
        Ok(s) => s,
        Err(e) => return internal_error(format!("Failed to get settings: {e}")),
    };

    let has_password = settings
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let oidc_configured = settings.get("oidcIssuerUrl").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty())
        && settings.get("oidcClientId").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty())
        && settings.get("oidcClientSecret").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty());

    let enable_request_logs = std::env::var("ENABLE_REQUEST_LOGS").as_deref() == Ok("true");
    let enable_translator = std::env::var("ENABLE_TRANSLATOR").as_deref() == Ok("true");

    let mut safe_settings = settings;
    if let Some(obj) = safe_settings.as_object_mut() {
        obj.remove("password");
        obj.remove("oidcClientSecret");
        obj.insert("oidcConfigured".to_string(), Value::Bool(oidc_configured));
        obj.insert("enableRequestLogs".to_string(), Value::Bool(enable_request_logs));
        obj.insert("enableTranslator".to_string(), Value::Bool(enable_translator));
        obj.insert("hasPassword".to_string(), Value::Bool(has_password));
    }

    let mut resp = (StatusCode::OK, Json(safe_settings)).into_response();
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// PATCH /api/settings - Update settings
pub async fn patch_settings(ext: Option<Extension<Db>>, Json(mut body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let mut body_map = match body.as_object_mut() {
        Some(m) => m.clone(),
        None => return bad_request("Invalid body"),
    };

    // Strip protected keys
    for key in PROTECTED_SETTING_KEYS {
        body_map.remove(*key);
    }

    // Password change
    if let Some(new_password) = body_map.remove("newPassword").and_then(|v| v.as_str().map(String::from)) {
        let current_password = body_map.remove("currentPassword").and_then(|v| v.as_str().map(String::from));
        let settings = match get_settings(&db) {
            Ok(s) => s,
            Err(e) => return internal_error(format!("Failed to get settings: {e}")),
        };

        let current_hash = settings.get("password").and_then(|v| v.as_str());
        if let Some(hash) = current_hash {
            if !hash.is_empty() {
                let Some(curr) = current_password else {
                    return bad_request("Current password required");
                };
                if !bcrypt::verify(&curr, hash).unwrap_or(false) {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "Invalid current password" }))).into_response();
                }
            } else if let Some(curr) = current_password {
                if curr != DEFAULT_PASSWORD && curr != "123456" {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "Invalid current password" }))).into_response();
                }
            }
        } else if let Some(curr) = current_password {
            if curr != DEFAULT_PASSWORD && curr != "123456" {
                return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "Invalid current password" }))).into_response();
            }
        }

        match bcrypt::hash(&new_password, 10) {
            Ok(hashed) => {
                body_map.insert("password".to_string(), Value::String(hashed));
            }
            Err(e) => return internal_error(format!("Failed to hash password: {e}")),
        }
    }

    if let Some(secret) = body_map.get("oidcClientSecret") {
        if secret.as_str().map_or(true, |s| s.trim().is_empty()) {
            body_map.remove("oidcClientSecret");
        }
    }

    let updated = match update_settings(&db, Value::Object(body_map)) {
        Ok(s) => s,
        Err(e) => return internal_error(format!("Failed to update settings: {e}")),
    };

    let oidc_configured = updated.get("oidcIssuerUrl").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty())
        && updated.get("oidcClientId").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty())
        && updated.get("oidcClientSecret").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty());

    let mut safe = updated;
    if let Some(obj) = safe.as_object_mut() {
        obj.remove("password");
        obj.remove("oidcClientSecret");
        obj.insert("oidcConfigured".to_string(), Value::Bool(oidc_configured));
    }

    let mut resp = (StatusCode::OK, Json(safe)).into_response();
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// GET /api/settings/database - Export DB
pub async fn get_settings_database(ext: Option<Extension<Db>>, headers: HeaderMap) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let settings = match get_settings(&db) {
        Ok(s) => s,
        Err(e) => return internal_error(format!("Failed to get settings: {e}")),
    };

    if !is_cli_request(&headers) {
        let pass = headers.get("x-9r-password").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !verify_dashboard_password(&settings, pass) {
            return unauthorized("Invalid password");
        }
    }

    let exported_settings = nr_db::export_settings(&db).unwrap_or_else(|_| json!({}));

    // Provider connections
    let provider_connections: Vec<Value> = db
        .query_all("SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections", &[], |row| {
            let data_str: String = row.get(7)?;
            let mut val: Value = serde_json::from_str(&data_str).unwrap_or_else(|_| json!({}));
            val["id"] = Value::String(row.get(0)?);
            val["provider"] = Value::String(row.get(1)?);
            val["authType"] = Value::String(row.get(2)?);
            val["name"] = row.get::<_, Option<String>>(3)?.map(Value::String).unwrap_or(Value::Null);
            val["email"] = row.get::<_, Option<String>>(4)?.map(Value::String).unwrap_or(Value::Null);
            val["priority"] = row.get::<_, Option<i32>>(5)?.map(|p| Value::Number(p.into())).unwrap_or(Value::Null);
            val["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
            val["createdAt"] = Value::String(row.get(8)?);
            val["updatedAt"] = Value::String(row.get(9)?);
            Ok(val)
        })
        .unwrap_or_default();

    // Provider nodes
    let provider_nodes: Vec<Value> = db
        .query_all("SELECT id, type, name, data, createdAt, updatedAt FROM providerNodes", &[], |row| {
            let data_str: String = row.get(3)?;
            let mut val: Value = serde_json::from_str(&data_str).unwrap_or_else(|_| json!({}));
            val["id"] = Value::String(row.get(0)?);
            val["type"] = row.get::<_, Option<String>>(1)?.map(Value::String).unwrap_or(Value::Null);
            val["name"] = row.get::<_, Option<String>>(2)?.map(Value::String).unwrap_or(Value::Null);
            val["createdAt"] = Value::String(row.get(4)?);
            val["updatedAt"] = Value::String(row.get(5)?);
            Ok(val)
        })
        .unwrap_or_default();

    // Proxy pools
    let proxy_pools: Vec<Value> = db
        .query_all("SELECT id, isActive, testStatus, data, createdAt, updatedAt FROM proxyPools", &[], |row| {
            let data_str: String = row.get(3)?;
            let mut val: Value = serde_json::from_str(&data_str).unwrap_or_else(|_| json!({}));
            val["id"] = Value::String(row.get(0)?);
            val["isActive"] = Value::Bool(row.get::<_, i32>(1)? == 1);
            val["testStatus"] = row.get::<_, Option<String>>(2)?.map(Value::String).unwrap_or_else(|| Value::String("unknown".to_string()));
            val["createdAt"] = Value::String(row.get(4)?);
            val["updatedAt"] = Value::String(row.get(5)?);
            Ok(val)
        })
        .unwrap_or_default();

    // API keys
    let api_keys: Vec<Value> = match nr_db::get_api_keys(&db) {
        Ok(keys) => keys
            .into_iter()
            .map(|k| {
                json!({
                    "id": k.id,
                    "key": k.key,
                    "name": k.name,
                    "machineId": k.machine_id,
                    "isActive": k.is_active,
                    "expiredAt": k.expired_at,
                    "limitConcurrent": k.limit_concurrent,
                    "rpm": k.rpm,
                    "createdAt": k.created_at,
                    "allowedModels": k.allowed_models,
                    "allowedEndpoints": k.allowed_endpoints,
                    "access": {
                        "restricted": k.access.restricted,
                        "allow": k.access.allow,
                    }
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    // Combos
    let combos: Vec<Value> = match nr_db::get_combos(&db) {
        Ok(c) => c
            .into_iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "name": item.name,
                    "kind": item.kind,
                    "models": item.models,
                    "createdAt": item.created_at,
                    "updatedAt": item.updated_at,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    // KV tables: modelAliases, customModels, mitmAlias, pricing
    let model_aliases = nr_db::get_model_aliases(&db).unwrap_or_else(|_| json!({}));
    let custom_models = nr_db::get_custom_models(&db).unwrap_or_default();
    let mitm_alias = nr_db::get_mitm_alias(&db, None).unwrap_or_else(|_| json!({}));
    let pricing: Value = {
        let rows = db.query_all("SELECT key, value FROM kv WHERE scope = 'pricing'", &[], |row| {
            let k: String = row.get(0)?;
            let v: String = row.get(1)?;
            Ok((k, serde_json::from_str::<Value>(&v).unwrap_or(Value::Null)))
        }).unwrap_or_default();
        let mut map = Map::new();
        for (k, v) in rows {
            map.insert(k, v);
        }
        Value::Object(map)
    };

    (
        StatusCode::OK,
        Json(json!({
            "settings": exported_settings,
            "providerConnections": provider_connections,
            "providerNodes": provider_nodes,
            "proxyPools": proxy_pools,
            "apiKeys": api_keys,
            "combos": combos,
            "modelAliases": model_aliases,
            "customModels": custom_models,
            "mitmAlias": mitm_alias,
            "pricing": pricing,
        })),
    )
        .into_response()
}

// POST /api/settings/database - Import DB
pub async fn post_settings_database(
    ext: Option<Extension<Db>>,
    headers: HeaderMap,
    Json(mut payload): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let settings = match get_settings(&db) {
        Ok(s) => s,
        Err(e) => return internal_error(format!("Failed to get settings: {e}")),
    };

    let pass = payload
        .as_object_mut()
        .and_then(|obj| obj.remove("password"))
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();

    if !is_cli_request(&headers) && !verify_dashboard_password(&settings, &pass) {
        return unauthorized("Invalid password");
    }

    let import_res = db.with_tx(|tx| {
        // Wipe tables (except _meta)
        tx.execute("DELETE FROM settings")?;
        tx.execute("DELETE FROM providerConnections")?;
        tx.execute("DELETE FROM providerNodes")?;
        tx.execute("DELETE FROM proxyPools")?;
        tx.execute("DELETE FROM apiKeys")?;
        tx.execute("DELETE FROM combos")?;
        tx.execute("DELETE FROM kv WHERE scope IN ('modelAliases', 'customModels', 'mitmAlias', 'pricing')")?;

        if let Some(s) = payload.get("settings") {
            let data_str = s.to_string();
            tx.execute_with_params("INSERT INTO settings(id, data) VALUES(1, ?) ON CONFLICT(id) DO UPDATE SET data = excluded.data", [&data_str])?;
        }

        if let Some(conns) = payload.get("providerConnections").and_then(|v| v.as_array()) {
            for c in conns {
                let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let provider = c.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                let auth_type = c.get("authType").and_then(|v| v.as_str()).unwrap_or("oauth");
                let name = c.get("name").and_then(|v| v.as_str());
                let email = c.get("email").and_then(|v| v.as_str());
                let priority = c.get("priority").and_then(|v| v.as_i64()).unwrap_or(0);
                let is_active = c.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true);
                let created_at = c.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
                let updated_at = c.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");

                let mut extra = c.clone();
                for field in &["id", "provider", "authType", "name", "email", "priority", "isActive", "createdAt", "updatedAt"] {
                    extra.as_object_mut().map(|o| o.remove(*field));
                }

                tx.execute_with_params(
                    "INSERT OR REPLACE INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (id, provider, auth_type, name, email, priority, if is_active { 1 } else { 0 }, extra.to_string(), created_at, updated_at),
                )?;
            }
        }

        if let Some(nodes) = payload.get("providerNodes").and_then(|v| v.as_array()) {
            for n in nodes {
                let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let node_type = n.get("type").and_then(|v| v.as_str());
                let name = n.get("name").and_then(|v| v.as_str());
                let created_at = n.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
                let updated_at = n.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");

                let mut extra = n.clone();
                for field in &["id", "type", "name", "createdAt", "updatedAt"] {
                    extra.as_object_mut().map(|o| o.remove(*field));
                }

                tx.execute_with_params(
                    "INSERT OR REPLACE INTO providerNodes(id, type, name, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                    (id, node_type, name, extra.to_string(), created_at, updated_at),
                )?;
            }
        }

        if let Some(pools) = payload.get("proxyPools").and_then(|v| v.as_array()) {
            for p in pools {
                let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let is_active = p.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true);
                let test_status = p.get("testStatus").and_then(|v| v.as_str()).unwrap_or("unknown");
                let created_at = p.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
                let updated_at = p.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");

                let mut extra = p.clone();
                for field in &["id", "isActive", "testStatus", "createdAt", "updatedAt"] {
                    extra.as_object_mut().map(|o| o.remove(*field));
                }

                tx.execute_with_params(
                    "INSERT OR REPLACE INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                    (id, if is_active { 1 } else { 0 }, test_status, extra.to_string(), created_at, updated_at),
                )?;
            }
        }

        if let Some(keys) = payload.get("apiKeys").and_then(|v| v.as_array()) {
            for k in keys {
                let id = k.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let key = k.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let name = k.get("name").and_then(|v| v.as_str());
                let machine_id = k.get("machineId").and_then(|v| v.as_str());
                let is_active = k.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true);
                let expired_at = k.get("expiredAt").and_then(|v| v.as_str());
                let limit_concurrent = k.get("limitConcurrent").and_then(|v| v.as_i64()).unwrap_or(0);
                let rpm = k.get("rpm").and_then(|v| v.as_i64()).unwrap_or(0);
                let created_at = k.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
                let allowed_models = k.get("allowedModels").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());
                let allowed_endpoints = k.get("allowedEndpoints").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());

                let restricted = k.get("access").and_then(|v| v.get("restricted")).and_then(|v| v.as_bool()).unwrap_or(false);
                let allow = k.get("access").and_then(|v| v.get("allow")).map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());

                tx.execute_with_params(
                    "INSERT OR REPLACE INTO apiKeys(id, key, name, machineId, isActive, expiredAt, limitConcurrent, rpm, createdAt, allowedModels, allowedEndpoints, accessRestricted, accessAllow) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (id, key, name, machine_id, if is_active { 1 } else { 0 }, expired_at, limit_concurrent, rpm, created_at, allowed_models, allowed_endpoints, if restricted { 1 } else { 0 }, allow),
                )?;
            }
        }

        if let Some(combos) = payload.get("combos").and_then(|v| v.as_array()) {
            for c in combos {
                let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let kind = c.get("kind").and_then(|v| v.as_str());
                let models = c.get("models").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());
                let created_at = c.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
                let updated_at = c.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");

                tx.execute_with_params(
                    "INSERT OR REPLACE INTO combos(id, name, kind, models, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                    (id, name, kind, models, created_at, updated_at),
                )?;
            }
        }

        if let Some(aliases) = payload.get("modelAliases").and_then(|v| v.as_object()) {
            for (k, v) in aliases {
                let val_str = v.as_str().unwrap_or("");
                tx.execute_with_params(
                    "INSERT INTO kv(scope, key, value) VALUES('modelAliases', ?, ?)",
                    (k, val_str),
                )?;
            }
        }

        if let Some(custom) = payload.get("customModels").and_then(|v| v.as_array()) {
            for m in custom {
                let p_alias = m.get("providerAlias").and_then(|v| v.as_str()).unwrap_or("");
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let m_type = m.get("type").and_then(|v| v.as_str()).unwrap_or("llm");
                let key = format!("{p_alias}|{id}|{m_type}");
                tx.execute_with_params(
                    "INSERT INTO kv(scope, key, value) VALUES('customModels', ?, ?)",
                    (&key, m.to_string()),
                )?;
            }
        }

        if let Some(pricing) = payload.get("pricing").and_then(|v| v.as_object()) {
            for (k, v) in pricing {
                tx.execute_with_params(
                    "INSERT INTO kv(scope, key, value) VALUES('pricing', ?, ?)",
                    (k, v.to_string()),
                )?;
            }
        }

        Ok(())
    });

    match import_res {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to import database: {e}")),
    }
}

// POST /api/settings/proxy-test - Test proxy URL
pub async fn post_settings_proxy_test(Json(body): Json<Value>) -> Response {
    let proxy_url = match body.get("proxyUrl").and_then(|v| v.as_str()) {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => return (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "status": 400, "error": "proxyUrl is required" }))).into_response(),
    };

    let test_url = body
        .get("testUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("https://google.com/");

    let timeout_ms = body
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(8000)
        .min(30000);

    let proxy = match reqwest::Proxy::all(proxy_url) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "status": 400, "error": format!("Invalid proxy URL: {e}") }))).into_response(),
    };

    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "ok": false, "status": 500, "error": format!("Client build error: {e}") }))).into_response(),
    };

    let start = Instant::now();
    match client.head(test_url).send().await {
        Ok(res) => {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let status = res.status().as_u16();
            let status_text = res.status().canonical_reason().unwrap_or("").to_string();
            let ok = res.status().is_success() || res.status().is_redirection();

            if ok {
                (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "status": status,
                        "statusText": status_text,
                        "url": test_url,
                        "elapsedMs": elapsed_ms,
                    })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(json!({
                        "ok": false,
                        "status": status,
                        "error": format!("Proxy test failed with status {status}"),
                    })),
                )
                    .into_response()
            }
        }
        Err(e) => {
            let msg = if e.is_timeout() {
                "Proxy test timed out".to_string()
            } else {
                format!("{e}")
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "status": 500,
                    "error": msg,
                })),
            )
                .into_response()
        }
    }
}

// GET /api/settings/require-login
pub async fn get_settings_require_login(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let settings = get_settings(&db).unwrap_or_else(|_| json!({}));

    let require_login = settings.get("requireLogin").and_then(|v| v.as_bool()).unwrap_or(true);
    let tunnel_dashboard_access = settings.get("tunnelDashboardAccess").and_then(|v| v.as_bool()).unwrap_or(true);
    let tunnel_url = settings.get("tunnelUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tailscale_url = settings.get("tailscaleUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();

    (
        StatusCode::OK,
        Json(json!({
            "requireLogin": require_login,
            "tunnelDashboardAccess": tunnel_dashboard_access,
            "tunnelUrl": tunnel_url,
            "tailscaleUrl": tailscale_url,
        })),
    )
        .into_response()
}
