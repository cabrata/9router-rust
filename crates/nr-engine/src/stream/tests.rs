//! Stream transform tests: SSE framing, translation and terminal frames.

use super::*;
use nr_core::http::UpstreamResponse;
use std::pin::Pin;

fn sse_body(frames: &[&str]) -> nr_core::http::BodyStream {
    let text: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    UpstreamResponse::from_bytes(200, Default::default(), text).body
}

/// Drain the client stream. Must be awaited on the tokio runtime: the pump
/// task and its tokio mpsc channel need a live reactor, so
/// `futures::executor::block_on` here would deadlock the test.
async fn collect(
    mut s: Pin<Box<dyn futures::Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>>,
) -> String {
    let mut out = String::new();
    while let Some(c) = s.next().await {
        out.push_str(&String::from_utf8_lossy(&c.unwrap()));
    }
    out
}

#[test]
fn format_sse_openai_frame() {
    let data = serde_json::json!({"id":"1","choices":[]});
    assert_eq!(format_sse(&data, "openai"), "data: {\"id\":\"1\",\"choices\":[]}\n\n");
}

#[test]
fn format_sse_claude_uses_named_events() {
    let data = serde_json::json!({"type":"content_block_delta","delta":{}});
    assert_eq!(
        format_sse(&data, "claude"),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{}}\n\n"
    );
}

#[test]
fn format_sse_responses_event_frame() {
    let data = serde_json::json!({"event":"response.done","data":{"x":1}});
    assert_eq!(format_sse(&data, "openai-responses"), "event: response.done\ndata: {\"x\":1}\n\n");
}

#[test]
fn stream_error_bytes_claude() {
    let b = build_stream_error_bytes(503, "boom", "claude");
    let s = String::from_utf8(b.to_vec()).unwrap();
    assert!(s.starts_with("event: error\ndata: "));
    assert!(s.contains("\"message\":\"boom\""));
}

#[test]
fn stream_error_bytes_openai_includes_done() {
    let b = build_stream_error_bytes(502, "boom", "openai");
    let s = String::from_utf8(b.to_vec()).unwrap();
    assert!(s.contains("\"error\""));
    assert!(s.ends_with("data: [DONE]\n\n"));
}

#[test]
fn parse_line_handles_done_and_payload() {
    assert_eq!(parse_upstream_data_line("data: [DONE]"), None);
    assert_eq!(
        parse_upstream_data_line("data: {\"a\":1}"),
        Some(serde_json::json!({"a":1}))
    );
    assert_eq!(parse_upstream_data_line("not data"), None);
}

#[tokio::test]
async fn openai_to_openai_passthrough_emits_frames_and_done() {
    let upstream = sse_body(&["{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"]);
    let client = transform_sse_to_client(upstream, "openai", "openai", tokio_util::sync::CancellationToken::new());
    let out = collect(client).await;
    assert!(out.contains("\"content\":\"hi\""));
    assert!(out.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn openai_to_claude_translation_emits_named_events() {
    let upstream = sse_body(&["{\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hey\"}}]}"]);
    let client = transform_sse_to_client(upstream, "openai", "claude", tokio_util::sync::CancellationToken::new());
    let out = collect(client).await;
    assert!(out.contains("event: content_block_delta"), "got: {out}");
    assert!(!out.contains("data: [DONE]"), "claude has no [DONE]");
}

#[tokio::test]
async fn abort_emits_terminal_error_frame() {
    // Upstream that never finishes; cancel mid-stream and expect a terminal frame.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<std::result::Result<Bytes, std::io::Error>>();
    let body = Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let cancel = tokio_util::sync::CancellationToken::new();
    let client = transform_sse_to_client(Box::pin(body), "openai", "openai", cancel.clone());

    tx.send(Ok(Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n"))).unwrap();
    // Give the pump a moment, then abort.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let out = collect(client).await;
    assert!(out.contains("\"content\":\"a\""), "got: {out}");
    // The frame is the OpenAI error envelope (JS `buildErrorBody`): type/code
    // carry the classification, the numeric status is not part of the body.
    assert!(out.contains("aborted"), "terminal frame after abort: {out}");
    assert!(out.contains("\"code\":\"gateway_timeout\""), "504 mapping: {out}");
    assert!(out.ends_with("\n\n"));
}
