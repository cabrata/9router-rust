//! OpenAI chunk building. Port of open-sse/translator/concerns/chunk.js.

use nr_core::Json;
use serde_json::json;

#[derive(Clone, Debug)]
pub struct ChunkOptions {
    pub id: String,
    pub created: i64,
    pub model: String,
}

/// Build OpenAI chat.completion.chunk. Caller supplies id/created/model so each
/// translator keeps its exact id-generation + created semantics (no Date.now here).
pub fn build_chunk(opts: &ChunkOptions, delta: Json, finish_reason: Option<&str>) -> Json {
    json!({
        "id": opts.id,
        "object": "chat.completion.chunk",
        "created": opts.created,
        "model": opts.model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }]
    })
}

/// Convenience wrapper mirroring the JS `buildChunk(id, model, delta, finishReason)` call sites,
/// which derive id/model from the stream state and use "now" for `created`.
pub fn build_chunk_for(id: &str, model: &str, delta: Json, finish_reason: Option<&str>) -> Json {
    let opts = ChunkOptions {
        id: id.to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        model: model.to_string(),
    };
    build_chunk(&opts, delta, finish_reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_chunk() {
        let opts = ChunkOptions {
            id: "test-123".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
        };
        let delta = json!({"role": "assistant", "content": "hello"});
        let chunk = build_chunk(&opts, delta, Some("stop"));

        assert_eq!(chunk.get("id").and_then(|v| v.as_str()), Some("test-123"));
        assert_eq!(chunk.get("model").and_then(|v| v.as_str()), Some("gpt-4"));
        assert_eq!(chunk.get("object").and_then(|v| v.as_str()), Some("chat.completion.chunk"));
        assert_eq!(
            chunk.get("choices")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("finish_reason")),
            Some(&json!("stop"))
        );
    }
}
