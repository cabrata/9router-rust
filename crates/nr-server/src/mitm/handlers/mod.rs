//! Per-provider request/response mutation. Port of src/mitm/handlers/.
//!
//! Each handler takes the intercepted request (path, raw body, context) and
//! returns the response to send back to the IDE. The router does the actual
//! translation, so handlers mostly reshape the model name and stream format.

pub mod antigravity;
pub mod base;
pub mod copilot;
pub mod cursor;
pub mod kiro;

use axum::response::Response;
use http::HeaderMap;

use crate::mitm::config;

/// Everything a handler needs about the intercepted request.
pub struct InterceptCtx {
    /// Tool id: `antigravity` | `copilot` | `kiro` | `cursor`.
    pub tool: String,
    /// Mapped model from the mitmAlias table, if any.
    pub model: Option<String>,
    /// Base URL of the 9Router doing the translation.
    pub router_base: String,
    /// Router API key, when one is configured.
    pub api_key: Option<String>,
    /// Client request headers (already stripped of hop-by-hop before forwarding).
    pub client_headers: HeaderMap,
}

impl InterceptCtx {
    pub fn new(tool: &str, model: Option<String>, client_headers: HeaderMap) -> Self {
        Self {
            tool: tool.to_string(),
            model,
            router_base: base::router_base(),
            api_key: base::router_api_key(),
            client_headers,
        }
    }
}

/// Dispatch to the tool's handler.
pub async fn intercept(
    tool: &str,
    path_and_query: &str,
    body: &[u8],
    ctx: &InterceptCtx,
) -> nr_core::Result<Response> {
    match tool {
        "antigravity" => antigravity::intercept(path_and_query, body, ctx).await,
        "copilot" => copilot::intercept(path_and_query, body, ctx).await,
        "kiro" => kiro::intercept(path_and_query, body, ctx).await,
        "cursor" => cursor::intercept(path_and_query, body, ctx).await,
        other => Err(nr_core::Error::other(format!("no MITM handler for tool {other}"))),
    }
}

/// Resolve the alias-mapped model for `(tool, raw model)`, mirroring JS `getMappedModel`.
///
/// Order: exact alias (after synonym normalization) → prefix match → regex pattern
/// fallback. Returns `None` when the model should pass through untouched.
pub fn mapped_model(tool: &str, model: Option<&str>) -> Option<String> {
    let model = model?;
    let aliases = crate::mitm::db_reader::get_mitm_alias(tool)?;
    let aliases = aliases.as_object()?;
    if aliases.is_empty() {
        return None;
    }

    let lookup = config::normalize_model(tool, model);

    if let Some(target) = aliases.get(&lookup).and_then(|v| v.as_str()) {
        return Some(target.to_string());
    }

    // Prefix match: an alias key that is a prefix of the model (or vice versa)
    // catches version suffixes the synonym table doesn't enumerate.
    for (k, v) in aliases {
        let Some(target) = v.as_str() else { continue };
        if k.is_empty() {
            continue;
        }
        if lookup.starts_with(k.as_str()) || k.starts_with(&lookup) {
            return Some(target.to_string());
        }
    }

    // Pattern fallback: catches renamed variants (e.g. deprecated pro IDs → gemini-pro-agent).
    config::pattern_alias(tool, &lookup).and_then(|alias| {
        aliases
            .get(alias)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_model_means_no_mapping() {
        assert!(mapped_model("antigravity", None).is_none());
    }
}
