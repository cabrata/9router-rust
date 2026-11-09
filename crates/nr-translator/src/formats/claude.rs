//! Claude format helpers. Port of open-sse/translator/formats/claude.js.

use nr_core::Json;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::json;

use crate::schema::{claude_blocks, roles};

const CACHE_CONTROL_5M: &str = "ephemeral";
const TRAILING_USER_PLACEHOLDER: &str = "Continue.";

static DEFAULT_THINKING_CLAUDE_SIGNATURE: Lazy<String> = Lazy::new(|| {
    nr_data::export("open-sse/config/defaultThinkingSignature", "DEFAULT_THINKING_CLAUDE_SIGNATURE")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
});

/// Anthropic rejects a tool carrying BOTH defer_loading:true and cache_control.
/// Anchor on the last tool that CAN be cached instead of dropping caching.
pub fn last_cacheable_tool_index(tools: Option<&[Json]>) -> Option<usize> {
    let tools = tools?;
    for (idx, tool) in tools.iter().enumerate().rev() {
        if !tool.get("defer_loading").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Some(idx);
        }
    }
    None
}

/// Check if message has valid non-empty content.
pub fn has_valid_content(msg: &Json) -> bool {
    if let Some(content_str) = msg.get("content").and_then(|v| v.as_str()) {
        return !content_str.trim().is_empty();
    }
    if let Some(content_arr) = msg.get("content").and_then(|v| v.as_array()) {
        return content_arr.iter().any(is_contentful_block);
    }
    if let Some(content_obj) = msg.get("content").filter(|v| v.is_object()) {
        return is_contentful_block(content_obj);
    }
    false
}

fn is_contentful_block(block: &Json) -> bool {
    let Some(block_obj) = block.as_object() else {
        return false;
    };
    match block_obj.get("type").and_then(|v| v.as_str()) {
        Some(claude_blocks::TEXT) => block_obj
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        Some(claude_blocks::TOOL_USE)
        | Some(claude_blocks::TOOL_RESULT)
        | Some(claude_blocks::IMAGE)
        | Some(claude_blocks::DOCUMENT)
        | Some(claude_blocks::CONTAINER_UPLOAD) => true,
        _ => false,
    }
}

/// Content may arrive as a single content block object; wrap as one-block array
/// and strip any client-placed cache_control.
fn normalize_message_content(msg: &mut Json) {
    if let Some(c) = msg.get_mut("content") {
        if c.is_object() && !c.is_array() {
            if let Some(obj) = c.as_object_mut() {
                obj.remove("cache_control");
            }
            let wrapped = json!([c.clone()]);
            *c = wrapped;
        }
    }
}

