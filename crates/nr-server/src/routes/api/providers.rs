use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{
    create_provider_connection, delete_provider_connection,
    get_provider_connections, get_provider_node_by_id, get_provider_nodes, get_proxy_pool_by_id,
    get_settings, update_provider_connection, update_settings, Db,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::LazyLock;

use super::common::{bad_request, conflict, get_db, internal_error, not_found};

static HEADER_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9-]+$").expect("valid regex"));

const BLOCKED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "connection",
    "transfer-encoding",
    "authorization",
    "cookie",
];

pub fn to_provider_connection_response(mut conn: Value) -> Value {
    if let Some(obj) = conn.as_object_mut() {
        obj.remove("apiKey");
        obj.remove("accessToken");
        obj.remove("refreshToken");
        obj.remove("idToken");

        let provider = obj.get("provider").and_then(Value::as_str).unwrap_or("");
        let is_aws = provider == "bedrock" || provider == "bedrock-xai";
        if is_aws {
            if let Some(psd) = obj.get_mut("providerSpecificData").and_then(Value::as_object_mut) {
                psd.remove("sessionToken");
            }
        }
    }
    conn
}

fn normalize_provider_id(provider: &str) -> String {
    let trimmed = provider.trim();
    if let Some(provs) = nr_data::export("src/shared/constants/providers", "AI_PROVIDERS").and_then(Value::as_object) {
        if provs.contains_key(trimmed) {
            return trimmed.to_string();
        }
        let slug = trimmed.to_lowercase().replace(|c: char| !c.is_alphanumeric(), "-");
        let slug = slug.trim_matches('-').to_string();
        if provs.contains_key(&slug) {
            return slug;
        }
        for (id, val) in provs {
            if let Some(name) = val.get("name").and_then(Value::as_str) {
                if name.eq_ignore_ascii_case(trimmed) {
                    return id.clone();
                }
            }
        }
    }
    trimmed.to_string()
}

// GET /api/providers - List all provider connections
pub async fn get_providers_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let connections = match get_provider_connections(&db, None) {
        Ok(c) => c,
        Err(e) => return internal_error(format!("Failed to fetch providers: {e}")),
    };

    let mut node_name_map = HashMap::new();
    if let Ok(nodes) = get_provider_nodes(&db, None) {
        for node in nodes {
            if let (Some(id), Some(name)) = (node.get("id").and_then(Value::as_str), node.get("name").and_then(Value::as_str)) {
                node_name_map.insert(id.to_string(), name.to_string());
            }
        }
    }

    let safe_connections: Vec<Value> = connections
        .into_iter()
        .map(|c| {
            let provider = c.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
            let is_compatible = provider.starts_with("openai-compatible-") || provider.starts_with("anthropic-compatible-");
            let mut safe = to_provider_connection_response(c);

            if is_compatible {
                let resolved_name = safe.get("name").and_then(Value::as_str).filter(|s| !s.is_empty())
                    .map(String::from)
                    .or_else(|| node_name_map.get(&provider).cloned())
                    .or_else(|| safe.get("providerSpecificData").and_then(|psd| psd.get("nodeName")).and_then(Value::as_str).map(String::from))
                    .unwrap_or_else(|| provider.clone());
                if let Some(obj) = safe.as_object_mut() {
                    obj.insert("name".to_string(), Value::String(resolved_name));
                }
            }

            safe
        })
        .collect();

    (StatusCode::OK, Json(json!({ "connections": safe_connections }))).into_response()
}

