//! Claude → Kiro request translator (DIRECT route, no OpenAI pivot).
//! Port of open-sse/translator/request/claude-to-kiro.js.

use nr_core::{Credentials, Json};
use serde_json::json;

use super::kiro_shared::{
    apply_thinking_override, replay_store, resolve_model_intent, resolve_profile_arn,
    resolve_thinking_budget, restored_tool_names,
};
use crate::concerns::kiroConversation::{canonicalize_kiro_conversation, kiro_empty_user_content, normalize_kiro_tool_specs};
use crate::schema::claude_blocks;
use crate::TranslationOptions;

const DEFAULT_IMAGE_MIME: &str = "image/png";

fn image_format(media_type: &str) -> String {
    media_type.split('/').nth(1).unwrap_or(media_type).to_string()
}

fn convert_claude_messages_to_kiro(messages: &[Json], model: &str) -> (Vec<Json>, Json) {
    let mut history: Vec<Json> = Vec::new();
    let mut pending_user_content: Vec<String> = Vec::new();
    let mut pending_assistant_content: Vec<String> = Vec::new();
    let mut pending_tool_results: Vec<Json> = Vec::new();
    let mut pending_images: Vec<Json> = Vec::new();
    let mut current_role: Option<String> = None;
    let mut current_message = Json::Null;

    fn flush(
        history: &mut Vec<Json>,
        current_message: &mut Json,
        current_role: &mut Option<String>,
        user_content: &mut Vec<String>,
        assistant_content: &mut Vec<String>,
        tool_results: &mut Vec<Json>,
        images: &mut Vec<Json>,
        model: &str,
    ) {
        match current_role.as_deref() {
            Some("user") => {
                let text = user_content.join("\n\n").trim().to_string();
                let content = if text.is_empty() {
                    kiro_empty_user_content(!tool_results.is_empty()).to_string()
                } else {
                    text
                };
                let mut user_msg = json!({"userInputMessage": {"content": content, "modelId": model}});
                if !images.is_empty() {
                    user_msg["userInputMessage"]["images"] = Json::Array(std::mem::take(images));
                }
                if !tool_results.is_empty() {
                    user_msg["userInputMessage"]["userInputMessageContext"] = json!({"toolResults": std::mem::take(tool_results)});
                }
                history.push(user_msg.clone());
                *current_message = user_msg;
                user_content.clear();
                tool_results.clear();
                images.clear();
            }
            Some("assistant") => {
                let text = assistant_content.join("\n\n").trim().to_string();
                let content = if text.is_empty() { "...".to_string() } else { text };
                history.push(json!({"assistantResponseMessage": {"content": content}}));
                assistant_content.clear();
            }
            _ => {}
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if Some(role) != current_role.as_deref() && current_role.is_some() {
            flush(&mut history, &mut current_message, &mut current_role, &mut pending_user_content, &mut pending_assistant_content, &mut pending_tool_results, &mut pending_images, model);
        }
        current_role = Some(role.to_string());

        if role == "user" {
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                pending_user_content.push(text.to_string());
            } else if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
                for block in blocks {
                    let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if kind == claude_blocks::TEXT {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            pending_user_content.push(text.to_string());
                        }
                    } else if kind == claude_blocks::IMAGE {
                        let source = block.get("source");
                        if source.and_then(|s| s.get("type")).and_then(|v| v.as_str()) == Some("base64") {
                            if let Some(data) = source.and_then(|s| s.get("data")).and_then(|v| v.as_str()) {
                                let media_type = source.and_then(|s| s.get("media_type")).and_then(|v| v.as_str()).unwrap_or(DEFAULT_IMAGE_MIME);
                                pending_images.push(json!({"format": image_format(media_type), "source": {"bytes": data}}));
                            }
                        }
                    } else if kind == claude_blocks::TOOL_RESULT {
                        let mut result_content = String::new();
                        match block.get("content") {
                            Some(Json::String(s)) => result_content = s.clone(),
                            Some(Json::Array(parts)) => {
                                // Images a tool returned (screenshots) ride along as user
                                // images; Kiro tool results are text-only.
                                let mut has_image = false;
                                for c in parts {
                                    if c.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::IMAGE)
                                        && c.get("source").and_then(|s| s.get("type")).and_then(|v| v.as_str()) == Some("base64")
                                    {
                                        has_image = true;
                                        if let Some(data) = c.get("source").and_then(|s| s.get("data")).and_then(|v| v.as_str()) {
                                            let media_type = c.get("source").and_then(|s| s.get("media_type")).and_then(|v| v.as_str()).unwrap_or(DEFAULT_IMAGE_MIME);
                                            pending_images.push(json!({"format": image_format(media_type), "source": {"bytes": data}}));
                                        }
                                    }
                                }
                                let text = parts
                                    .iter()
                                    .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TEXT))
                                    .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                result_content = if !text.is_empty() {
                                    text
                                } else if has_image {
                                    "(image attached)".to_string()
                                } else {
                                    serde_json::to_string(parts).unwrap_or_default()
                                };
                            }
                            Some(other) if !other.is_null() => {
                                result_content = serde_json::to_string(other).unwrap_or_default();
                            }
                            _ => {}
                        }
                        let is_error = block.get("is_error").map(nr_core::json::truthy).unwrap_or(false);
                        pending_tool_results.push(json!({
                            "toolUseId": block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or(""),
                            "status": if is_error { "error" } else { "success" },
                            "content": [{"text": result_content}]
                        }));
                    }
                }
            }
        } else if role == "assistant" {
            let mut text_content = String::new();
            let mut tool_uses: Vec<Json> = Vec::new();
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                text_content = text.to_string();
            } else if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
                for block in blocks {
                    let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if kind == claude_blocks::TEXT {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            text_content.push_str(text);
                        }
                    } else if kind == claude_blocks::TOOL_USE {
                        tool_uses.push(json!({
                            "toolUseId": block.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            "name": block.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                            "input": block.get("input").cloned().unwrap_or(Json::Object(Default::default()))
                        }));
                    }
                }
            }
            if !text_content.is_empty() {
                pending_assistant_content.push(text_content);
            }
            if !tool_uses.is_empty() {
                flush(&mut history, &mut current_message, &mut current_role, &mut pending_user_content, &mut pending_assistant_content, &mut pending_tool_results, &mut pending_images, model);
                if let Some(last) = history.last_mut() {
                    if last.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false) {
                        last["assistantResponseMessage"]["toolUses"] = Json::Array(tool_uses);
                    }
                }
                current_role = None;
            }
        }
    }

    if current_role.is_some() {
        flush(&mut history, &mut current_message, &mut current_role, &mut pending_user_content, &mut pending_assistant_content, &mut pending_tool_results, &mut pending_images, model);
    }

    for i in (0..history.len()).rev() {
        if history[i].get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false) {
            current_message = history.remove(i);
            break;
        }
    }

    for item in history.iter_mut() {
        if let Some(user) = item.get_mut("userInputMessage").filter(|v| !v.is_null()) {
            let ctx_empty = user.get("userInputMessageContext").and_then(|c| c.as_object()).map(|c| c.is_empty()).unwrap_or(true);
            if ctx_empty {
                if let Some(obj) = user.as_object_mut() {
                    obj.remove("userInputMessageContext");
                }
            }
            if user.get("modelId").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                user["modelId"] = json!(model);
            }
        }
    }

    // Merge consecutive user turns (Kiro requires alternating roles).
    let mut merged: Vec<Json> = Vec::new();
    for current in history {
        let is_user = current.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false);
        let prev_user = merged.last().map(|p| p.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false)).unwrap_or(false);
        if is_user && prev_user {
            let prev = merged.last_mut().unwrap();
            let cur_content = current["userInputMessage"]["content"].as_str().unwrap_or("").to_string();
            let prev_content = prev["userInputMessage"]["content"].as_str().unwrap_or("").to_string();
            prev["userInputMessage"]["content"] = json!(if prev_content.is_empty() {
                cur_content
            } else {
                format!("{prev_content}\n\n{cur_content}")
            });
            let cur_ctx = current["userInputMessage"].get("userInputMessageContext").cloned();
            if let Some(cur_ctx) = cur_ctx {
                if prev["userInputMessage"].get("userInputMessageContext").map(|v| v.is_null()).unwrap_or(true) {
                    prev["userInputMessage"]["userInputMessageContext"] = cur_ctx;
                } else {
                    if let Some(results) = cur_ctx.get("toolResults").and_then(|v| v.as_array()).cloned() {
                        if !results.is_empty() {
                            let mut all = prev["userInputMessage"]["userInputMessageContext"].get("toolResults").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                            all.extend(results);
                            prev["userInputMessage"]["userInputMessageContext"]["toolResults"] = Json::Array(all);
                        }
                    }
                    if let Some(tools) = cur_ctx.get("tools").and_then(|v| v.as_array()).cloned() {
                        if !tools.is_empty() {
                            let mut all = prev["userInputMessage"]["userInputMessageContext"].get("tools").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                            all.extend(tools);
                            prev["userInputMessage"]["userInputMessageContext"]["tools"] = Json::Array(all);
                        }
                    }
                }
            }
        } else {
            merged.push(current);
        }
    }

    if current_message.is_null() {
        current_message = json!({"userInputMessage": {"content": "", "modelId": model}});
    }
    (merged, current_message)
}

