//! Route modules for nr-server.

pub mod auth;

use axum::Router;

pub fn routes() -> Router {
    Router::new().merge(auth::routes())
}
