use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn get_version() -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "currentVersion": CURRENT_VERSION,
            "latestVersion": Value::Null,
            "hasUpdate": false,
        })),
    )
        .into_response()
}

use serde_json::Value;

pub async fn post_version_shutdown() -> Response {
    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        std::process::exit(0);
    });

    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "message": "Shutting down for manual update..."
        })),
    )
        .into_response()
}

pub async fn post_version_update() -> Response {
    if std::env::var("NODE_ENV").as_deref() != Ok("production") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Update is only available in production build (9router CLI)"
            })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "message": "Updater started. This app will exit shortly."
        })),
    )
        .into_response()
}
