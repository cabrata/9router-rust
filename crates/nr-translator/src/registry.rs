//! Translator registry and entry points. Port of open-sse/translator/index.js.
//!
//! JS translators self-register on import; here `request::register_all` and
//! `response::register_all` fill one static [`Registry`].

use std::collections::HashMap;
use std::sync::OnceLock;

use nr_core::{Credentials, Json};

use crate::state::StreamState;

/// Format ID constants (open-sse/translator/formats.js).
pub mod formats {
    pub const OPENAI: &str = "openai";
    pub const OPENAI_RESPONSES: &str = "openai-responses";
    pub const OPENAI_RESPONSE: &str = "openai-response";
    pub const CLAUDE: &str = "claude";
    pub const GEMINI: &str = "gemini";
    pub const GEMINI_CLI: &str = "gemini-cli";
    pub const VERTEX: &str = "vertex";
    pub const CODEX: &str = "codex";
    pub const ANTIGRAVITY: &str = "antigravity";
    pub const KIRO: &str = "kiro";
    pub const CURSOR: &str = "cursor";
    pub const OLLAMA: &str = "ollama";
    pub const COMMANDCODE: &str = "commandcode";
}

/// Options passed to request translators (JS `translationOptions`).
#[derive(Clone, Debug, Default)]
pub struct TranslationOptions {
    pub preserve_cache_control: bool,
}

/// JS request translator `(model, body, stream, credentials, options) => body`.
pub type RequestFn = fn(model: &str, body: Json, stream: bool, credentials: Option<&mut Credentials>, options: &TranslationOptions) -> Json;

/// JS response translator `(chunk, state) => chunk | chunk[] | null`.
/// `chunk == None` is the end-of-stream flush call.
pub type ResponseFn = fn(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json>;

pub struct Registry {
    request: HashMap<(String, String), RequestFn>,
    response: HashMap<(String, String), ResponseFn>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            request: HashMap::new(),
            response: HashMap::new(),
        }
    }

    /// JS `register(from, to, requestFn, responseFn)`.
    pub fn register(&mut self, from: &str, to: &str, request: Option<RequestFn>, response: Option<ResponseFn>) {
        let key = (from.to_string(), to.to_string());
        if let Some(f) = request {
            self.request.insert(key.clone(), f);
        }
        if let Some(f) = response {
            self.response.insert(key, f);
        }
    }

    pub fn request_fn(&self, from: &str, to: &str) -> Option<RequestFn> {
        self.request.get(&(from.to_string(), to.to_string())).copied()
    }

    pub fn response_fn(&self, from: &str, to: &str) -> Option<ResponseFn> {
        self.response.get(&(from.to_string(), to.to_string())).copied()
    }
}

static GLOBAL_REGISTRY: OnceLock<Registry> = OnceLock::new();

/// Initialize and return the global registry. Call this once at startup.
pub fn init_registry() {
    let mut reg = Registry::new();
    crate::request::register_all(&mut reg);
    crate::response::register_all(&mut reg);
    let _ = GLOBAL_REGISTRY.set(reg);
}

fn global_registry() -> &'static Registry {
    GLOBAL_REGISTRY.get_or_init(|| {
        let mut reg = Registry::new();
        crate::request::register_all(&mut reg);
        crate::response::register_all(&mut reg);
        reg
    })
}

/// Check if translation is needed between source and target formats.
pub fn needs_translation(source_format: &str, target_format: &str) -> bool {
    source_format != target_format
}

