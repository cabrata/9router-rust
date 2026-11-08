//! The executor contract between the engine (chatCore) and per-provider
//! executors. Port of the surface of open-sse/executors/base.js that
//! chatCore calls; base-class internals (buildUrl, buildHeaders, retry loop)
//! live in nr-executors.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::http::{HeaderList, ProxyOptions, UpstreamResponse};
use crate::{Credentials, Json, JsonMap, Logger, Result};

/// Arguments of JS `executor.execute({ model, body, stream, credentials, ... })`.
pub struct ExecRequest<'a> {
    pub model: &'a str,
    /// The translated provider-format body.
    pub body: Json,
    pub stream: bool,
    /// Executors may write session/runtime fields back (JS mutates `credentials`).
    pub credentials: &'a mut Credentials,
    pub provider_session_id: Option<String>,
    pub client_tool: Option<String>,
    /// Client disconnect / abort (JS `signal`).
    pub cancel: CancellationToken,
    pub log: &'a Logger,
    pub proxy: ProxyOptions,
    /// User per-provider overrides, e.g. `{ "headers": { ... } }`.
    pub provider_overrides: Option<Json>,
}

/// What JS `execute()` resolves to.
pub struct ExecResult {
    pub response: UpstreamResponse,
    pub url: String,
    pub headers: HeaderList,
    pub transformed_body: Json,
    /// Set when the executor already decoded the upstream into another wire
    /// format (cursor AgentService → "openai"); `None` = the registry format.
    pub response_format: Option<String>,
}

#[async_trait]
pub trait Executor: Send + Sync {
    /// Provider id this instance serves.
    fn provider(&self) -> &str;

    /// Providers that need no credential: chatCore skips refresh on 401/403.
    fn no_auth(&self) -> bool {
        false
    }

    /// Send the request upstream. Errors are transport failures
    /// (`Error::Network` → 502, `Error::Aborted` → 499); an upstream error
    /// status is returned as `Ok` with that status, like JS `fetch`.
    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult>;

    /// JS `refreshCredentials(credentials, log, proxyOptions)`: the fields to
    /// merge into the credentials (`accessToken`, `refreshToken`, `expiresAt`,
    /// …), or `None` when refresh is unsupported or failed.
    async fn refresh_credentials(&self, _creds: &Credentials, _log: &Logger, _proxy: Option<&ProxyOptions>) -> Result<Option<JsonMap>> {
        Ok(None)
    }

    /// JS `needsRefresh(credentials)`.
    fn needs_refresh(&self, _creds: &Credentials) -> bool {
        false
    }

    /// JS `parseError(response, bodyText)` → `(status, message)`.
    fn parse_error(&self, status: u16, body: &str) -> (u16, String) {
        let msg = if body.is_empty() { format!("HTTP {status}") } else { body.to_string() };
        (status, msg)
    }
}
