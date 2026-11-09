use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub async fn get_init() -> Response {
    (StatusCode::OK, "Initialized").into_response()
}
