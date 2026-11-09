//! Translator registry and entry points. Port of open-sse/translator/index.js.
//!
//! JS translators self-register on import; here `request::register_all` and
//! `response::register_all` fill one static [`Registry`].

use std::collections::HashMap;
use std::sync::OnceLock;

use nr_core::{Credentials, Json};
use serde_json::json;

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
    use crate::concerns::thinkingUnified;
    use nr_common::provider;

    // Strip explicit content types (opt-in via strip[] in PROVIDER_MODELS)
    // TODO(dedupe): stripContentTypes requires PROVIDER_MODELS[provider].strip; deferred
    // until caller threads `provider` through translate_request (currently not in signature).

    // Role the client actually ended on, before any translator drops an emptied
    // turn. detectClientLastRole: not every source uses messages[] (Gemini uses
    // contents[], Responses/Codex use input[]).
    let client_last_role = detect_client_last_role(&body);

    // Pre-translation: normalize provider-native thinking config against last message role.
    provider::normalize_thinking_config(&mut body);

    // Capture thinking intent before any translator strips/renames the fields.
    let thinking_intent = thinkingUnified::capture_thinking(&body);

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
            crate::formats::openai::filter_to_openai_format(&mut body, &crate::formats::openai::FilterOptions {
                preserve_cache_control: options.preserve_cache_control,
            });
        }
        let kiro_mapped = target_format == formats::KIRO
            && (source_format == formats::OPENAI || source_format == formats::CLAUDE);
        if !kiro_mapped {
            thinkingUnified::apply_thinking(target_format, model, &mut body, None, thinking_intent.as_ref());
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

    // Post-translation: apply thinking, Claude prep, cloaking.
    // Kiro's GenerateAssistantResponse request does not accept the generic
    // top-level `thinking` field; its translators map thinking intent to
    // KAS-compatible systemPrompt/additionalModelRequestFields instead.
    let kiro_thinking_mapped_by_translator = target_format == formats::KIRO
        && (source_format == formats::OPENAI || source_format == formats::CLAUDE);
    if !kiro_thinking_mapped_by_translator {
        thinkingUnified::apply_thinking(target_format, model, &mut body, None, thinking_intent.as_ref());
    }
    if target_format == formats::OPENAI {
        crate::formats::openai::filter_to_openai_format(&mut body, &crate::formats::openai::FilterOptions {
            preserve_cache_control: options.preserve_cache_control,
        });
    }
    if target_format == formats::CLAUDE {
        let api_key = credentials.as_deref().and_then(|c| c.str("key").or_else(|| c.str("apiKey"))).map(|s| s.to_string());
        let connection_id = credentials.as_deref().and_then(|c| c.str("connection_id")).map(|s| s.to_string());
        crate::formats::claude::prepare_claude_request(
            &mut body,
            None,
            api_key.as_deref(),
            connection_id.as_deref(),
            None,
            None,
        );
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            let mut owned = std::mem::take(messages);
            crate::formats::claude::ensure_trailing_user_turn(&mut owned, client_last_role.as_deref());
            *messages = owned;
        }
        crate::formats::claude::anchor_claude_cache(&mut body);
    }

    body
}

/// Port of JS `detectClientLastRole`: the role the client's conversation ended
/// on, in the source format's own shape. Only an explicit trailing
/// model/assistant turn counts as real prefill.
fn detect_client_last_role(body: &Json) -> Option<String> {
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        return messages.last().and_then(|m| m.get("role")).and_then(|v| v.as_str()).map(|s| s.to_string());
    }
    let items = body
        .get("contents")
        .and_then(|v| v.as_array())
        .or_else(|| body.get("input").and_then(|v| v.as_array()))?;
    let role = items.last().and_then(|m| m.get("role")).and_then(|v| v.as_str());
    if role == Some(crate::schema::roles::ASSISTANT) || role == Some("model") {
        return Some(crate::schema::roles::ASSISTANT.to_string());
    }
    None
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
            // Restore OAuth-cloaked tool names (state.toolNameMap set by cloak_claude_tools).
            if let Some(map) = state.tool_name_map.as_ref() {
                let restored = nr_common::claude_cloaking::decloak_stream_chunk(chunk, Some(map));
                return vec![restored];
            }
            return vec![chunk.clone()];
        }
        return Vec::new();
    }

    let reg = global_registry();
    let mut results = Vec::new();

    // Direct route: target:source pair (e.g., kiro:claude, kiro:openai)
    if let Some(direct_fn) = reg.response_fn(target_format, source_format) {
        let converted = direct_fn(chunk, state);
        return restore_tool_names(converted, state.tool_name_map.as_ref());
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

    // Restore sanitized tool names (`mcp__a__b` → `mcp_a_b` in Kiro, suffixed
    // cloaked names in Claude) so tool calls stream back under the client's
    // own names. JS: restoreToolNames(results, state?.toolNameMap).
    restore_tool_names(results, state.tool_name_map.as_ref())
}

