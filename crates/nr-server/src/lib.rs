//! HTTP server: the `/v1` gateway, the `/api` management API and the
//! dashboard UI (maud + htmx). Port of src/app, the rest of src/lib,
//! src/mitm and custom-server.js. Wave 2/3 (see AGENTS.md).
//!
//! FROZEN for the CLI: [`ServerOptions`] and [`run`].

pub mod console_log_buffer;
pub mod dashboard;
pub mod mitm;
pub mod routes;
pub mod support;
pub mod v1;

pub use routes::api::api_routes;

use std::net::SocketAddr;

use axum::Extension;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

/// Where to listen. Defaults follow the JS build: `PORT` (20128) and `HOSTNAME`.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub port: u16,
    pub hostname: String,
}

impl Default for ServerOptions {
    fn default() -> Self {
        let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(20128);
        let hostname = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "0.0.0.0".into());
        Self { port, hostname }
    }
}

/// The application router. Mounts `/v1`, `/api` and the HTML dashboard.
///
/// The `Db` is shared via `Extension` — route handlers take
/// `Extension<Db>` so tests can swap in a fresh temp-dir database.
pub fn app() -> Router {
    let db = nr_db::db().expect("nr-server: cannot open default database");
    Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .merge(v1::v1_router())
        .merge(routes::routes())
        .merge(api_routes())
        .merge(dashboard::router())
        .layer(axum::Extension(db))
}

/// Serve until shutdown (Ctrl-C / SIGTERM). Frozen contract for nr-cli.
pub async fn run(opts: ServerOptions) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind((opts.hostname.as_str(), opts.port)).await?;
    tracing::info!("9router listening on http://{}:{}", opts.hostname, opts.port);
    axum::serve(listener, app().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
