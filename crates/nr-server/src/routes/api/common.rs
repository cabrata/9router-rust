use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::Db;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub fn err_json(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub fn bad_request(msg: impl Into<String>) -> Response {
    err_json(StatusCode::BAD_REQUEST, msg)
}

pub fn unauthorized(msg: impl Into<String>) -> Response {
    err_json(StatusCode::UNAUTHORIZED, msg)
}

pub fn forbidden(msg: impl Into<String>) -> Response {
    err_json(StatusCode::FORBIDDEN, msg)
}

pub fn not_found(msg: impl Into<String>) -> Response {
    err_json(StatusCode::NOT_FOUND, msg)
}

pub fn conflict(msg: impl Into<String>) -> Response {
    err_json(StatusCode::CONFLICT, msg)
}

pub fn internal_error(msg: impl Into<String>) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

pub fn get_db(ext: Option<axum::extract::Extension<Db>>) -> Result<Db, Response> {
    if let Some(axum::extract::Extension(db)) = ext {
        return Ok(db);
    }
    nr_db::db().map_err(|e| internal_error(format!("Database error: {e}")))
}

pub fn cors_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    headers
}

/// Generate consistent machine ID following src/shared/utils/machineId.js
pub fn get_consistent_machine_id(salt: Option<&str>) -> String {
    let salt_value = salt
        .map(String::from)
        .or_else(|| std::env::var("MACHINE_ID_SALT").ok())
        .unwrap_or_else(|| "endpoint-proxy-salt".to_string());

    let raw = load_raw_machine_id();
    let mut hasher = Sha256::new();
    hasher.update(format!("{raw}{salt_value}").as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    hex[..16.min(hex.len())].to_string()
}

fn load_raw_machine_id() -> String {
    let machine_id_file: PathBuf = nr_core::paths::data_dir().join("machine-id");
    if let Ok(content) = std::fs::read_to_string(&machine_id_file) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let generated = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::create_dir_all(nr_core::paths::data_dir());
    let _ = std::fs::write(&machine_id_file, &generated);
    generated
}

/// JSON object checker
pub fn is_json_object(val: &Value) -> bool {
    matches!(val, Value::Object(_))
}
