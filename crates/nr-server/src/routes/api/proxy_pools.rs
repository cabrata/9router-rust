use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{
    create_proxy_pool, delete_proxy_pool, get_provider_connections, get_proxy_pool_by_id,
    get_proxy_pools, update_proxy_pool, Db,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;

use super::common::{bad_request, get_db, internal_error, not_found};

const VALID_PROXY_TYPES: &[&str] = &["http", "vercel", "cloudflare", "deno", "netlify"];

#[derive(Deserialize)]
pub struct ProxyPoolsQuery {
    pub isActive: Option<String>,
    pub includeUsage: Option<String>,
}

// GET /api/proxy-pools - List proxy pools
pub async fn get_proxy_pools_handler(
    ext: Option<Extension<Db>>,
    Query(query): Query<ProxyPoolsQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let is_active_filter = match query.isActive.as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };

    let filter = is_active_filter.map(|a| json!({ "isActive": a }));
    let mut pools = match get_proxy_pools(&db, filter.as_ref()) {
        Ok(p) => p,
        Err(e) => return internal_error(format!("Failed to fetch proxy pools: {e}")),
    };

    if query.includeUsage.as_deref() == Some("true") {
        let mut usage_map: HashMap<String, usize> = HashMap::new();
        if let Ok(conns) = get_provider_connections(&db, None) {
            for c in conns {
                if let Some(pool_id) = c
                    .get("providerSpecificData")
                    .and_then(|psd| psd.get("proxyPoolId"))
                    .and_then(Value::as_str)
                {
                    *usage_map.entry(pool_id.to_string()).or_default() += 1;
                }
            }
        }

        for pool in &mut pools {
            if let Some(id) = pool.get("id").and_then(Value::as_str) {
                let count = usage_map.get(id).copied().unwrap_or(0);
                if let Some(obj) = pool.as_object_mut() {
                    obj.insert("boundConnectionCount".to_string(), json!(count));
                }
            }
        }
    }

    (StatusCode::OK, Json(json!({ "proxyPools": pools }))).into_response()
}

// POST /api/proxy-pools - Create proxy pool
pub async fn post_proxy_pools(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let name = match body.get("name").and_then(Value::as_str) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        _ => return bad_request("Name is required"),
    };

    let proxy_url = match body.get("proxyUrl").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => return bad_request("Proxy URL is required"),
    };

    let no_proxy = body.get("noProxy").and_then(Value::as_str).unwrap_or("");
    let is_active = body.get("isActive").and_then(Value::as_bool).unwrap_or(true);
    let strict_proxy = body.get("strictProxy").and_then(Value::as_bool).unwrap_or(false);

    let raw_type = body.get("type").and_then(Value::as_str).unwrap_or("http");
    let pool_type = if VALID_PROXY_TYPES.contains(&raw_type) {
        raw_type
    } else {
        "http"
    };

    let data = json!({
        "name": name,
        "proxyUrl": proxy_url,
        "noProxy": no_proxy,
        "isActive": is_active,
        "strictProxy": strict_proxy,
        "type": pool_type,
    });

    match create_proxy_pool(&db, &data) {
        Ok(pool) => (StatusCode::CREATED, Json(json!({ "proxyPool": pool }))).into_response(),
        Err(e) => internal_error(format!("Failed to create proxy pool: {e}")),
    }
}

// GET /api/proxy-pools/:id - Get single proxy pool
pub async fn get_proxy_pool_by_id_handler(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_proxy_pool_by_id(&db, &id) {
        Ok(Some(pool)) => (StatusCode::OK, Json(json!({ "proxyPool": pool }))).into_response(),
        Ok(None) => not_found("Proxy pool not found"),
        Err(e) => internal_error(format!("Failed to fetch proxy pool: {e}")),
    }
}

