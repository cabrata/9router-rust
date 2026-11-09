//! Request translators. Port of open-sse/translator/request/.

use crate::Registry;

pub mod openai_to_ollama;
pub mod gemini_to_openai;
pub mod openai_to_claude;
pub mod claude_to_openai;

pub mod openai_to_gemini;
pub mod openai_to_vertex;

pub mod antigravity_to_openai {
    use nr_core::{Credentials, Json};
    use crate::TranslationOptions;
    pub fn antigravity_to_openai_request(_model: &str, body: Json, _stream: bool, _credentials: Option<&mut Credentials>, _options: &TranslationOptions) -> Json {
        // TODO: implement
        body
    }
}

pub mod openai_responses {
    use nr_core::{Credentials, Json};
    use crate::TranslationOptions;
    pub fn openai_responses_request(_model: &str, body: Json, _stream: bool, _credentials: Option<&mut Credentials>, _options: &TranslationOptions) -> Json {
        // TODO: implement - converts OpenAI Responses API format
        body
    }
}

pub mod openai_to_kiro {
    use nr_core::{Credentials, Json};
    use crate::TranslationOptions;
    pub fn openai_to_kiro_request(_model: &str, body: Json, _stream: bool, _credentials: Option<&mut Credentials>, _options: &TranslationOptions) -> Json {
        // TODO: implement
        body
    }
}

pub mod openai_to_cursor {
    use nr_core::{Credentials, Json};
    use crate::TranslationOptions;
    pub fn openai_to_cursor_request(_model: &str, body: Json, _stream: bool, _credentials: Option<&mut Credentials>, _options: &TranslationOptions) -> Json {
        // TODO: implement
        body
    }
}

pub mod openai_to_commandcode {
    use nr_core::{Credentials, Json};
    use crate::TranslationOptions;
    pub fn openai_to_commandcode_request(_model: &str, body: Json, _stream: bool, _credentials: Option<&mut Credentials>, _options: &TranslationOptions) -> Json {
        // TODO: implement
        body
    }
}

pub mod claude_to_kiro {
    use nr_core::{Credentials, Json};
    use crate::TranslationOptions;
    pub fn claude_to_kiro_request(_model: &str, body: Json, _stream: bool, _credentials: Option<&mut Credentials>, _options: &TranslationOptions) -> Json {
        // TODO: implement direct route
        body
    }
}

/// Register every request translator (JS side-effect imports in translator/index.js).
pub fn register_all(reg: &mut Registry) {
    // claude-to-openai
    reg.register("claude", "openai", Some(claude_to_openai::claude_to_openai_request), None);

    // openai-to-claude
    reg.register("openai", "claude", Some(openai_to_claude::openai_to_claude_request), None);

    // gemini-to-openai
    reg.register("gemini", "openai", Some(gemini_to_openai::gemini_to_openai_request), None);
    reg.register("gemini-cli", "openai", Some(gemini_to_openai::gemini_to_openai_request), None);

    // openai-to-gemini
    reg.register("openai", "gemini", Some(openai_to_gemini::openai_to_gemini_request), None);

    // openai-to-vertex
    reg.register("openai", "vertex", Some(openai_to_vertex::openai_to_vertex_request), None);

    // antigravity-to-openai
    reg.register("antigravity", "openai", Some(antigravity_to_openai::antigravity_to_openai_request), None);

    // openai-responses (self-contained handler)
    reg.register("openai-responses", "openai", Some(openai_responses::openai_responses_request), None);

    // openai-to-kiro
    reg.register("openai", "kiro", Some(openai_to_kiro::openai_to_kiro_request), None);

    // openai-to-cursor
    reg.register("openai", "cursor", Some(openai_to_cursor::openai_to_cursor_request), None);

    // openai-to-ollama
    reg.register("openai", "ollama", Some(openai_to_ollama::openai_to_ollama_request), None);

    // openai-to-commandcode
    reg.register("openai", "commandcode", Some(openai_to_commandcode::openai_to_commandcode_request), None);

    // claude-to-kiro (direct route)
    reg.register("claude", "kiro", Some(claude_to_kiro::claude_to_kiro_request), None);
}