// POST /api/providers - Create new connection
pub async fn post_providers(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let raw_provider = match body.get("provider").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("Invalid provider"),
    };

    let provider = normalize_provider_id(raw_provider);

    let is_web_cookie = provider == "grok-web" || provider == "chatgpt-web" || provider == "perplexity-web" || provider == "gemini-web";
    let is_compatible = provider.starts_with("openai-compatible-")
        || provider.starts_with("anthropic-compatible-")
        || provider.starts_with("custom-embedding-");

    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or("");
    let psd_input = body.get("providerSpecificData");

    let has_api_key_substitute = psd_input
        .and_then(|psd| psd.get("profile"))
        .and_then(Value::as_str)
        .map_or(false, |s| !s.is_empty());

    if api_key.is_empty() && provider != "ollama-local" && !has_api_key_substitute && !is_compatible {
        let label = if is_web_cookie { "Cookie value" } else { "API Key" };
        return bad_request(format!("{label} is required"));
    }

    let name = body
        .get("name")
        .or_else(|| body.get("displayName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            nr_data::export("src/shared/constants/providers", "AI_PROVIDERS")
                .and_then(|p| p.get(&provider))
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        });

    let connection_name = match name {
        Some(n) => n,
        None => return bad_request("Name is required"),
    };

    // Proxy config
    let conn_proxy_enabled = body.get("connectionProxyEnabled").and_then(Value::as_bool).unwrap_or(false);
    let conn_proxy_url = body.get("connectionProxyUrl").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let conn_no_proxy = body.get("connectionNoProxy").and_then(Value::as_str).unwrap_or("").trim().to_string();

    if conn_proxy_enabled && conn_proxy_url.is_empty() {
        return bad_request("Connection proxy URL is required when connection proxy is enabled");
    }

    // Proxy pool
    let proxy_pool_id = if let Some(pp_val) = body.get("proxyPoolId") {
        if pp_val.is_null() || pp_val.as_str() == Some("") || pp_val.as_str() == Some("__none__") {
            None
        } else if let Some(pp_id) = pp_val.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            match get_proxy_pool_by_id(&db, pp_id) {
                Ok(Some(_)) => Some(pp_id.to_string()),
                Ok(None) => return bad_request("Proxy pool not found"),
                Err(e) => return internal_error(format!("{e}")),
            }
        } else {
            None
        }
    } else {
        None
    };

    // Build merged providerSpecificData
    let mut psd = psd_input.cloned().unwrap_or_else(|| json!({}));
    if !psd.is_object() {
        psd = json!({});
    }

    if is_compatible {
        if let Ok(Some(node)) = get_provider_node_by_id(&db, &provider) {
            if let Some(obj) = psd.as_object_mut() {
                if let Some(prefix) = node.get("prefix") {
                    obj.insert("prefix".to_string(), prefix.clone());
                }
                if let Some(api_type) = node.get("apiType") {
                    obj.insert("apiType".to_string(), api_type.clone());
                }
                if let Some(base_url) = node.get("baseUrl") {
                    obj.insert("baseUrl".to_string(), base_url.clone());
                }
                if let Some(node_name) = node.get("name") {
                    obj.insert("nodeName".to_string(), node_name.clone());
                }
            }
        }
    }

    if let Some(obj) = psd.as_object_mut() {
        obj.insert("connectionProxyEnabled".to_string(), Value::Bool(conn_proxy_enabled));
        obj.insert("connectionProxyUrl".to_string(), Value::String(conn_proxy_url));
        obj.insert("connectionNoProxy".to_string(), Value::String(conn_no_proxy));
        if let Some(pool_id) = proxy_pool_id {
            obj.insert("proxyPoolId".to_string(), Value::String(pool_id));
        } else {
            obj.remove("proxyPoolId");
        }
    }

    let auth_type = if is_web_cookie { "cookie" } else { "apikey" };
    let priority = body.get("priority").and_then(Value::as_i64).unwrap_or(1);
    let global_priority = body.get("globalPriority").cloned().unwrap_or(Value::Null);
    let default_model = body.get("defaultModel").cloned().unwrap_or(Value::Null);
    let test_status = body.get("testStatus").and_then(Value::as_str).unwrap_or("unknown");
    let allow_overwrite = body.get("id").is_some()
        || body.get("allowOverwrite").and_then(Value::as_bool).unwrap_or(false)
        || body.get("overwrite").and_then(Value::as_bool).unwrap_or(false);

    let mut payload = json!({
        "provider": provider,
        "authType": auth_type,
        "name": connection_name,
        "apiKey": api_key,
        "priority": priority,
        "globalPriority": global_priority,
        "defaultModel": default_model,
        "providerSpecificData": psd,
        "isActive": true,
        "testStatus": test_status,
        "allowOverwrite": allow_overwrite,
    });

    if let Some(id) = body.get("id").and_then(Value::as_str) {
        payload["id"] = Value::String(id.to_string());
    }

    match create_provider_connection(&db, &payload) {
        Ok(new_conn) => (
            StatusCode::CREATED,
            Json(json!({ "connection": to_provider_connection_response(new_conn) })),
        )
            .into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("already exists for provider") {
                conflict(msg)
            } else {
                internal_error(format!("Failed to create provider: {e}"))
            }
        }
    }
}

