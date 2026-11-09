//! Copilot interception: replace the model and forward to the matching router path.
//! Port of src/mitm/handlers/copilot.js.

use axum::response::Response;
use http::StatusCode;

use super::{base, InterceptCtx};
use crate::mitm::logger;

/// Copilot endpoint → 9Router path. First substring match wins, same as JS
/// object-iteration order.
const URL_MAP: &[(&str, &str)] = &[
    ("/chat/completions", "/v1/chat/completions"),
    ("/v1/messages", "/v1/messages"),
    ("/responses", "/v1/responses"),
];

fn resolve_router_path(request_url: &str) -> &'static str {
    for (pattern, router_path) in URL_MAP {
        if request_url.contains(pattern) {
            return router_path;
        }
    }
    "/v1/chat/completions"
}

pub async fn intercept(
    path_and_query: &str,
    body: &[u8],
    ctx: &InterceptCtx,
) -> nr_core::Result<Response> {
    let result = async {
        let mut parsed: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| nr_core::Error::other(format!("copilot body: {e}")))?;
        // Copilot always carries a model; the mapped alias is authoritative.
        if let Some(model) = ctx.model.as_deref() {
            parsed["model"] = serde_json::Value::String(model.into());
        }
        let router_path = resolve_router_path(path_and_query);
        let upstream = base::fetch_router(
            &ctx.router_base,
            router_path,
            ctx.api_key.as_deref(),
            &parsed,
            &ctx.client_headers,
        )
        .await?;
        Ok::<_, nr_core::Error>(base::stream_through(upstream, None))
    }
    .await;

    match result {
        Ok(response) => Ok(response),
        Err(e) => {
            logger::err(&format!("[copilot] {e}"));
            Ok(base::error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_copilot_paths_to_router_paths() {
        assert_eq!(resolve_router_path("/chat/completions"), "/v1/chat/completions");
        assert_eq!(resolve_router_path("/v1/messages?beta=true"), "/v1/messages");
        assert_eq!(resolve_router_path("/responses"), "/v1/responses");
        assert_eq!(resolve_router_path("/something/else"), "/v1/chat/completions");
    }
}
