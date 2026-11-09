//! Streaming plumbing: upstream SSE parsing, translation into the client wire
//! format, and frame serialization. Port of open-sse/utils/stream.js +
//! open-sse/utils/streamHelpers.js `formatSSE`.

use bytes::Bytes;
use futures::StreamExt;
use nr_common::sse_constants::SSE_DONE;
use nr_core::{Json, Result};
use nr_translator::init_state;
use nr_translator::state::StreamState;

use crate::translate::translate_chunk_to_client;

/// Serialize one translated client chunk into SSE bytes (JS `formatSSE`).
/// - Responses-API frames (`{ event, data }`) → named `event:` + `data:`.
/// - Claude frames (`{ type }`) → `event: <type>` + `data:`.
/// - Everything else → `data: <json>`.
pub fn format_sse(data: &Json, source_format: &str) -> String {
    if let (Some(event), Some(inner)) = (
        data.get("event").and_then(Json::as_str),
        data.get("data"),
    ) {
        return format!("event: {event}\ndata: {inner}\n\n");
    }
    if source_format == "claude" {
        if let Some(ty) = data.get("type").and_then(Json::as_str) {
            return format!("event: {ty}\ndata: {data}\n\n");
        }
    }
    format!("data: {data}\n\n")
}

/// Terminal bytes for a stream that aborts after HTTP 200 was already sent
/// (JS `buildStreamErrorBytes`). Claude clients get `event: error`; every other
/// client gets the OpenAI error frame + `[DONE]`.
pub fn build_stream_error_bytes(status: u16, message: &str, client_format: &str) -> Bytes {
    let err = json_error(status, message);
    if client_format == "claude" {
        let frame = serde_json::json!({ "type": "error", "error": err });
        Bytes::from(format_sse(&frame, "claude"))
    } else {
        let frame = serde_json::json!({ "error": err });
        Bytes::from(format!("{}{}", format_sse(&frame, client_format), SSE_DONE))
    }
}

/// OpenAI-style error body (`{ error: { message, type, code } }`).
pub fn json_error(status: u16, message: &str) -> Json {
    let (ty, code) = match status {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("billing_error", "payment_required"),
        403 => ("permission_error", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        406 => ("invalid_request_error", "model_not_supported"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        _ => ("server_error", "internal_server_error"),
    };
    serde_json::json!({ "message": message, "type": ty, "code": code })
}

/// Extract the JSON payload of one upstream SSE `data:` line.
/// `data: [DONE]` → `None` (end of stream sentinel).
pub fn parse_upstream_data_line(line: &str) -> Option<Json> {
    let payload = line.strip_prefix("data:").map(str::trim_start)?;
    if payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Collect SSE byte chunks into logical `data:` JSON payloads, calling `f`
/// per payload. Returns when the upstream closes.
pub async fn pump_sse<F>(body: nr_core::http::BodyStream, mut on_data: F) -> Result<()>
where
    F: FnMut(Json) -> Result<()> + Send,
{
    let mut stream = body;
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(nr_core::Error::from)?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].to_string();
            buffer.drain(..=pos);
            let line = line.trim_end_matches('\r');
            if !line.starts_with("data:") {
                continue;
            }
            if let Some(payload) = parse_upstream_data_line(line) {
                on_data(payload)?;
            }
        }
    }
    Ok(())
}

/// Pipe an upstream SSE body through the response translator into the client
/// wire format, emitting `data:` frames. Returns the client-facing body stream.
///
/// `upstream_format` is the format the executor produced (`ExecResult::response_format`
/// or the target format); `client_format` is the source format the client sent.
///
/// On client abort after headers were sent, JS emits a terminal error frame
/// (`onAbortTerminal`) so the client sees a real error instead of a truncated
/// stream; this does the same.
pub fn transform_sse_to_client(
    body: nr_core::http::BodyStream,
    upstream_format: &'static str,
    client_format: &'static str,
    client_abort: tokio_util::sync::CancellationToken,
) -> nr_core::http::BodyStream {
    let state: std::sync::Arc<std::sync::Mutex<StreamState>> =
        std::sync::Arc::new(std::sync::Mutex::new(init_state(client_format)));

    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);

    tokio::spawn(async move {
        let mut upstream = body;
        let mut aborted = false;
        // Small line buffer: upstream frames may be split across TCP chunks.
        let mut line_buf = String::new();

        macro_rules! send {
            ($item:expr) => {{
                if tx.send($item).await.is_err() {
                    return; // receiver dropped (client gone)
                }
            }};
        }

        loop {
            tokio::select! {
                _ = client_abort.cancelled() => {
                    aborted = true;
                    break;
                }
                chunk = upstream.next() => {
                    let Some(chunk) = chunk else { break };
                    let bytes = match chunk {
                        Ok(b) => b,
                        Err(e) => {
                            send!(Err(std::io::Error::other(e.to_string())));
                            return;
                        }
                    };
                    line_buf.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = line_buf.find('\n') {
                        let line = line_buf[..pos].trim_end_matches('\r').to_string();
                        line_buf.drain(..=pos);
                        if !line.starts_with("data:") {
                            continue;
                        }
                        let payload = line.strip_prefix("data:").map(str::trim_start).unwrap_or("");
                        if payload == "[DONE]" {
                            // The sentinel is consumed here, never forwarded; the
                            // terminal frame is synthesized below (JS passthrough
                            // does the same: forward, then append `data: [DONE]`).
                            continue;
                        }
                        let Ok(parsed) = serde_json::from_str::<Json>(payload) else { continue };
                        // Scope the guard: `MutexGuard` is not `Send`, so it must not
                        // be live across the `send!` await points below.
                        let translated = {
                            let mut st = state.lock().expect("stream state lock poisoned");
                            translate_chunk_to_client(upstream_format, client_format, Some(&parsed), &mut st)
                        };
                        for t in translated {
                            send!(Ok(Bytes::from(format_sse(&t, client_format))));
                        }
                    }
                }
            }
        }

        // End-of-stream flush: translators that defer closing events emit here.
        let flushed = {
            let mut st = state.lock().expect("stream state lock poisoned");
            translate_chunk_to_client(upstream_format, client_format, None, &mut st)
        };
        for t in flushed {
            send!(Ok(Bytes::from(format_sse(&t, client_format))));
        }

        if aborted {
            // HTTP 200 is already on the wire; send a terminal error frame.
            send!(Ok(build_stream_error_bytes(504, "Upstream stream aborted", client_format)));
            return;
        }
        // Terminate the stream: some clients (OpenClaw) hang without the OpenAI
        // sentinel. Claude closes on the translator's message_stop instead, and
        // Gemini-family clients reject the sentinel outright.
        if client_format != "claude" {
            send!(Ok(Bytes::from_static(SSE_DONE.as_bytes())));
        }
    });

    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

#[cfg(test)]
mod tests;