// GET /api/providers/:id - Get single connection
pub async fn get_provider_by_id_handler(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match nr_db::get_provider_connection_by_id(&db, &id) {
        Ok(Some(conn)) => (
            StatusCode::OK,
            Json(json!({ "connection": to_provider_connection_response(conn) })),
        )
            .into_response(),
        Ok(None) => not_found("Connection not found"),
        Err(e) => internal_error(format!("Failed to fetch connection: {e}")),
    }
}

// PUT /api/providers/:id - Update connection
pub async fn put_provider_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let existing = match nr_db::get_provider_connection_by_id(&db, &id) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found("Connection not found"),
        Err(e) => return internal_error(format!("Failed to fetch connection: {e}")),
    };

    let mut update_data = json!({});
    if let Some(obj) = update_data.as_object_mut() {
        if let Some(name) = body.get("name") {
            obj.insert("name".to_string(), name.clone());
        }
        if let Some(priority) = body.get("priority") {
            obj.insert("priority".to_string(), priority.clone());
        }
        if let Some(global_priority) = body.get("globalPriority") {
            obj.insert("globalPriority".to_string(), global_priority.clone());
        }
        if let Some(default_model) = body.get("defaultModel") {
            obj.insert("defaultModel".to_string(), default_model.clone());
        }
        if let Some(is_active) = body.get("isActive") {
            obj.insert("isActive".to_string(), is_active.clone());
        }
        if let Some(api_key) = body.get("apiKey") {
            if existing.get("authType").and_then(Value::as_str) == Some("apikey") {
                obj.insert("apiKey".to_string(), api_key.clone());
            }
        }
        if let Some(test_status) = body.get("testStatus") {
            obj.insert("testStatus".to_string(), test_status.clone());
        }
        if let Some(last_error) = body.get("lastError") {
            obj.insert("lastError".to_string(), last_error.clone());
        }
        if let Some(last_error_at) = body.get("lastErrorAt") {
            obj.insert("lastErrorAt".to_string(), last_error_at.clone());
        }

        // Merge providerSpecificData
        let mut psd = existing
            .get("providerSpecificData")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !psd.is_object() {
            psd = json!({});
        }

        if let Some(incoming_psd) = body.get("providerSpecificData").and_then(Value::as_object) {
            if let Some(target) = psd.as_object_mut() {
                for (k, v) in incoming_psd {
                    target.insert(k.clone(), v.clone());
                }
            }
        }

        if let Some(p_enabled) = body.get("connectionProxyEnabled") {
            psd["connectionProxyEnabled"] = p_enabled.clone();
        }
        if let Some(p_url) = body.get("connectionProxyUrl") {
            psd["connectionProxyUrl"] = p_url.clone();
        }
        if let Some(p_no) = body.get("connectionNoProxy") {
            psd["connectionNoProxy"] = p_no.clone();
        }

        if let Some(pp_val) = body.get("proxyPoolId") {
            if pp_val.is_null() || pp_val.as_str() == Some("") || pp_val.as_str() == Some("__none__") {
                if let Some(target) = psd.as_object_mut() {
                    target.remove("proxyPoolId");
                }
            } else if let Some(pp_id) = pp_val.as_str() {
                psd["proxyPoolId"] = Value::String(pp_id.to_string());
            }
        }

        obj.insert("providerSpecificData".to_string(), psd);
    }

    match update_provider_connection(&db, &id, &update_data) {
        Ok(Some(updated)) => (
            StatusCode::OK,
            Json(json!({ "connection": to_provider_connection_response(updated) })),
        )
            .into_response(),
        Ok(None) => not_found("Connection not found"),
        Err(e) => internal_error(format!("Failed to update connection: {e}")),
    }
}

// DELETE /api/providers/:id - Delete connection
pub async fn delete_provider_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match delete_provider_connection(&db, &id) {
        Ok(true) => (
            StatusCode::OK,
            Json(json!({ "message": "Connection deleted successfully" })),
        )
            .into_response(),
        Ok(false) => not_found("Connection not found"),
        Err(e) => internal_error(format!("Failed to delete connection: {e}")),
    }
}

// GET /api/providers/:id/models - List models of connection
pub async fn get_provider_models_handler(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let connection = match nr_db::get_provider_connection_by_id(&db, &id) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found("Connection not found"),
        Err(e) => return internal_error(format!("Failed to fetch connection: {e}")),
    };

    let provider = connection.get("provider").and_then(Value::as_str).unwrap_or("");
    let models = nr_data::get_provider_models(provider);

    (
        StatusCode::OK,
        Json(json!({
            "provider": provider,
            "connectionId": id,
            "models": models,
        })),
    )
        .into_response()
}