// PUT /api/proxy-pools/:id - Update proxy pool
pub async fn put_proxy_pool_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let _existing = match get_proxy_pool_by_id(&db, &id) {
        Ok(Some(p)) => p,
        Ok(None) => return not_found("Proxy pool not found"),
        Err(e) => return internal_error(format!("Failed to fetch proxy pool: {e}")),
    };

    if let Some(name_val) = body.get("name") {
        match name_val.as_str() {
            Some(n) if !n.trim().is_empty() => {}
            _ => return bad_request("Name is required"),
        }
    }

    if let Some(url_val) = body.get("proxyUrl") {
        match url_val.as_str() {
            Some(u) if !u.trim().is_empty() => {}
            _ => return bad_request("Proxy URL is required"),
        }
    }

    match update_proxy_pool(&db, &id, &body) {
        Ok(Some(updated)) => (StatusCode::OK, Json(json!({ "proxyPool": updated }))).into_response(),
        Ok(None) => not_found("Proxy pool not found"),
        Err(e) => internal_error(format!("Failed to update proxy pool: {e}")),
    }
}

// DELETE /api/proxy-pools/:id - Delete proxy pool
pub async fn delete_proxy_pool_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let _existing = match get_proxy_pool_by_id(&db, &id) {
        Ok(Some(p)) => p,
        Ok(None) => return not_found("Proxy pool not found"),
        Err(e) => return internal_error(format!("Failed to fetch proxy pool: {e}")),
    };

    // Check usage
    let mut bound_count = 0;
    if let Ok(conns) = get_provider_connections(&db, None) {
        for c in conns {
            if c.get("providerSpecificData")
                .and_then(|psd| psd.get("proxyPoolId"))
                .and_then(Value::as_str)
                == Some(&id)
            {
                bound_count += 1;
            }
        }
    }

    if bound_count > 0 {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "Proxy pool is currently in use",
                "boundConnectionCount": bound_count,
            })),
        )
            .into_response();
    }

    match delete_proxy_pool(&db, &id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Ok(false) => not_found("Proxy pool not found"),
        Err(e) => internal_error(format!("Failed to delete proxy pool: {e}")),
    }
}

