use nr_core::Json;
use serde_json::json;

/// Encode JSON data as SSE chunk.
pub fn sse_chunk(data: &Json) -> String {
    format!("data: {}\n\n", data.to_string())
}

/// Build OpenAI chat.completion.chunk SSE frame. Key order: id, object, created, model, choices.
pub fn chat_chunk_sse(
    id: &str,
    created: i64,
    model: &str,
    delta: &Json,
    finish_reason: Option<&str>,
) -> String {
    let choices = json!([{
        "index": 0,
        "delta": delta,
        "finish_reason": finish_reason
    }]);

    let data = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": choices
    });

    sse_chunk(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_chunk_format() {
        let data = json!({"text": "hello"});
        let chunk = sse_chunk(&data);
        assert!(chunk.starts_with("data: "));
        assert!(chunk.ends_with("\n\n"));
    }

    #[test]
    fn test_chat_chunk_sse_format() {
        let delta = json!({"role": "assistant", "content": "test"});
        let chunk = chat_chunk_sse("id-123", 1234567890, "gpt-4", &delta, Some("stop"));
        assert!(chunk.contains("\"object\":\"chat.completion.chunk\""));
        assert!(chunk.contains("\"finish_reason\":\"stop\""));
        assert!(chunk.contains("\"index\":0"));
    }
}
