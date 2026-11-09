//! Antigravity interception: forward the Gemini body as-is to the router.
//! Port of src/mitm/handlers/antigravity.js.
//!
//! The router auto-detects the format from `body.userAgent === "antigravity"` +
//! `body.request.contents` and runs
//! antigravity→openai→provider→openai→antigravity internally, so this handler
//! only swaps the model and streams the reply back.

use axum::response::Response;
use http::StatusCode;

use super::{base, InterceptCtx};
use crate::mitm::logger;

pub async fn intercept(
    path_and_query: &str,
    body: &[u8],
    ctx: &InterceptCtx,
) -> nr_core::Result<Response> {
    let is_stream = path_and_query.contains(":streamGenerateContent");
    let mut dumper = logger::ResponseDumper::new(path_and_query, "antigravity", "intercept-antigravity");

    let result = async {
        let mut parsed: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| nr_core::Error::other(format!("antigravity body: {e}")))?;
        if let Some(model) = ctx.model.as_deref() {
            parsed["model"] = serde_json::Value::String(model.into());
        }
        let upstream = base::fetch_router(
            &ctx.router_base,
            "/v1/chat/completions",
            ctx.api_key.as_deref(),
            &parsed,
            &ctx.client_headers,
        )
        .await?;
        if let Some(d) = dumper.as_mut() {
            d.write_header(upstream.status().as_u16(), upstream.headers());
        }
        Ok::<_, nr_core::Error>(base::stream_through(upstream, dumper.take()))
    }
    .await;

    match result {
        Ok(response) => Ok(response),
        Err(e) => {
            logger::err(&format!("[antigravity] {e}"));
            if let Some(d) = dumper {
                d.finish();
            }
            // For the stream endpoint send an SSE error chunk so the SDK doesn't
            // hang waiting for data.
            Ok(if is_stream {
                base::sse_error_response(&e.to_string())
            } else {
                base::error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
            })
        }
    }
}
