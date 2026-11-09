use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub async fn post_shutdown(headers: HeaderMap) -> Response {
    if std::env::var("NODE_ENV").as_deref() == Ok("production") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Not allowed in production"
            })),
        )
            .into_response();
    }

    let secret = std::env::var("SHUTDOWN_SECRET").unwrap_or_default();
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if secret.is_empty() || auth_header != format!("Bearer {secret}") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "success": false,
                "message": "Unauthorized"
            })),
        )
            .into_response();
    }

    // Schedule exit
    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        // `killAllBridges()` runs in the JS signal cleanup; without it a
        // spawned stdio MCP child would outlive the server.
        crate::routes::mcp::kill_all_bridges();
        std::process::exit(0);
    });

    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "message": "Shutting down..."
        })),
    )
        .into_response()
}
