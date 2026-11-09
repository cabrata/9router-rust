//! Response translators. Port of open-sse/translator/response/.

use crate::Registry;

pub mod claude_to_openai;
pub mod cursor_to_openai;
pub mod openai_to_claude;
pub mod gemini_to_openai;

pub mod openai_to_antigravity {
    use nr_core::Json;
    use crate::StreamState;
    pub fn openai_to_antigravity_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
        // TODO: implement
        if let Some(chunk) = _chunk { vec![chunk.clone()] } else { Vec::new() }
    }
}

pub mod openai_responses {
    use nr_core::Json;
    use crate::StreamState;
    pub fn openai_responses_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
        // TODO: implement - converts OpenAI Responses API format
        if let Some(chunk) = _chunk { vec![chunk.clone()] } else { Vec::new() }
    }
}

pub mod kiro_to_openai {
    use nr_core::Json;
    use crate::StreamState;
    pub fn kiro_to_openai_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
        // TODO: implement
        if let Some(chunk) = _chunk { vec![chunk.clone()] } else { Vec::new() }
    }
}

pub mod ollama_to_openai {
    use nr_core::Json;
    use crate::StreamState;
    pub fn ollama_to_openai_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
        // TODO: implement
        if let Some(chunk) = _chunk { vec![chunk.clone()] } else { Vec::new() }
    }
}

pub mod commandcode_to_openai {
    use nr_core::Json;
    use crate::StreamState;
    pub fn commandcode_to_openai_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
        // TODO: implement
        if let Some(chunk) = _chunk { vec![chunk.clone()] } else { Vec::new() }
    }
}

pub mod kiro_to_claude {
    use nr_core::Json;
    use crate::StreamState;
    pub fn kiro_to_claude_response(_chunk: Option<&Json>, _state: &mut StreamState) -> Vec<Json> {
        // TODO: implement direct route
        if let Some(chunk) = _chunk { vec![chunk.clone()] } else { Vec::new() }
    }
}

/// Register every response translator (JS side-effect imports in translator/index.js).
pub fn register_all(reg: &mut Registry) {
    // claude-to-openai
    reg.register("claude", "openai", None, Some(|chunk, state| claude_to_openai::claude_to_openai_response(chunk, state)));

    // openai-to-claude
    reg.register("openai", "claude", None, Some(|chunk, state| openai_to_claude::openai_to_claude_response(chunk, state)));

    // gemini-to-openai
    reg.register("gemini", "openai", None, Some(|chunk, state| gemini_to_openai::gemini_to_openai_response(chunk, state)));
    reg.register("gemini-cli", "openai", None, Some(|chunk, state| gemini_to_openai::gemini_to_openai_response(chunk, state)));
    reg.register("antigravity", "openai", None, Some(|chunk, state| gemini_to_openai::gemini_to_openai_response(chunk, state)));
    reg.register("vertex", "openai", None, Some(|chunk, state| gemini_to_openai::gemini_to_openai_response(chunk, state)));

    // openai-to-antigravity
    reg.register("openai", "antigravity", None, Some(openai_to_antigravity::openai_to_antigravity_response));

    // openai-responses
    reg.register("openai-responses", "openai", None, Some(openai_responses::openai_responses_response));

    // kiro-to-openai
    reg.register("kiro", "openai", None, Some(kiro_to_openai::kiro_to_openai_response));

    // cursor-to-openai: passthrough
    reg.register("cursor", "openai", None, Some(cursor_to_openai::cursor_to_openai_response));

    // ollama-to-openai
    reg.register("ollama", "openai", None, Some(ollama_to_openai::ollama_to_openai_response));

    // commandcode-to-openai
    reg.register("commandcode", "openai", None, Some(commandcode_to_openai::commandcode_to_openai_response));

    // kiro-to-claude (direct route)
    reg.register("kiro", "claude", None, Some(kiro_to_claude::kiro_to_claude_response));
}