/// Fix tool_use/tool_result ordering for Claude API:
/// 1. In assistant message with tool_use: remove text AFTER tool_use
/// 2. Merge consecutive same-role messages
pub fn fix_tool_use_ordering(messages: &mut Vec<Json>) {
    if messages.len() <= 1 {
        return;
    }

    // Pass 1: Fix assistant messages with tool_use - remove text after tool_use
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
            if let Some(content) = msg.get("content").and_then(|c| c.as_array()).cloned() {
                let has_tool_use = content.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_USE));
                if has_tool_use {
                    let mut new_content = Vec::new();
                    let mut found_tool_use = false;
                    for block in content {
                        let t = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if t == claude_blocks::TOOL_USE {
                            found_tool_use = true;
                            new_content.push(block);
                        } else if t == claude_blocks::THINKING || t == claude_blocks::REDACTED_THINKING {
                            new_content.push(block);
                        } else if !found_tool_use {
                            new_content.push(block);
                        }
                    }
                    msg["content"] = Json::Array(new_content);
                }
            }
        }
    }

    // Pass 2: Merge consecutive same-role messages
    let mut merged: Vec<Json> = Vec::new();
    for msg in messages.drain(..) {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let is_same_as_last = merged.last().and_then(|l| l.get("role").and_then(|v| v.as_str())) == Some(&role);

        let msg_content: Vec<Json> = match msg.get("content") {
            Some(Json::Array(arr)) => arr.clone(),
            Some(Json::String(s)) => vec![json!({"type": claude_blocks::TEXT, "text": s})],
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };

        if is_same_as_last {
            let last = merged.last_mut().unwrap();
            let last_content: Vec<Json> = match last.get("content") {
                Some(Json::Array(arr)) => arr.clone(),
                Some(Json::String(s)) => vec![json!({"type": claude_blocks::TEXT, "text": s})],
                Some(other) => vec![other.clone()],
                None => Vec::new(),
            };

            let mut tool_results: Vec<Json> = last_content
                .iter()
                .chain(msg_content.iter())
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT))
                .cloned()
                .collect();
            let other_content: Vec<Json> = last_content
                .iter()
                .chain(msg_content.iter())
                .filter(|b| b.get("type").and_then(|v| v.as_str()) != Some(claude_blocks::TOOL_RESULT))
                .cloned()
                .collect();
            tool_results.extend(other_content);
            last["content"] = Json::Array(tool_results);
        } else {
            let mut new_msg = msg;
            new_msg["content"] = Json::Array(msg_content);
            merged.push(new_msg);
        }
    }

    *messages = merged;
}

/// Ensure trailing user turn for Claude (no empty assistant prefill).
pub fn ensure_trailing_user_turn(messages: &mut Vec<Json>, original_last_role: Option<&str>) {
    if original_last_role == Some(roles::ASSISTANT) {
        return;
    }
    let is_last_assistant = messages
        .last()
        .and_then(|m| m.get("role").and_then(|v| v.as_str()))
        == Some(roles::ASSISTANT);
    if is_last_assistant {
        messages.push(json!({
            "role": roles::USER,
            "content": [{"type": claude_blocks::TEXT, "text": TRAILING_USER_PLACEHOLDER}]
        }));
    }
}

fn handles_thinking_blocks(provider: Option<&str>) -> bool {
    let p = provider.unwrap_or("");
    p == "claude" || p.starts_with("anthropic-compatible") || p == "deepseek"
}

fn build_thinking_placeholder(provider: Option<&str>, unsigned: bool) -> Json {
    let mut block = json!({
        "type": claude_blocks::THINKING,
        "thinking": "."
    });
    let p = provider.unwrap_or("");
    if p != "deepseek" && !unsigned {
        block["signature"] = json!(DEFAULT_THINKING_CLAUDE_SIGNATURE.as_str());
    }
    block
}

/// Move image blocks inside a tool_result to become sibling user blocks.
pub fn hoist_tool_result_images(body: &mut Json) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else { return };
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) != Some(roles::USER) {
            continue;
        }
        let Some(content) = msg.get("content").and_then(|c| c.as_array()).cloned() else { continue };
        let mut hoisted = Vec::new();
        let mut new_content = Vec::new();
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) != Some(claude_blocks::TOOL_RESULT) {
                new_content.push(block);
                continue;
            }
            let sub_content = block.get("content").and_then(|c| c.as_array());
            let Some(sub_content) = sub_content else {
                new_content.push(block);
                continue;
            };
            let images: Vec<Json> = sub_content
                .iter()
                .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::IMAGE))
                .cloned()
                .collect();
            if images.is_empty() {
                new_content.push(block);
                continue;
            }
            let rest: Vec<Json> = sub_content
                .iter()
                .filter(|c| c.get("type").and_then(|v| v.as_str()) != Some(claude_blocks::IMAGE))
                .cloned()
                .collect();
            let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
            hoisted.push(json!({"type": claude_blocks::TEXT, "text": format!("[Image from tool result {tool_use_id}]")}));
            hoisted.extend(images);

            let mut updated_block = block.clone();
            updated_block["content"] = if rest.is_empty() {
                json!([{"type": claude_blocks::TEXT, "text": "(image attached below)"}])
            } else {
                Json::Array(rest)
            };
            new_content.push(updated_block);
        }
        if !hoisted.is_empty() {
            new_content.extend(hoisted);
            msg["content"] = Json::Array(new_content);
        }
    }
}

