//! Dashboard pages: endpoint, usage, quota, request-logs, console-log, profile. Absolute paths (`/dashboard/...`).
use axum::Router;

pub fn router() -> Router {
    Router::new()
}