// POST /api/proxy-pools/:id/test - Test proxy pool entry
pub async fn post_proxy_pool_test(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let pool = match get_proxy_pool_by_id(&db, &id) {
        Ok(Some(p)) => p,
        Ok(None) => return not_found("Proxy pool not found"),
        Err(e) => return internal_error(format!("Failed to fetch proxy pool: {e}")),
    };

    let proxy_url = pool.get("proxyUrl").and_then(Value::as_str).unwrap_or("");
    let pool_type = pool.get("type").and_then(Value::as_str).unwrap_or("http");

    let now_iso = chrono::Utc::now().to_rfc3339();
    let start = Instant::now();

    let (ok, status, error) = if pool_type == "vercel"
        || pool_type == "cloudflare"
        || pool_type == "deno"
        || pool_type == "netlify"
    {
        // Relay test: send request to relay with x-relay-target header
        let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build() {
            Ok(c) => c,
            Err(e) => return internal_error(format!("{e}")),
        };
        match client
            .get(proxy_url)
            .header("x-relay-target", "https://httpbin.org")
            .header("x-relay-path", "/get")
            .send()
            .await
        {
            Ok(r) => (r.status().is_success(), r.status().as_u16(), None),
            Err(e) => (false, 500, Some(format!("{e}"))),
        }
    } else {
        // Standard HTTP proxy test
        match reqwest::Proxy::all(proxy_url) {
            Ok(p) => {
                match reqwest::Client::builder().proxy(p).timeout(std::time::Duration::from_secs(8)).build() {
                    Ok(client) => {
                        match client.head("https://google.com/").send().await {
                            Ok(r) => (r.status().is_success() || r.status().is_redirection(), r.status().as_u16(), None),
                            Err(e) => (false, 500, Some(format!("{e}"))),
                        }
                    }
                    Err(e) => (false, 500, Some(format!("{e}"))),
                }
            }
            Err(e) => (false, 400, Some(format!("Invalid proxy URL: {e}"))),
        }
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let test_status = if ok { "active" } else { "error" };

    let mut update_data = json!({
        "testStatus": test_status,
        "lastTestedAt": now_iso.clone(),
        "isActive": ok,
    });
    if let Some(err_msg) = &error {
        update_data["lastError"] = Value::String(err_msg.clone());
    } else {
        update_data["lastError"] = Value::Null;
    }

    let _ = update_proxy_pool(&db, &id, &update_data);

    (
        StatusCode::OK,
        Json(json!({
            "ok": ok,
            "status": status,
            "statusText": if ok { "OK" } else { "Error" },
            "error": error,
            "elapsedMs": elapsed_ms,
            "testedAt": now_iso,
        })),
    )
        .into_response()
}

// POST /api/proxy-pools/vercel-deploy
pub async fn post_proxy_pools_vercel_deploy(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let _vercel_token = match body.get("vercelToken").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => return bad_request("Vercel API token is required"),
    };

    let project_name = body
        .get("projectName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("relay-{}", &uuid::Uuid::new_v4().to_string()[..8]));

    let deploy_url = format!("https://{project_name}.vercel.app");

    let pool_data = json!({
        "name": project_name,
        "proxyUrl": deploy_url,
        "type": "vercel",
        "noProxy": "",
        "isActive": true,
        "strictProxy": false,
    });

    match create_proxy_pool(&db, &pool_data) {
        Ok(pool) => (
            StatusCode::CREATED,
            Json(json!({ "proxyPool": pool, "deployUrl": deploy_url })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to create proxy pool: {e}")),
    }
}

// POST /api/proxy-pools/cloudflare-deploy
pub async fn post_proxy_pools_cloudflare_deploy(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let account_id = match body.get("accountId").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() => a.trim(),
        _ => return bad_request("Cloudflare Account ID and API Token are required"),
    };

    let _api_token = match body.get("apiToken").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => return bad_request("Cloudflare Account ID and API Token are required"),
    };

    let project_name = body
        .get("projectName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("relay-{}", &uuid::Uuid::new_v4().to_string()[..8]));

    let deploy_url = format!("https://{project_name}.{account_id}.workers.dev");

    let pool_data = json!({
        "name": project_name,
        "proxyUrl": deploy_url,
        "type": "cloudflare",
        "noProxy": "",
        "isActive": true,
        "strictProxy": false,
    });

    match create_proxy_pool(&db, &pool_data) {
        Ok(pool) => (
            StatusCode::CREATED,
            Json(json!({ "proxyPool": pool, "deployUrl": deploy_url })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to create proxy pool: {e}")),
    }
}

// POST /api/proxy-pools/deno-deploy
pub async fn post_proxy_pools_deno_deploy(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let _deno_token = match body.get("denoToken").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => return bad_request("Deno Deploy API token is required"),
    };

    let project_name = body
        .get("projectName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("relay-{}", &uuid::Uuid::new_v4().to_string()[..8]));

    let deploy_url = format!("https://{project_name}.deno.net");

    let pool_data = json!({
        "name": project_name,
        "proxyUrl": deploy_url,
        "type": "deno",
        "noProxy": "",
        "isActive": true,
        "strictProxy": false,
    });

    match create_proxy_pool(&db, &pool_data) {
        Ok(pool) => (
            StatusCode::CREATED,
            Json(json!({ "proxyPool": pool, "deployUrl": deploy_url })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to create proxy pool: {e}")),
    }
}

// POST /api/proxy-pools/netlify-deploy
pub async fn post_proxy_pools_netlify_deploy(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let _netlify_token = match body.get("netlifyToken").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => return bad_request("Netlify API token is required"),
    };

    let site_name = body
        .get("siteName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("relay-{}", &uuid::Uuid::new_v4().to_string()[..8]));

    let deploy_url = format!("https://{site_name}.netlify.app");

    let pool_data = json!({
        "name": site_name,
        "proxyUrl": deploy_url,
        "type": "netlify",
        "noProxy": "",
        "isActive": true,
        "strictProxy": false,
    });

    match create_proxy_pool(&db, &pool_data) {
        Ok(pool) => (
            StatusCode::CREATED,
            Json(json!({ "proxyPool": pool, "deployUrl": deploy_url })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to create proxy pool: {e}")),
    }
}
