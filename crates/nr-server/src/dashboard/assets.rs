//! Embedded static assets for the dashboard (htmx, CSS).
//!
//! Files under `crates/nr-server/static/` are compiled in with `rust-embed`
//! so the binary needs no external asset directory at runtime.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static/"]
struct StaticDir;

/// Inline favicon (the JS build ships `public/favicon.svg`).
pub const FAVICON: &str = include_str!("../../static/favicon.svg");

/// GET /static/{path}
pub async fn static_handler(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let Some(file) = StaticDir::get(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = match path.rsplit_once('.') {
        Some((_, "css")) => "text/css; charset=utf-8",
        Some((_, "js")) => "application/javascript; charset=utf-8",
        Some((_, "svg")) => "image/svg+xml",
        Some((_, "png")) => "image/png",
        Some((_, "ico")) => "image/x-icon",
        _ => "application/octet-stream",
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
        ],
        file.data,
    )
        .into_response()
}
