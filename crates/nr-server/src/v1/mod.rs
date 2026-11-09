//! /v1 HTTP gateway: OpenAI-compatible API surface.
//!
//! Port of `src/app/api/v1/**` and `src/app/api/v1beta/**`. The engine work
//! lives in `nr-engine`; routes here do request parsing, key auth and wire
//! shaping. Endpoints are registered in order of specificity so fixed-path
//! routes (e.g. `/models/info`) are not shadowed by the `{provider}/{model}`
//! catch-all.

pub mod chat;
pub mod media;
pub mod models;
pub mod v1beta;

use axum::Router;

pub fn v1_router() -> Router {
    models::routes()
        .merge(v1beta::routes())
        .merge(chat::routes())
        .merge(media::routes())
}
