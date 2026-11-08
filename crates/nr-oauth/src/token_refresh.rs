//! Port of open-sse/services/tokenRefresh.js and tokenRefresh/{providers,dedup}.js.
//!
//! FROZEN signatures (executor crates call them while this is being ported).
//! Each mirrors the JS function of the same name; JS returns the refreshed
//! fields (`accessToken`, `refreshToken`, `expiresIn`/`expiresAt`, …) or `null`.
//! Wave-0 stubs return `None` until the oauth agent ports them.

use nr_core::http::ProxyOptions;
use nr_core::{Credentials, Json, JsonMap, Logger};

/// JS `parseVertexSaJson(apiKey)`: the service-account JSON, or `None`.
pub fn parse_vertex_sa_json(_api_key: &str) -> Option<Json> {
    None
}

/// JS `refreshVertexToken(saJson, log)`.
pub async fn refresh_vertex_token(_sa_json: &Json, _log: &Logger) -> Option<JsonMap> {
    None
}

/// JS `refreshGoogleToken(refreshToken, clientId, clientSecret, log)`.
pub async fn refresh_google_token(_refresh_token: &str, _client_id: &str, _client_secret: &str, _log: &Logger) -> Option<JsonMap> {
    None
}

/// JS `refreshKiroToken(refreshToken, providerSpecificData, log, proxyOptions)`.
pub async fn refresh_kiro_token(_refresh_token: &str, _provider_specific_data: Option<&JsonMap>, _log: &Logger, _proxy: Option<&ProxyOptions>) -> Option<JsonMap> {
    None
}

/// JS `refreshMiniMaxCodeToken(provider, refreshToken, log)`.
pub async fn refresh_minimax_code_token(_provider: &str, _refresh_token: &str, _log: &Logger) -> Option<JsonMap> {
    None
}

/// JS `refreshTokenByProvider(provider, credentials, log)`.
pub async fn refresh_token_by_provider(_provider: &str, _creds: &Credentials, _log: &Logger) -> Option<JsonMap> {
    None
}