/// Port of open-sse/utils/opencodeFingerprint.js `restoreToolNames`.
/// Maps tool call names back through `map` (sanitized → client) across the
/// shapes clients actually see: Claude content_block_start + message content,
/// OpenAI Chat Completions delta/message.tool_calls, and Responses
/// output / output_item.added events.
fn restore_tool_names(payloads: Vec<Json>, map: Option<&HashMap<String, String>>) -> Vec<Json> {
    let Some(map) = map.filter(|m| !m.is_empty()) else {
        return payloads;
    };
    payloads.into_iter().map(|p| restore_tool_names_one(p, map)).collect()
}

fn restore_tool_names_one(payload: Json, map: &HashMap<String, String>) -> Json {
    let Json::Object(mut out) = payload.clone() else {
        if let Json::Array(arr) = payload {
            return Json::Array(arr.into_iter().map(|item| restore_tool_names_one(item, map)).collect());
        }
        return payload;
    };

    // Rename a tool-bearing object's `name` when the map knows it.
    let rename = |value: &Json| -> Option<Json> {
        let name = value.get("name").and_then(|v| v.as_str())?;
        let mapped = map.get(name)?;
        let mut next = value.clone();
        next["name"] = json!(mapped);
        Some(next)
    };
    let rename_tagged = |value: &Json, kind: &str| -> Option<Json> {
        if value.get("type").and_then(|v| v.as_str()) != Some(kind) {
            return None;
        }
        rename(value)
    };
    let rename_all = |values: &[Json], kinds: &[&str]| -> Option<Vec<Json>> {
        let mut changed = false;
        let mut next: Vec<Json> = Vec::with_capacity(values.len());
        for value in values {
            match kinds.iter().find_map(|kind| rename_tagged(value, kind)) {
                Some(renamed) => {
                    next.push(renamed);
                    changed = true;
                }
                None => next.push(value.clone()),
            }
        }
        changed.then_some(next)
    };

    // Claude SSE content_block_start event and Claude message bodies.
    if let Some(renamed) = out.get("content_block").and_then(|b| rename_tagged(b, "tool_use")) {
        out.insert("content_block".into(), renamed);
    }
    if let Some(content) = out.get("content").and_then(|v| v.as_array()) {
        if let Some(renamed) = rename_all(content, &["tool_use"]) {
            out.insert("content".into(), Json::Array(renamed));
        }
    }

    // OpenAI Chat Completions: streaming `delta` and non-streaming `message`.
    if let Some(choices) = out.get("choices").and_then(|v| v.as_array()).cloned() {
        let mut changed = false;
        let next: Vec<Json> = choices
            .into_iter()
            .map(|choice| {
                let mut choice = choice;
                let mut choice_changed = false;
                for holder in ["delta", "message"] {
                    let calls = choice
                        .get(holder)
                        .and_then(|v| v.get("tool_calls"))
                        .and_then(|v| v.as_array())
                        .cloned();
                    let Some(calls) = calls else { continue };
                    let mut renamed_calls: Vec<Json> = Vec::with_capacity(calls.len());
                    let mut call_changed = false;
                    for call in calls {
                        match call.get("function").and_then(rename) {
                            Some(function) => {
                                let mut call = call;
                                call["function"] = function;
                                renamed_calls.push(call);
                                call_changed = true;
                            }
                            None => renamed_calls.push(call),
                        }
                    }
                    if call_changed {
                        choice[holder]["tool_calls"] = Json::Array(renamed_calls);
                        choice_changed = true;
                    }
                }
                changed |= choice_changed;
                choice
            })
            .collect();
        if changed {
            out.insert("choices".into(), Json::Array(next));
        }
    }

    // OpenAI Responses final body (`output[]`) and SSE `response.output_item.*` (`item`).
    if let Some(output) = out.get("output").and_then(|v| v.as_array()) {
        if let Some(renamed) = rename_all(output, &["function_call"]) {
            out.insert("output".into(), Json::Array(renamed));
        }
    }
    if let Some(item) = out.get("item").and_then(|v| rename_tagged(v, "function_call")) {
        out.insert("item".into(), item);
    }

    Json::Object(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map() -> HashMap<String, String> {
        [("mcp_a_b".to_string(), "mcp__a__b".to_string())].into_iter().collect()
    }

    #[test]
    fn test_restore_claude_content_block_start() {
        let chunk = json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "mcp_a_b", "id": "t1"}});
        let restored = restore_tool_names(vec![chunk], Some(&map()));
        assert_eq!(restored[0]["content_block"]["name"], "mcp__a__b");
    }

    #[test]
    fn test_restore_claude_message_content() {
        let msg = json!({"role": "assistant", "content": [
            {"type": "text", "text": "hi"},
            {"type": "tool_use", "name": "mcp_a_b", "id": "t1", "input": {}}
        ]});
        let restored = restore_tool_names(vec![msg], Some(&map()));
        assert_eq!(restored[0]["content"][0]["text"], "hi");
        assert_eq!(restored[0]["content"][1]["name"], "mcp__a__b");
    }

    #[test]
    fn test_restore_openai_delta_and_message() {
        let m = map();
        let delta = json!({"choices": [{"delta": {"tool_calls": [{"id": "t1", "function": {"name": "mcp_a_b", "arguments": "{}"}}]}}]});
        let restored = restore_tool_names(vec![delta], Some(&m));
        assert_eq!(restored[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "mcp__a__b");

        let message = json!({"choices": [{"message": {"tool_calls": [{"id": "t1", "function": {"name": "mcp_a_b", "arguments": "{}"}}]}}]});
        let restored = restore_tool_names(vec![message], Some(&m));
        assert_eq!(restored[0]["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "mcp__a__b");
    }

    #[test]
    fn test_restore_responses_output_and_item() {
        let m = map();
        let body = json!({"output": [{"type": "function_call", "name": "mcp_a_b", "call_id": "c1"}]});
        let restored = restore_tool_names(vec![body], Some(&m));
        assert_eq!(restored[0]["output"][0]["name"], "mcp__a__b");

        let event = json!({"type": "response.output_item.added", "item": {"type": "function_call", "name": "mcp_a_b"}});
        let restored = restore_tool_names(vec![event], Some(&m));
        assert_eq!(restored[0]["item"]["name"], "mcp__a__b");
    }

    #[test]
    fn test_restore_noop_without_map() {
        let chunk = json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "mcp_a_b"}});
        let restored = restore_tool_names(vec![chunk.clone()], None);
        assert_eq!(restored[0]["content_block"]["name"], "mcp_a_b");
        let restored = restore_tool_names(vec![chunk], Some(&HashMap::new()));
        assert_eq!(restored[0]["content_block"]["name"], "mcp_a_b");
    }

    #[test]
    fn test_restore_unknown_names_untouched() {
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{"id": "t1", "function": {"name": "other"}}]}}]});
        let restored = restore_tool_names(vec![chunk], Some(&map()));
        assert_eq!(restored[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "other");
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_detect_client_last_role_shapes() {
        let openai = json!({"messages": [{"role": "user", "content": "hi"}, {"role": "assistant", "content": "yo"}]});
        assert_eq!(detect_client_last_role(&openai).as_deref(), Some("assistant"));

        let gemini = json!({"contents": [{"role": "user", "parts": []}, {"role": "model", "parts": []}]});
        assert_eq!(detect_client_last_role(&gemini).as_deref(), Some("assistant"));

        // A trailing function output must not read as prefill.
        let responses = json!({"input": [{"type": "function_call_output", "call_id": "c1", "output": "x"}]});
        assert_eq!(detect_client_last_role(&responses), None);

        assert_eq!(detect_client_last_role(&json!({})), None);
    }

    #[test]
    fn test_translate_request_claude_gets_trailing_user_turn() {
        // Client ends on an assistant turn that the translator empties: Claude
        // rejects a trailing assistant message, so the placeholder is added.
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "tools": []});
        let out = translate_request("openai", "claude", "claude-sonnet-4-5", body, true, None, &TranslationOptions::default());
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.last().unwrap()["role"], "user");
    }

    #[test]
    fn test_translate_request_kiro_skips_generic_thinking_field() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "reasoning_effort": "high"});
        let out = translate_request("openai", "kiro", "kiro-claude-sonnet-4", body, true, None, &TranslationOptions::default());
        assert!(!out.is_null(), "{out}");
        // Kiro keeps thinking out of the generic top-level field.
        assert!(out.get("thinking").is_none(), "{out}");
        assert!(out.get("reasoning_effort").is_none(), "{out}");
        assert!(out["conversationState"].is_object(), "{out}");
    }

    #[test]
    fn test_translate_request_openai_to_openai_filters() {
        // `store` is an OpenAI-native field (kept); `developer` role is
        // normalized to `system` because most providers reject it.
        let body = json!({"model": "x", "messages": [
            {"role": "developer", "content": "sys"},
            {"role": "user", "content": "hi"}
        ], "store": true});
        let out = translate_request("openai", "openai", "x", body, true, None, &TranslationOptions::default());
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["store"], true);
    }
}