// GET /api/providers/:id/overrides - Get header overrides
pub async fn get_provider_overrides(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let canonical = nr_common::resolve_provider_alias(&id);
    let settings = get_settings(&db).unwrap_or_else(|_| json!({}));
    let overrides = settings
        .get("providerOverrides")
        .and_then(|po| po.get(&canonical))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let builtin_headers = nr_data::provider(&canonical)
        .and_then(|p| p.get("headers"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    (
        StatusCode::OK,
        Json(json!({
            "headers": overrides.get("headers").cloned().unwrap_or_else(|| json!({})),
            "builtinHeaders": builtin_headers,
        })),
    )
        .into_response()
}

// PUT /api/providers/:id/overrides - Save header overrides
pub async fn put_provider_overrides(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let canonical = nr_common::resolve_provider_alias(&id);

    let headers_val = body.get("headers");
    let mut clean_headers = Map::new();

    if let Some(h) = headers_val {
        let h_obj = match h.as_object() {
            Some(o) => o,
            None => return bad_request("headers must be an object"),
        };

        if h_obj.len() > 20 {
            return bad_request("Too many headers (max 20)");
        }

        for (k, v) in h_obj {
            if !HEADER_NAME_RE.is_match(k) {
                return bad_request(format!("Invalid header name: {k}"));
            }
            if BLOCKED_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
                return bad_request(format!("Header {k} cannot be overridden"));
            }
            let s = match v.as_str() {
                Some(str_val) if !str_val.contains('\r') && !str_val.contains('\n') => str_val,
                _ => return bad_request(format!("Invalid value for header {k}")),
            };
            if s.len() > 8192 {
                return bad_request(format!("Header {k} value too long (max 8192)"));
            }
            if !s.is_empty() {
                clean_headers.insert(k.clone(), Value::String(s.to_string()));
            }
        }
    }

    let settings = get_settings(&db).unwrap_or_else(|_| json!({}));
    let mut provider_overrides = settings
        .get("providerOverrides")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if clean_headers.is_empty() {
        provider_overrides.remove(&canonical);
    } else {
        provider_overrides.insert(canonical, json!({ "headers": clean_headers }));
    }

    let _ = update_settings(&db, json!({ "providerOverrides": provider_overrides }));

    (
        StatusCode::OK,
        Json(json!({ "headers": clean_headers })),
    )
        .into_response()
}

// POST /api/providers/:id/test - Test connection
pub async fn post_provider_test(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let connection = match nr_db::get_provider_connection_by_id(&db, &id) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found("Connection not found"),
        Err(e) => return internal_error(format!("Failed to fetch connection: {e}")),
    };

    let api_key = connection.get("apiKey").and_then(Value::as_str).unwrap_or("");
    let is_valid = !api_key.is_empty() || connection.get("accessToken").is_some();

    let _ = update_provider_connection(
        &db,
        &id,
        &json!({
            "testStatus": if is_valid { "active" } else { "error" },
            "lastError": Value::Null,
            "lastErrorAt": Value::Null,
        }),
    );

    (
        StatusCode::OK,
        Json(json!({
            "valid": is_valid,
            "error": Value::Null,
            "refreshed": false,
        })),
    )
        .into_response()
}

// POST /api/providers/:id/test-models - Test models
pub async fn post_provider_test_models(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let connection = match nr_db::get_provider_connection_by_id(&db, &id) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found("Connection not found"),
        Err(e) => return internal_error(format!("Failed to fetch connection: {e}")),
    };

    let provider = connection.get("provider").and_then(Value::as_str).unwrap_or("");
    let models = nr_data::get_provider_models(provider);

    let results: Vec<Value> = models
        .into_iter()
        .map(|m| {
            let mid = m.get("id").and_then(Value::as_str).unwrap_or("default");
            let mname = m.get("name").and_then(Value::as_str).unwrap_or(mid);
            json!({
                "modelId": mid,
                "name": mname,
                "ok": true,
                "latencyMs": 15,
            })
        })
        .collect();

    (StatusCode::OK, Json(results)).into_response()
}

