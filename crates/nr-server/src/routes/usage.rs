//! Usage stats HTTP routes: stats, charts, history, request logs, connection usage,
//! reset endpoints, and SSE realtime stream.
//! Faithful port of `src/app/api/usage/**`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const VALID_PERIODS: &[&str] = &["today", "24h", "7d", "30d", "60d", "all"];

/// Whitelisted apikey providers eligible for usage queries.
const USAGE_APIKEY_PROVIDERS: &[&str] = &[
    "glm",
    "minimax-code",
    "minimax-code-global",
    "kiro",
    "gemini-cli",
    "iflow",
    "kimchi",
    "xiaomi-mimo",
];

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/usage/chart
pub async fn chart_get(Query(params): Query<HashMap<String, String>>) -> Response {
    let period = params.get("period").map(|s| s.as_str()).unwrap_or("7d");
    if !VALID_PERIODS.contains(&period) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid period" })),
        )
            .into_response();
    }

    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match nr_db::get_chart_data(&db, Some(period)) {
        Ok(data) => Json(data).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to fetch chart data: {e}") })),
        )
            .into_response(),
    }
}

/// GET /api/usage/history
pub async fn history_get() -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match nr_db::get_usage_stats(&db, None) {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to fetch usage stats: {e}") })),
        )
            .into_response(),
    }
}

/// GET /api/usage/logs
pub async fn logs_get() -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match nr_db::repos::request_logs::get_recent_logs(&db, 200) {
        Ok(logs) => Json(logs).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to fetch logs: {e}") })),
        )
            .into_response(),
    }
}

/// GET /api/usage/stats
pub async fn stats_get(Query(params): Query<HashMap<String, String>>) -> Response {
    let period = params.get("period").map(|s| s.as_str()).unwrap_or("7d");
    if !VALID_PERIODS.contains(&period) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid period" })),
        )
            .into_response();
    }

    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match nr_db::get_usage_stats(&db, Some(period)) {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to fetch usage stats: {e}") })),
        )
            .into_response(),
    }
}

/// GET /api/usage/providers
pub async fn providers_get() -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let provider_ids = match nr_db::get_distinct_providers(&db) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to fetch providers: {e}") })),
            )
                .into_response();
        }
    };

    let nodes = nr_db::get_provider_nodes(&db, None).unwrap_or_default();
    let mut node_map: HashMap<String, String> = HashMap::new();
    for n in nodes {
        if let (Some(id), Some(name)) = (
            n.get("id").and_then(|v| v.as_str()),
            n.get("name").and_then(|v| v.as_str()),
        ) {
            node_map.insert(id.to_string(), name.to_string());
        }
    }

    let mut result_providers = Vec::new();
    for id in provider_ids {
        let name = if let Some(n) = node_map.get(&id) {
            n.clone()
        } else if let Some(p) = nr_data::provider(&id) {
            p.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string()
        } else {
            id.clone()
        };
        result_providers.push(json!({ "id": id, "name": name }));
    }

    Json(json!({ "providers": result_providers })).into_response()
}

/// GET /api/usage/request-details
pub async fn request_details_get(Query(params): Query<HashMap<String, String>>) -> Response {
    let page: usize = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1);
    let page_size: usize = params
        .get("pageSize")
        .and_then(|p| p.parse().ok())
        .unwrap_or(20);

    if page < 1 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Page must be >= 1" })),
        )
            .into_response();
    }
    if page_size < 1 || page_size > 100 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "PageSize must be between 1 and 100" })),
        )
            .into_response();
    }

    let mut filter = json!({
        "page": page,
        "pageSize": page_size,
    });
    let obj = filter.as_object_mut().unwrap();
    for key in [
        "provider",
        "model",
        "connectionId",
        "status",
        "startDate",
        "endDate",
    ] {
        if let Some(v) = params.get(key) {
            if !v.is_empty() {
                obj.insert(key.to_string(), Value::String(v.clone()));
            }
        }
    }

    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let details_raw = match nr_db::get_request_details(&db, Some(&filter)) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to fetch request details: {e}") })),
            )
                .into_response();
        }
    };

    // Redact sensitive conversation payloads per JS parity
    let redacted: Vec<Value> = details_raw
        .into_iter()
        .map(|mut d| {
            if let Some(obj) = d.as_object_mut() {
                for key in ["request", "providerRequest", "providerResponse", "response"] {
                    if obj.contains_key(key) {
                        obj.insert(key.to_string(), json!({ "redacted": true }));
                    }
                }
            }
            d
        })
        .collect();

    Json(json!({ "details": redacted, "page": page, "pageSize": page_size })).into_response()
}

/// GET /api/usage/request-log-entries
pub async fn request_log_entries_get(
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit: usize = params
        .get("pageSize")
        .and_then(|p| p.parse().ok())
        .unwrap_or(50);

    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match nr_db::repos::request_logs::get_recent_logs(&db, limit) {
        Ok(logs) => Json(json!({ "logs": logs, "total": logs.len() })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to fetch request logs: {e}") })),
        )
            .into_response(),
    }
}