fn extract_claude_system_text(system: Option<&Json>) -> String {
    match system {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Array(parts)) => parts
            .iter()
            .filter_map(|s| match s {
                Json::String(text) => Some(text.clone()),
                other => other.get("text").and_then(|v| v.as_str()).map(|t| t.to_string()),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Build a Kiro payload directly from a Claude Messages API request body.
pub fn claude_to_kiro_request(
    model: &str,
    body: Json,
    _stream: bool,
    credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let intent = resolve_model_intent(model);
    let upstream_model = intent.upstream.clone();

    let messages: Vec<Json> = body.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let (history, current_message) = convert_claude_messages_to_kiro(&messages, &upstream_model);
    claude_kiro_payload(&intent, &body, credentials, history, current_message)
}

fn claude_kiro_payload(
    intent: &super::kiro_shared::KiroModelIntent,
    body: &Json,
    credentials: Option<&mut Credentials>,
    history: Vec<Json>,
    current_message: Json,
) -> Json {
    let upstream_model = intent.upstream.as_str();
    let tools = body.get("tools").cloned().unwrap_or(Json::Null);
    let (tool_specs, name_map) = normalize_kiro_tool_specs(Some(&tools));

    let thinking_body = apply_thinking_override(body, intent.thinking_override.as_ref());
    let thinking_budget = resolve_thinking_budget(Some(&thinking_body), None, Some(&intent.model));
    let additional_model_request_fields = nr_data::kiro_constants::build_kiro_additional_model_request_fields_for_model(Some(&thinking_body), Some(upstream_model));
    let uses_native_gpt_effort = nr_data::kiro_constants::uses_kiro_native_gpt_effort(Some(&thinking_body), Some(upstream_model));

    let creds_psd = credentials.as_ref().and_then(|c| c.psd());
    let profile_arn = resolve_profile_arn(creds_psd);

    let mut system_prompt_parts: Vec<String> = Vec::new();
    if let Some(budget) = thinking_budget {
        if !uses_native_gpt_effort {
            system_prompt_parts.push(nr_data::kiro_constants::build_thinking_system_prefix(Some(budget.clamp(0, u32::MAX as i64) as u32)));
        }
    }
    if intent.agentic {
        system_prompt_parts.push(
            nr_data::export("open-sse/config/kiroConstants", "KIRO_AGENTIC_SYSTEM_PROMPT")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    let system_instruction = extract_claude_system_text(body.get("system"));
    if !system_instruction.is_empty() {
        system_prompt_parts.push(system_instruction);
    }
    let system_prompt = system_prompt_parts.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("\n\n");
    let timestamp = chrono::Utc::now().to_rfc3339();
    let current_time_context = format!("[Context: Current time is {timestamp}]");
    let content_prefix = [system_prompt.clone(), current_time_context.clone()]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");

    let headers = creds_psd.and_then(|p| p.get("rawHeaders")).and_then(|v| v.as_object());
    let identity = nr_common::session_manager::resolve_session_identity(headers, body, credentials.as_ref().and_then(|c| c.connection_id()), None, "kiro");
    let conversation_id = identity.session_id;
    let (replay_history, replay_current, _replayed) = replay_store()
        .lock()
        .map(|mut store| store.apply(
            Some(&conversation_id),
            credentials.as_ref().and_then(|c| c.connection_id()),
            upstream_model,
            &system_prompt,
            &content_prefix,
            &current_time_context,
            &history,
            Some(&current_message),
        ))
        .unwrap_or_else(|_| (history.clone(), current_message.clone(), false));

    let canonical = canonicalize_kiro_conversation(Some(&Json::Array(replay_history)), Some(&replay_current), upstream_model, &tool_specs, &name_map);
    if !canonical.get("valid").and_then(|v| v.as_bool()).unwrap_or(false) {
        tracing::error!(
            "[Kiro] refusing invalid conversation (claude → kiro): {}",
            canonical.get("errors").and_then(|e| e.as_array()).map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_else(|| "unknown".to_string())
        );
        return Json::Null;
    }
    let replay_current = canonical.get("currentMessage").and_then(|c| c.get("userInputMessage")).cloned().unwrap_or(Json::Null);

    let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).filter(|v| *v > 0).unwrap_or(32000);
    let temperature = body.get("temperature").filter(|v| !v.is_null()).cloned();
    let top_p = body.get("top_p").filter(|v| !v.is_null()).cloned();

    let mut user_input = json!({
        "content": replay_current.get("content").and_then(|v| v.as_str()).unwrap_or(""),
        "modelId": upstream_model,
        "origin": "AI_EDITOR"
    });
    if let Some(ctx) = replay_current.get("userInputMessageContext").filter(|v| !v.is_null()) {
        user_input["userInputMessageContext"] = ctx.clone();
    }
    if let Some(images) = replay_current.get("images").filter(|v| v.as_array().map(|a| !a.is_empty()).unwrap_or(false)) {
        user_input["images"] = images.clone();
    }

    let mut payload = json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "conversationId": conversation_id,
            "currentMessage": {"userInputMessage": user_input},
            "history": canonical.get("history").cloned().unwrap_or(Json::Array(Vec::new()))
        }
    });
    if !profile_arn.is_empty() {
        payload["profileArn"] = json!(profile_arn);
    }
    if let Some(fields) = additional_model_request_fields.filter(|v| !v.is_null()) {
        payload["additionalModelRequestFields"] = fields;
    }

    let mut inference = serde_json::Map::new();
    inference.insert("maxTokens".into(), json!(max_tokens));
    if let Some(t) = temperature.clone() {
        inference.insert("temperature".into(), t);
    }
    if let Some(p) = top_p {
        inference.insert("topP".into(), p);
    }
    payload["inferenceConfig"] = Json::Object(inference);

    payload["_kiroUpstreamModel"] = json!(upstream_model);
    let restored = restored_tool_names(&name_map);
    if !restored.is_empty() {
        payload["_toolNameMap"] = Json::Object(restored);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_to_kiro_basic() {
        let body = json!({
            "system": [{"type": "text", "text": "be terse"}],
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1000
        });
        let result = claude_to_kiro_request("kiro-claude-sonnet-4", body, true, None, &Default::default());
        assert!(!result.is_null(), "{result}");
        let content = result["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap();
        assert!(content.contains("be terse"), "{content}");
        assert!(content.ends_with("hello"), "{content}");
        assert_eq!(result["inferenceConfig"]["maxTokens"], 1000);
    }

    #[test]
    fn test_claude_to_kiro_tool_use_and_result() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "search"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "search", "input": {"q": "x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "found"}
                ]},
                {"role": "user", "content": "next"}
            ],
            "tools": [{"name": "search", "description": "d", "input_schema": {"type": "object"}}]
        });
        let result = claude_to_kiro_request("kiro-claude-sonnet-4", body, true, None, &Default::default());
        assert!(!result.is_null(), "{result}");
        let history = result["conversationState"]["history"].as_array().unwrap();
        let has_call = history.iter().any(|t| t.get("assistantResponseMessage").and_then(|a| a.get("toolUses")).and_then(|u| u.as_array()).map(|u| !u.is_empty()).unwrap_or(false));
        assert!(has_call, "{result}");
        // Tool specs land on the current message only.
        let current_tools = result["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"].as_array();
        assert_eq!(current_tools.map(|t| t.len()), Some(1), "{result}");
    }

    #[test]
    fn test_claude_to_kiro_tool_result_image_becomes_user_image() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "shot"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "shot", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                    ]}
                ]},
                {"role": "user", "content": "next"}
            ],
            "tools": [{"name": "shot", "input_schema": {"type": "object"}}]
        });
        let result = claude_to_kiro_request("kiro-claude-sonnet-4", body, true, None, &Default::default());
        assert!(!result.is_null(), "{result}");
        let text = serde_json::to_string(&result).unwrap();
        assert!(text.contains("(image attached)"), "{result}");
    }
}
