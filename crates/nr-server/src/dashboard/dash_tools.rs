//! Dashboard pages: cli-tools, mitm, translator, token-saver, pxpipe, skills, basic-chat. Absolute paths (`/dashboard/...`).
use axum::Router;

pub fn router() -> Router {
    Router::new()
}