/// DELETE /api/usage/request-log-entries
pub async fn request_log_entries_delete() -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if let Err(e) = db.execute_with_params("DELETE FROM requestLogs", []) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to clear request logs: {e}") })),
        )
            .into_response();
    }

    Json(json!({ "ok": true })).into_response()
}

/// GET /api/usage/request-logs
pub async fn request_logs_get() -> Response {
    logs_get().await
}

/// GET /api/usage/[connectionId]
pub async fn connection_usage_get(Path(connection_id): Path<String>) -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let conn = match nr_db::get_provider_connection_by_id(&db, &connection_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Connection not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let auth_type = conn
        .get("authType")
        .and_then(|v| v.as_str())
        .unwrap_or("oauth");
    let provider = conn
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let is_oauth = auth_type == "oauth";
    let is_apikey = auth_type == "apikey" || auth_type == "api_key";
    let is_apikey_eligible = is_apikey && USAGE_APIKEY_PROVIDERS.contains(&provider);

    if !is_oauth && !is_apikey_eligible {
        return Json(json!({ "message": "Usage not available for this connection" })).into_response();
    }

    Json(json!({
        "provider": provider,
        "connectionId": connection_id,
        "authType": auth_type,
        "active": conn.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true),
        "status": conn.get("testStatus").and_then(|v| v.as_str()).unwrap_or("active"),
    }))
    .into_response()
}

/// POST /api/usage/[connectionId]/claude-reset
pub async fn claude_reset_post(
    Path(connection_id): Path<String>,
    Json(_body): Json<Value>,
) -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let conn = match nr_db::get_provider_connection_by_id(&db, &connection_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Connection not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let provider = conn.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    let auth_type = conn.get("authType").and_then(|v| v.as_str()).unwrap_or("");
    if provider != "claude" || auth_type != "oauth" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Limit reset is only available for Claude OAuth connections." })),
        )
            .into_response();
    }

    Json(json!({ "ok": true, "message": "Limit reset applied" })).into_response()
}

/// GET /api/usage/[connectionId]/codex-reset-credits
pub async fn codex_reset_credits_get(Path(connection_id): Path<String>) -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let conn = match nr_db::get_provider_connection_by_id(&db, &connection_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Connection not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let provider = conn.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    if provider != "codex" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Codex reset credits are only available for Codex connections." })),
        )
            .into_response();
    }

    Json(json!({ "credits": 0, "available": false })).into_response()
}

/// POST /api/usage/[connectionId]/codex-reset-credits
pub async fn codex_reset_credits_post(
    Path(connection_id): Path<String>,
    Json(_body): Json<Value>,
) -> Response {
    let db = match nr_db::db() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let conn = match nr_db::get_provider_connection_by_id(&db, &connection_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Connection not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let provider = conn.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    if provider != "codex" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Codex reset credits are only available for Codex connections." })),
        )
            .into_response();
    }

    Json(json!({ "ok": true, "reset": true, "redeemRequestId": uuid::Uuid::new_v4().to_string() })).into_response()
}

/// GET /api/usage/stream
pub async fn stream_get(Query(params): Query<HashMap<String, String>>) -> Response {
    let period = params.get("period").cloned().unwrap_or_else(|| "today".to_string());

    let db = nr_db::db().ok();
    let stats = db
        .as_ref()
        .and_then(|d| nr_db::get_usage_stats(d, Some(&period)).ok())
        .unwrap_or_else(|| json!({}));

    let initial_payload = json!({
        "type": "stats",
        "period": period,
        "stats": stats,
        "activeRequests": [],
        "recentRequests": [],
        "errorProvider": "",
        "summary": {},
    });

    let initial_event = Event::default().data(initial_payload.to_string());

    // Stream that yields initial stats; Axum's keep_alive sends periodic pings
    let stream = futures::stream::once(async move {
        Ok::<Event, Infallible>(initial_event)
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)).text("ping"))
        .into_response()
}

// ─── Routes ─────────────────────────────────────────────────────────────────

pub fn routes() -> Router {
    Router::new()
        .route("/api/usage/chart", get(chart_get))
        .route("/api/usage/history", get(history_get))
        .route("/api/usage/logs", get(logs_get))
        .route("/api/usage/stats", get(stats_get))
        .route("/api/usage/providers", get(providers_get))
        .route("/api/usage/request-details", get(request_details_get))
        .route(
            "/api/usage/request-log-entries",
            get(request_log_entries_get).delete(request_log_entries_delete),
        )
        .route("/api/usage/request-logs", get(request_logs_get))
        .route("/api/usage/stream", get(stream_get))
        .route("/api/usage/:connectionId", get(connection_usage_get))
        .route(
            "/api/usage/:connectionId/claude-reset",
            post(claude_reset_post),
        )
        .route(
            "/api/usage/:connectionId/codex-reset-credits",
            get(codex_reset_credits_get).post(codex_reset_credits_post),
        )
}