fn mark_last_cacheable_block(msg: &mut Json) -> bool {
    let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else { return false };
    for block in content.iter_mut().rev() {
        let t = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == claude_blocks::THINKING || t == claude_blocks::REDACTED_THINKING {
            continue;
        }
        if let Some(obj) = block.as_object_mut() {
            obj.insert("cache_control".to_string(), json!({"type": CACHE_CONTROL_5M}));
            return true;
        }
    }
    false
}

fn mark_final_tool_results(body: &mut Json) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else { return false };
    let Some(last) = messages.last_mut() else { return false };
    if last.get("role").and_then(|v| v.as_str()) != Some(roles::USER) {
        return false;
    }
    let has_tool_result = last
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT)))
        .unwrap_or(false);
    if !has_tool_result {
        return false;
    }
    let already_cached = last
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().any(|b| b.get("cache_control").is_some()))
        .unwrap_or(false);
    if already_cached {
        return false;
    }
    mark_last_cacheable_block(last)
}

/// Re-anchor cache breakpoints on a Claude body.
pub fn anchor_claude_cache(body: &mut Json) {
    if !body.is_object() {
        return;
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages {
            normalize_message_content(msg);
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for t in tools.iter_mut() {
            if t.get("defer_loading").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(obj) = t.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
    }

    // System: head anchor on last system block at 1h
    if let Some(system) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
        let last_idx = system.len().saturating_sub(1);
        for (i, block) in system.iter_mut().enumerate() {
            if let Some(obj) = block.as_object_mut() {
                if i == last_idx {
                    obj.insert("cache_control".to_string(), json!({"type": "ephemeral", "ttl": "1h"}));
                } else {
                    obj.remove("cache_control");
                }
            }
        }
    }

    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        let last_idx = last_cacheable_tool_index(Some(tools));
        for (i, tool) in tools.iter_mut().enumerate() {
            if let Some(obj) = tool.as_object_mut() {
                if Some(i) == last_idx {
                    obj.insert("cache_control".to_string(), json!({"type": "ephemeral", "ttl": "1h"}));
                } else {
                    obj.remove("cache_control");
                }
            }
        }
    }

    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let mut anchored = false;
        for msg in messages.iter_mut().rev() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for b in content.iter_mut() {
                    if let Some(obj) = b.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
            if !anchored && msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
                anchored = mark_last_cacheable_block(msg);
            }
        }
        if !anchored {
            for msg in messages.iter_mut().rev() {
                if mark_last_cacheable_block(msg) {
                    break;
                }
            }
        }
        mark_final_tool_results(body);
    }
}

/// Normalize native Claude passthrough body to match Anthropic Messages API spec.
pub fn normalize_claude_passthrough(body: &mut Json, model: &str) {
    if !body.is_object() {
        return;
    }
    // Downgrade adaptive thinking for haiku models
    if model.to_lowercase().contains("haiku") {
        if body.get("thinking").and_then(|t| t.get("type")).and_then(|v| v.as_str()) == Some("adaptive") {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": 10000});
        }
        if let Some(obj) = body.as_object_mut() {
            obj.remove("output_config");
        }
    }

    let original_last_role = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.last())
        .and_then(|m| m.get("role"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            normalize_message_content(msg);
        }
        // Fold mid-conversation system messages into neighboring user turn
        let mut folded: Vec<Json> = Vec::new();
        for msg in messages.drain(..) {
            if msg.get("role").and_then(|v| v.as_str()) != Some(roles::SYSTEM) {
                folded.push(msg);
                continue;
            }
            let text = match msg.get("content") {
                Some(Json::String(s)) => s.clone(),
                Some(Json::Array(arr)) => arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if text.trim().is_empty() {
                continue;
            }
            let block = json!({"type": claude_blocks::TEXT, "text": text});
            if let Some(prev) = folded.last_mut().filter(|m| m.get("role").and_then(|v| v.as_str()) == Some(roles::USER)) {
                if let Some(content) = prev.get_mut("content").and_then(|c| c.as_array_mut()) {
                    content.push(block);
                }
            } else {
                folded.push(json!({"role": roles::USER, "content": [block]}));
            }
        }
        // Filter empty content
        folded.retain(has_valid_content);
        ensure_trailing_user_turn(&mut folded, original_last_role.as_deref());
        *messages = folded;
    }
}

