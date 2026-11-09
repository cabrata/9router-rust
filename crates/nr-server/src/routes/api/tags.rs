use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::common::cors_headers;

pub async fn get_tags() -> Response {
    let models = nr_data::export("open-sse/config/ollamaModels", "ollamaModels")
        .cloned()
        .unwrap_or_else(|| json!({ "models": [] }));

    let mut resp = (StatusCode::OK, Json(models)).into_response();
    resp.headers_mut().extend(cors_headers());
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

pub async fn options_tags() -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().extend(cors_headers());
    resp
}
