use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::common::cors_headers;

pub async fn get_health() -> Response {
    let mut resp = (StatusCode::OK, Json(json!({ "ok": true }))).into_response();
    resp.headers_mut().extend(cors_headers());
    resp
}

pub async fn options_health() -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().extend(cors_headers());
    resp
}
