//! Cursor interception — not implemented, by design.
//! Port of src/mitm/handlers/cursor.js, which answers 501 as well: Cursor's wire
//! format is protobuf, so model extraction/routing needs a decoder we don't have.

use axum::response::Response;
use http::StatusCode;

use super::InterceptCtx;

pub async fn intercept(
    _path_and_query: &str,
    _body: &[u8],
    _ctx: &InterceptCtx,
) -> nr_core::Result<Response> {
    let body = serde_json::json!({
        "error": {
            "message": "Cursor MITM support is coming soon.",
            "type": "not_implemented"
        }
    });
    Ok(Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(axum::body::Body::empty())))
}
