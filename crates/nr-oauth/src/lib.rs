//! OAuth: token refresh used by executors and the engine, plus the
//! dashboard-side authorization flows. Owner: oauth agent (see AGENTS.md).
//!
//! - [`token_refresh`]: open-sse/services/tokenRefresh.js + tokenRefresh/*.js
//! - [`credential_manager`]: open-sse/services/oauthCredentialManager.js
//! - [`flows`]: src/lib/oauth/** (authorize URL, code exchange, device code, polling)

pub mod credential_manager;
pub mod flows;
pub mod token_refresh;
