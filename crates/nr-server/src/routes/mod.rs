//! Dashboard-side route modules. The coordinator merges the single
//! [`routes()`] router (auth, cli-tools, oauth flows, usage) with the
//! gateway's own `/v1`+`/v1beta` router.

pub mod api;
pub mod auth;
pub mod cli_tools;
pub mod headroom;
pub mod mcp;
pub mod oauth_flow;
pub mod oauth_imports;
pub mod oauth_jwt;
pub mod oauth_local_creds;
pub mod pxpipe;
pub mod translator;
pub mod tunnel;
pub mod usage;

use axum::Router;

pub fn routes() -> Router {
    Router::new()
        .merge(auth::routes())
        .merge(cli_tools::routes())
        .merge(oauth_flow::routes())
        .merge(oauth_imports::routes())
        .merge(pxpipe::routes())
        .merge(headroom::routes())
        .merge(translator::routes())
        .merge(tunnel::routes())
        .merge(mcp::routes())
        .merge(usage::routes())
}