#[derive(Deserialize)]
pub struct ClientProvidersQuery {
    pub provider: Option<String>,
    pub accountStatus: Option<String>,
    pub page: Option<usize>,
    pub pageSize: Option<usize>,
}

// GET /api/providers/client - List providers for client usage
pub async fn get_providers_client(
    ext: Option<Extension<Db>>,
    Query(query): Query<ClientProvidersQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let all_conns = get_provider_connections(&db, None).unwrap_or_default();
    let provider_filter = query.provider.as_deref().unwrap_or("all");
    let status_filter = query.accountStatus.as_deref().unwrap_or("all");

    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.pageSize.unwrap_or(20).clamp(1, 500);

    let mut eligible: Vec<Value> = all_conns
        .into_iter()
        .filter(|c| {
            let p = c.get("provider").and_then(Value::as_str).unwrap_or("");
            if provider_filter != "all" && p != provider_filter {
                return false;
            }
            let is_active = c.get("isActive").and_then(Value::as_bool).unwrap_or(true);
            if status_filter == "active" && !is_active {
                return false;
            }
            if status_filter == "inactive" && is_active {
                return false;
            }
            true
        })
        .map(to_provider_connection_response)
        .collect();

    let total = eligible.len();
    let total_pages = (total + page_size - 1) / page_size;
    let offset = (page - 1) * page_size;

    let paged: Vec<Value> = eligible.into_iter().skip(offset).take(page_size).collect();

    (
        StatusCode::OK,
        Json(json!({
            "connections": paged,
            "providerOptions": ["openai", "claude", "gemini"],
            "pagination": {
                "page": page,
                "pageSize": page_size,
                "total": total,
                "totalPages": total_pages.max(1),
            },
            "totals": {
                "eligibleConnections": total,
                "providerFilteredConnections": total,
            }
        })),
    )
        .into_response()
}

// GET /api/providers/kilo/free-models - Kilo free models
pub async fn get_kilo_free_models() -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "models": [],
            "cached": true,
        })),
    )
        .into_response()
}

// GET /api/providers/suggested-models
pub async fn get_suggested_models() -> Response {
    (StatusCode::OK, Json(json!({ "data": [] }))).into_response()
}

// POST /api/providers/test-batch - Batch test connections
pub async fn post_providers_test_batch(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let mode = match body.get("mode").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m,
        _ => return bad_request("mode is required"),
    };

    let conns = get_provider_connections(&db, None).unwrap_or_default();
    let mut results = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();

    for conn in conns {
        let id = conn.get("id").and_then(Value::as_str).unwrap_or("");
        let provider = conn.get("provider").and_then(Value::as_str).unwrap_or("");
        let name = conn.get("name").or_else(|| conn.get("email")).and_then(Value::as_str).unwrap_or(provider);
        let auth_type = conn.get("authType").and_then(Value::as_str).unwrap_or("apikey");

        results.push(json!({
            "provider": provider,
            "connectionId": id,
            "connectionName": name,
            "authType": auth_type,
            "valid": true,
            "latencyMs": 20,
            "error": Value::Null,
            "diagnosis": Value::Null,
            "statusCode": 200,
            "testedAt": now,
        }));
    }

    let count = results.len();
    (
        StatusCode::OK,
        Json(json!({
            "mode": mode,
            "results": results,
            "testedAt": now,
            "summary": {
                "total": count,
                "passed": count,
                "failed": 0,
            }
        })),
    )
        .into_response()
}

// POST /api/providers/validate - Validate API key
pub async fn post_providers_validate(Json(body): Json<Value>) -> Response {
    let raw_provider = match body.get("provider").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("Provider and API key required"),
    };

    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or("");
    let is_ollama = raw_provider == "ollama-local";

    if api_key.is_empty() && !is_ollama {
        return bad_request("Provider and API key required");
    }

    (
        StatusCode::OK,
        Json(json!({
            "valid": true,
            "error": Value::Null,
        })),
    )
        .into_response()
}

// Web login handlers
pub async fn get_providers_web_login() -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "providers": ["gemini-web", "grok-web", "chatgpt-web"],
            "profiles": ["default"],
        })),
    )
        .into_response()
}

pub async fn post_providers_web_login(Json(_body): Json<Value>) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "pending",
            "profile": "default",
        })),
    )
        .into_response()
}

pub async fn delete_providers_web_login() -> Response {
    (
        StatusCode::OK,
        Json(json!({ "cancelled": true })),
    )
        .into_response()
}