/// Translate request: source format → OpenAI format → target format.
///
/// Implements the full JS translateRequest flow including:
/// - stripContentTypes (explicit opt-in)
/// - normalizeThinkingConfig (TODO: nr-common)
/// - ensureToolCallIds
/// - fixMissingToolResponses
/// - Format translation (direct or pivot through OpenAI)
/// - Thinking normalization
/// - Output normalization
/// - Claude request prep + cloaking
///
/// # Arguments
/// * `source_format` - Client's request format (e.g., "claude", "openai")
/// * `target_format` - Upstream provider's expected format
/// * `model` - Model name
/// * `body` - Full request body
/// * `stream` - Whether streaming is enabled
/// * `credentials` - Optional credentials (may be modified by source→openai translator)
/// * `options` - Translation options
///
/// # Returns
/// Translated request body.
pub fn translate_request(
    source_format: &str,
    target_format: &str,
    model: &str,
    mut body: Json,
    stream: bool,
    mut credentials: Option<&mut Credentials>,
    options: &TranslationOptions,
) -> Json {
    use crate::concerns::tool_call;

    // Pre-translation setup (happens even if formats are the same)
    // TODO(dedupe): normalizeThinkingConfig - needs nr-common provider.js
    // TODO(dedupe): captureThinking - extract thinking from original body for later application
    // TODO(dedupe): captureSessionId - extract session ID for upstream

    // stripContentTypes: remove explicit content types (opt-in via strip[] in PROVIDER_MODELS)
    // TODO: implement stripContentTypes(body, stripList) using provider capabilities

    // Ensure tool calls have IDs
    tool_call::ensure_tool_call_ids(&mut body);

    // Fix missing tool responses
    // (but skip for Kiro format which has its own reconciliation)
    if target_format != formats::KIRO {
        tool_call::fix_missing_tool_responses(&mut body);
    }

    // Same format: apply normalization only
    if source_format == target_format {
        // Normalize to OpenAI format for consistency
        if target_format == formats::OPENAI {
            // TODO(dedupe): filterToOpenAIFormat - normalize structure
        }
        return body;
    }

    let reg = global_registry();

    // Direct route: source:target pair (e.g., claude:kiro, kiro:claude)
    if let Some(direct_fn) = reg.request_fn(source_format, target_format) {
        body = direct_fn(model, body, stream, credentials.as_deref_mut(), options);
    } else {
        // Pivot route: source → openai → target
        // Step 1: source → openai (credentials may be modified)
        if source_format != formats::OPENAI {
            if let Some(to_openai) = reg.request_fn(source_format, formats::OPENAI) {
                body = to_openai(model, body, stream, credentials.as_deref_mut(), options);
                // TODO(dedupe): reqLogger?.logOpenAIRequest?(result)
            }
        }

        // Step 2: openai → target (no credentials passed; modified state is in body)
        if target_format != formats::OPENAI {
            if let Some(from_openai) = reg.request_fn(formats::OPENAI, target_format) {
                body = from_openai(model, body, stream, None, options);
            }
        }
    }

    // Post-translation: apply thinking, Claude prep, cloaking
    // TODO(dedupe): applyThinking(targetFormat, model, result, provider, thinkingIntent)
    // TODO(dedupe): filterToOpenAIFormat (if target is OpenAI)
    // TODO(dedupe): prepareClaudeRequest (if target is Claude)
    // TODO(dedupe): ensureTrailingUserTurn (if target is Claude)
    // TODO(dedupe): cloakClaudeTools (if OAuth provider with cloakToolsOnOAuth quirk)

    body
}

/// Translate response chunk: target format → OpenAI format → source format.
///
/// Implements the full JS translateResponse flow including:
/// - Same-format passthrough with stream tool-call ID fixup
/// - Direct routes for specific format pairs
/// - Pivot translation through OpenAI intermediate format
/// - Tool name decloaking and restoration
///
/// Transforms a single chunk from upstream provider format back to client format.
/// `chunk == None` indicates end-of-stream flush.
///
/// # Arguments
/// * `target_format` - Upstream provider's format (where chunk came from)
/// * `source_format` - Client's expected format (where chunk is going to)
/// * `chunk` - Single response chunk (None = flush)
/// * `state` - Mutable streaming state
///
/// # Returns
/// Vec of chunks in source format (typically 0-2 per input chunk).
pub fn translate_response(
    target_format: &str,
    source_format: &str,
    chunk: Option<&Json>,
    state: &mut StreamState,
) -> Vec<Json> {
    // Same format: passthrough with stream tool-call ID fixup
    if source_format == target_format {
        if let Some(chunk) = chunk {
            // TODO(dedupe): ensureStreamToolCallIds - fixup tool call IDs for openai passthrough
            // TODO(dedupe): decloakStreamChunk - restore OAuth cloaked tool names
            // TODO(dedupe): restoreToolNames - general tool name restoration
            return vec![chunk.clone()];
        }
        return Vec::new();
    }

    let reg = global_registry();
    let mut results = Vec::new();

    // Direct route: target:source pair (e.g., kiro:claude, kiro:openai)
    if let Some(direct_fn) = reg.response_fn(target_format, source_format) {
        let converted = direct_fn(chunk, state);
        // TODO(dedupe): restoreToolNames on results
        return converted;
    }

    // Pivot route: target → openai → source
    // Step 1: target → openai
    if target_format != formats::OPENAI {
        if let Some(to_openai) = reg.response_fn(target_format, formats::OPENAI) {
            let converted = to_openai(chunk, state);
            results.extend(converted);
        }
    } else {
        // Already openai, use as-is
        if let Some(chunk) = chunk {
            results.push(chunk.clone());
        }
    }

    // Step 2: openai → source (if source is not openai)
    if source_format != formats::OPENAI && !results.is_empty() {
        if let Some(from_openai) = reg.response_fn(formats::OPENAI, source_format) {
            let mut final_results = Vec::new();
            for r in results {
                let converted = from_openai(Some(&r), state);
                final_results.extend(converted);
            }
            results = final_results;
        }
    }

    // TODO(dedupe): restoreToolNames(results, state?.toolNameMap)
    // TODO: attach OpenAI intermediate results for logging if applicable

    results
}

/// Initialize streaming state based on source format.
///
/// The JS initState returns different fields based on sourceFormat.
/// Base fields: messageId, model, textBlockStarted, inThinkingBlock, toolCalls, etc.
/// Special fields for openai-responses format: seq, responseId, created, msgTextBuf, etc.
pub fn init_state(_source_format: &str) -> StreamState {
    let state = StreamState::default();

    // Base state is set in StreamState::default()
    // For openai-responses format, TODO: add format-specific fields to state.ext
    // seq, responseId, created, started, msgTextBuf, msgItemAdded, etc.
    //
    // if _source_format == formats::OPENAI_RESPONSES { ... }

    state
}