/// Prepare request for Claude format endpoints.
pub fn prepare_claude_request(
    body: &mut Json,
    provider: Option<&str>,
    api_key: Option<&str>,
    connection_id: Option<&str>,
    raw_headers: Option<&Json>,
    session_id: Option<&str>,
) {
    if !body.is_object() {
        return;
    }
    let is_anthropic = provider == Some("claude")
        || provider.map_or(false, |p| p.starts_with("anthropic-compatible-"));
    if !is_anthropic {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("context_management");
        }
    }

    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let model_caps = nr_data::capabilities::get_capabilities_for_model(provider, Some(&model));

    if let Some(off_type) = model_caps.get("thinkingOffType").and_then(|v| v.as_str()) {
        if body.get("thinking").and_then(|t| t.get("type")).and_then(|v| v.as_str()) == Some("disabled") {
            body["thinking"] = json!({"type": off_type});
        }
    }
    if model_caps.get("forcedToolChoice").and_then(|v| v.as_bool()) == Some(false) {
        let is_forced = body.get("tool_choice").and_then(|t| t.get("type")).and_then(|v| v.as_str()).map_or(false, |t| t == "any" || t == "tool");
        if is_forced {
            body["tool_choice"] = json!({"type": "auto"});
        }
    }

    // Clamp max_tokens to ceiling
    if let Some(max_tokens) = body.get("max_tokens").and_then(|v| v.as_i64()) {
        let ceiling = model_caps.get("maxOutput").and_then(|v| v.as_i64()).unwrap_or(64000);
        let mut target_max = max_tokens.min(ceiling);
        if let Some(budget) = body.get("thinking").and_then(|t| t.get("budget_tokens")).and_then(|v| v.as_i64()) {
            if budget >= target_max {
                target_max = (budget + 1024).min(ceiling);
                if budget >= target_max {
                    body["thinking"]["budget_tokens"] = json!(1024.max(target_max - 1024));
                }
            }
        }
        body["max_tokens"] = json!(target_max);
    }

    // System: ttl 1h on last block
    if let Some(system) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
        let last_idx = system.len().saturating_sub(1);
        for (i, block) in system.iter_mut().enumerate() {
            if let Some(obj) = block.as_object_mut() {
                obj.remove("cache_control");
                if i == last_idx {
                    obj.insert("cache_control".to_string(), json!({"type": "ephemeral", "ttl": "1h"}));
                }
            }
        }
    }

    let original_last_role = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.last())
        .and_then(|m| m.get("role"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let thinking_type_enabled = body.get("thinking").and_then(|t| t.get("type")).and_then(|v| v.as_str()) == Some("enabled");

    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            normalize_message_content(msg);
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for b in content {
                    if let Some(obj) = b.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
        }
        messages.retain(has_valid_content);
        fix_tool_use_ordering(messages);
        ensure_trailing_user_turn(messages, original_last_role.as_deref());

        // Check if thinking is enabled
        let last_is_user = messages
            .last()
            .and_then(|m| m.get("role").and_then(|v| v.as_str()))
            == Some(roles::USER);
        let thinking_enabled = last_is_user && thinking_type_enabled;

        let mut last_assistant_processed = false;
        for msg in messages.iter_mut().rev() {
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
                if !last_assistant_processed {
                    mark_last_cacheable_block(msg);
                    last_assistant_processed = true;
                }
                if handles_thinking_blocks(provider) {
                    let mut has_tool_use = false;
                    let mut has_kept_thinking = false;
                    let content_arr = msg.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                    let mut kept = Vec::new();
                    for b in content_arr {
                        let t = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if t == claude_blocks::THINKING || t == claude_blocks::REDACTED_THINKING {
                            let text = b.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                            if text.trim().is_empty() {
                                continue;
                            }
                            if provider == Some("claude") {
                                let sig = b.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                                if nr_common::claude_signature::is_valid_claude_signature(sig) {
                                    has_kept_thinking = true;
                                    kept.push(b);
                                }
                            } else {
                                let mut b = b;
                                b["signature"] = json!(DEFAULT_THINKING_CLAUDE_SIGNATURE.as_str());
                                has_kept_thinking = true;
                                kept.push(b);
                            }
                            continue;
                        }
                        if t == claude_blocks::TOOL_USE {
                            has_tool_use = true;
                        }
                        kept.push(b);
                    }
                    if thinking_enabled && !has_kept_thinking && has_tool_use {
                        kept.insert(0, build_thinking_placeholder(provider, false));
                    }
                    msg["content"] = Json::Array(kept);
                }
            }
        }
    }

    // Tools
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        let last_idx = last_cacheable_tool_index(Some(tools));
        for (i, tool) in tools.iter_mut().enumerate() {
            if let Some(obj) = tool.as_object_mut() {
                if Some(i) == last_idx {
                    obj.insert("cache_control".to_string(), json!({"type": "ephemeral", "ttl": "1h"}));
                } else {
                    obj.remove("cache_control");
                }
            }
        }
    }

    if provider != Some("claude") && !provider.map_or(false, |p| p.starts_with("anthropic-compatible")) {
        hoist_tool_result_images(body);
    }
    mark_final_tool_results(body);

    if (provider == Some("claude") || provider.map_or(false, |p| p.starts_with("anthropic-compatible"))) {
        if let Some(key) = api_key {
            let sid = session_id.map(|s| s.to_string()).unwrap_or_else(|| {
                connection_id.unwrap_or("anon").to_string()
            });
            *body = nr_common::claude_cloaking::apply_cloaking(body, key, Some(&sid));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_valid_content_text() {
        let msg = json!({"content": "hello"});
        assert!(has_valid_content(&msg));
    }

    #[test]
    fn test_has_valid_content_empty() {
        let msg = json!({"content": ""});
        assert!(!has_valid_content(&msg));
    }

    #[test]
    fn test_last_cacheable_tool_index() {
        let tools = vec![
            json!({"defer_loading": true}),
            json!({"defer_loading": false}),
            json!({"defer_loading": true}),
        ];
        let result = last_cacheable_tool_index(Some(&tools));
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_ensure_trailing_user_turn() {
        let mut messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        ensure_trailing_user_turn(&mut messages, Some("user"));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["text"], "Continue.");
    }

    #[test]
    fn test_ensure_trailing_user_turn_client_ended_assistant() {
        let mut messages = vec![
            json!({"role": "assistant", "content": "prefill"}),
        ];
        ensure_trailing_user_turn(&mut messages, Some("assistant"));
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_fix_tool_use_ordering_drops_text_after_tool() {
        let mut messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}},
                    {"type": "text", "text": "after"},
                ]
            }),
        ];
        fix_tool_use_ordering(&mut messages);
        // assistant was the last (1-message) chain? No, still needs len>1 — check content of index 1.
        let content = messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "before");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn test_fix_tool_use_ordering_merges_same_role() {
        let mut messages = vec![
            json!({"role": "user", "content": "a"}),
            json!({"role": "user", "content": "b"}),
        ];
        fix_tool_use_ordering(&mut messages);
        assert_eq!(messages.len(), 1);
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
    }
}
