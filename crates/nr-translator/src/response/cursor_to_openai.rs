//! Cursor to OpenAI response translator (passthrough).
//! CursorExecutor already emits OpenAI-format chunks.

use nr_core::Json;
use crate::state::StreamState;

/// Cursor responses are already in OpenAI format from the executor.
pub fn cursor_to_openai_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
    if let Some(chunk) = _chunk {
        return vec![chunk.clone()];
    }
    Vec::new()
}
