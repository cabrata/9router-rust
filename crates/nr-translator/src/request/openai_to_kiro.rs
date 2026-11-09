//! OpenAI to Kiro request translator. Port of open-sse/translator/request/openai-to-kiro.js.

use nr_core::{Credentials, Json};
use serde_json::json;

use crate::concerns::image::parse_data_uri;
use super::kiro_shared::{
    apply_thinking_override, replay_store, resolve_model_intent, resolve_profile_arn,
    resolve_thinking_budget, restored_tool_names,
};
use crate::concerns::kiroConversation::{
    canonicalize_kiro_conversation, kiro_empty_user_content, normalize_kiro_tool_specs,
};
use crate::schema::{claude_blocks, openai_blocks, roles};
use crate::TranslationOptions;

fn image_format(media_type: &str) -> String {
    media_type.split('/').nth(1).unwrap_or(media_type).to_string()
}

fn push_image_from_url(url: &str, text_parts: &mut Vec<String>, pending_images: &mut Vec<Json>) {
    if let Some(parsed) = parse_data_uri(url) {
        pending_images.push(json!({"format": image_format(&parsed.mime_type), "source": {"bytes": parsed.base64}}));
    } else if url.starts_with("http://") || url.starts_with("https://") {
        // Kiro only supports base64 — fall back to URL text.
        text_parts.push(format!("[Image: {url}]"));
    }
}

fn convert_messages(messages: &[Json], model: &str) -> (Vec<Json>, Json) {
    let mut history: Vec<Json> = Vec::new();
    let mut pending_user_content: Vec<String> = Vec::new();
    let mut pending_assistant_content: Vec<String> = Vec::new();
    let mut pending_tool_results: Vec<Json> = Vec::new();
    let mut pending_images: Vec<Json> = Vec::new();
    let mut current_role: Option<String> = None;
    let mut current_message = Json::Null;

    let flush_pending = |history: &mut Vec<Json>,
                         current_message: &mut Json,
                         current_role: &mut Option<String>,
                         user_content: &mut Vec<String>,
                         assistant_content: &mut Vec<String>,
                         tool_results: &mut Vec<Json>,
                         images: &mut Vec<Json>| {
        match current_role.as_deref() {
            Some("user") => {
                let text = user_content.join("\n\n").trim().to_string();
                let content = if text.is_empty() {
                    kiro_empty_user_content(!tool_results.is_empty()).to_string()
                } else {
                    text
                };
                let mut user_msg = json!({"userInputMessage": {"content": content, "modelId": ""}});
                if !images.is_empty() {
                    user_msg["userInputMessage"]["images"] = Json::Array(std::mem::take(images));
                }
                if !tool_results.is_empty() {
                    user_msg["userInputMessage"]["userInputMessageContext"] =
                        json!({"toolResults": std::mem::take(tool_results)});
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
    };

    for msg in messages {
        let msg_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let was_system = msg_role == roles::SYSTEM;
        let role = if msg_role == roles::SYSTEM || msg_role == roles::TOOL { roles::USER } else { msg_role };

        if Some(role) != current_role.as_deref() && current_role.is_some() {
            flush_pending(&mut history, &mut current_message, &mut current_role, &mut pending_user_content, &mut pending_assistant_content, &mut pending_tool_results, &mut pending_images);
        }
        current_role = Some(role.to_string());

        if role == roles::USER {
            let mut user_text = String::new();
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                user_text = text.to_string();
            } else if let Some(parts) = msg.get("content").and_then(|v| v.as_array()) {
                let mut text_parts: Vec<String> = Vec::new();
                for part in parts {
                    let kind = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if kind == openai_blocks::TEXT || part.get("text").and_then(|v| v.as_str()).is_some() {
                        text_parts.push(part.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string());
                    } else if kind == openai_blocks::IMAGE_URL {
                        let url = part.get("image_url").and_then(|u| u.get("url").and_then(|x| x.as_str())).unwrap_or("");
                        push_image_from_url(url, &mut text_parts, &mut pending_images);
                    } else if kind == claude_blocks::IMAGE {
                        let source = part.get("source");
                        if source.and_then(|s| s.get("type")).and_then(|v| v.as_str()) == Some("base64") {
                            if let Some(data) = source.and_then(|s| s.get("data")).and_then(|v| v.as_str()) {
                                let media_type = source.and_then(|s| s.get("media_type")).and_then(|v| v.as_str()).unwrap_or("image/png");
                                pending_images.push(json!({"format": image_format(media_type), "source": {"bytes": data}}));
                            }
                        }
                    } else if kind == claude_blocks::TOOL_RESULT {
                        let content_text = match part.get("content") {
                            Some(Json::String(s)) => s.clone(),
                            Some(Json::Array(arr)) => arr.iter().filter_map(|c| c.get("text").and_then(|v| v.as_str())).collect::<Vec<_>>().join("\n"),
                            _ => String::new(),
                        };
                        let is_error = part.get("is_error").map(nr_core::json::truthy).unwrap_or(false);
                        pending_tool_results.push(json!({
                            "toolUseId": part.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or(""),
                            "status": if is_error { "error" } else { "success" },
                            "content": [{"text": content_text}]
                        }));
                    }
                }
                user_text = text_parts.join("\n");
            }

            // <instructions> tags: Claude models treat these as authoritative directives.
            if !user_text.is_empty() {
                pending_user_content.push(if was_system {
                    format!("<instructions>\n{user_text}\n</instructions>")
                } else {
                    user_text
                });
            }

            if msg_role == roles::TOOL {
                let tool_content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let is_error = msg.get("is_error").map(nr_core::json::truthy).unwrap_or(false);
                pending_tool_results.push(json!({
                    "toolUseId": msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "status": if is_error { "error" } else { "success" },
                    "content": [{"text": tool_content}]
                }));
            }
        } else if role == roles::ASSISTANT {
            let mut text_content = String::new();
            let mut tool_uses: Vec<Json> = Vec::new();
            if let Some(parts) = msg.get("content").and_then(|v| v.as_array()) {
                let texts: Vec<&str> = parts.iter().filter_map(|b| b.get("text").and_then(|v| v.as_str())).collect();
                text_content = texts.join("\n").trim().to_string();
                for part in parts {
                    if part.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_USE) {
                        tool_uses.push(part.clone());
                    }
                }
            } else if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                text_content = text.trim().to_string();
            }
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                if !tool_calls.is_empty() {
                    tool_uses = tool_calls.clone();
                }
            }
            if !text_content.is_empty() {
                pending_assistant_content.push(text_content);
            }
            if !tool_uses.is_empty() {
                flush_pending(&mut history, &mut current_message, &mut current_role, &mut pending_user_content, &mut pending_assistant_content, &mut pending_tool_results, &mut pending_images);
                if let Some(last) = history.last_mut() {
                    if last.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false) {
                        let uses: Vec<Json> = tool_uses.iter().map(|tc| {
                            if let Some(func) = tc.get("function") {
                                json!({
                                    "toolUseId": tc.get("id").and_then(|v| v.as_str()).unwrap_or(&nr_common::session_manager::generate_binary_style_id()),
                                    "name": func.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                    "input": crate::concerns::json::safe_parse_json(func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}"), Json::Object(Default::default()))
                                })
                            } else {
                                json!({
                                    "toolUseId": tc.get("id").and_then(|v| v.as_str()).unwrap_or(&nr_common::session_manager::generate_binary_style_id()),
                                    "name": tc.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                    "input": tc.get("input").cloned().unwrap_or(Json::Object(Default::default()))
                                })
                            }
                        }).collect();
                        last["assistantResponseMessage"]["toolUses"] = Json::Array(uses);
                    }
                }
                current_role = None;
            }
        }
    }

    if current_role.is_some() {
        flush_pending(&mut history, &mut current_message, &mut current_role, &mut pending_user_content, &mut pending_assistant_content, &mut pending_tool_results, &mut pending_images);
    }

    for i in (0..history.len()).rev() {
        if history[i].get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false) {
            current_message = history.remove(i);
            break;
        }
    }

    for item in history.iter_mut() {
        if let Some(user) = item.get_mut("userInputMessage").filter(|v| !v.is_null()) {
            let ctx_empty = user
                .get("userInputMessageContext")
                .and_then(|c| c.as_object())
                .map(|c| c.is_empty())
                .unwrap_or(true);
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

    // Merge consecutive user messages (Kiro requires alternating user/assistant).
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

fn build_kiro_payload(
    origin: &str,
    model: &str,
    body: &Json,
    stream: bool,
    credentials: Option<&mut Credentials>,
    system_instruction: Option<String>,
) -> Json {
    let _ = stream;
    let messages: Vec<Json> = body.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let tools = body.get("tools").cloned().unwrap_or(Json::Null);
    let max_tokens = match body.get("max_tokens").and_then(|v| v.as_u64()) {
        Some(v) if v > 0 => v,
        _ => 32000,
    };
    let temperature = body.get("temperature").cloned();
    let top_p = body.get("top_p").cloned();

    let intent = resolve_model_intent(model);
    let upstream_model = intent.upstream;
    let agentic = intent.agentic;
    let thinking_body = apply_thinking_override(body, intent.thinking_override.as_ref());
    let thinking_budget = resolve_thinking_budget(Some(&thinking_body), None, Some(&intent.model));
    let additional_model_request_fields = nr_data::kiro_constants::build_kiro_additional_model_request_fields_for_model(Some(&thinking_body), Some(&upstream_model));
    let uses_native_gpt_effort = nr_data::kiro_constants::uses_kiro_native_gpt_effort(Some(&thinking_body), Some(&upstream_model));

    let (tool_specs, name_map) = normalize_kiro_tool_specs(Some(&tools));
    let (history, current_message) = convert_messages(&messages, &upstream_model);

    // api_key/idc/external_idp must never use the shared default ARN: it belongs
    // to another account and CodeWhisperer answers 403 "bearer token invalid".
    let creds_psd = credentials.as_ref().and_then(|c| c.psd());
    let profile_arn = resolve_profile_arn(creds_psd);

    let timestamp = chrono::Utc::now().to_rfc3339();
    let mut system_prompt_parts: Vec<String> = Vec::new();
    if let Some(budget) = thinking_budget {
        if !uses_native_gpt_effort {
            system_prompt_parts.push(nr_data::kiro_constants::build_thinking_system_prefix(Some(budget.clamp(0, u32::MAX as i64) as u32)));
        }
    }
    if agentic {
        system_prompt_parts.push(
            nr_data::export("open-sse/config/kiroConstants", "KIRO_AGENTIC_SYSTEM_PROMPT")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    if let Some(text) = system_instruction.filter(|s| !s.is_empty()) {
        system_prompt_parts.push(text);
    }
    let system_prompt = system_prompt_parts.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("\n\n");
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
            &upstream_model,
            &system_prompt,
            &content_prefix,
            &current_time_context,
            &history,
            Some(&current_message),
        ))
        .unwrap_or_else(|_| (history.clone(), current_message.clone(), false));

    let canonical = canonicalize_kiro_conversation(Some(&Json::Array(replay_history)), Some(&replay_current), &upstream_model, &tool_specs, &name_map);
    let valid = canonical.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
    if !valid {
        // canonicalize already flattened structured tools once; still invalid means
        // the body can't ship — Kiro would answer 400 REQUEST_BODY_INVALID. Return a
        // minimal single-turn payload so the caller can render a clean local error.
        tracing::error!(
            "[Kiro] refusing invalid conversation ({origin} → kiro): {}",
            canonical.get("errors").and_then(|e| e.as_array()).map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_else(|| "unknown".to_string())
        );
        return Json::Null;
    }
    let replay_current = canonical.get("currentMessage").and_then(|c| c.get("userInputMessage")).cloned().unwrap_or(Json::Null);

    let mut conversation_state = json!({
        "chatTriggerType": "MANUAL",
        "conversationId": conversation_id,
        "currentMessage": {
            "userInputMessage": {
                "content": replay_current.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "modelId": upstream_model,
                "origin": origin
            }
        },
        "history": canonical.get("history").cloned().unwrap_or(Json::Array(Vec::new()))
    });
    if let Some(ctx) = replay_current.get("userInputMessageContext") {
        if !ctx.is_null() {
            conversation_state["currentMessage"]["userInputMessage"]["userInputMessageContext"] = ctx.clone();
        }
    }
    if let Some(images) = replay_current.get("images") {
        if images.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            conversation_state["currentMessage"]["userInputMessage"]["images"] = images.clone();
        }
    }

    let mut payload = json!({"conversationState": conversation_state});
    if !profile_arn.is_empty() {
        payload["profileArn"] = json!(profile_arn);
    }
    if let Some(fields) = additional_model_request_fields {
        if !fields.is_null() {
            payload["additionalModelRequestFields"] = fields;
        }
    }
    if max_tokens > 0 || temperature.as_ref().map(|v| !v.is_null()).unwrap_or(false) || top_p.as_ref().map(|v| !v.is_null()).unwrap_or(false) {
        let mut inference = serde_json::Map::new();
        if max_tokens > 0 {
            inference.insert("maxTokens".into(), json!(max_tokens));
        }
        if let Some(t) = temperature.clone().filter(|v| !v.is_null()) {
            inference.insert("temperature".into(), t);
        }
        if let Some(p) = top_p.clone().filter(|v| !v.is_null()) {
            inference.insert("topP".into(), p);
        }
        payload["inferenceConfig"] = Json::Object(inference);
    }

    // Route the upstream model id to the executor and keep sanitized tool names
    // reversible so tool calls stream back under the client's own names.
    payload["_kiroUpstreamModel"] = json!(upstream_model);
    let restored = restored_tool_names(&name_map);
    if !restored.is_empty() {
        payload["_toolNameMap"] = Json::Object(restored);
    }
    payload
}

/// Convert OpenAI Chat Completions request to Kiro / AWS CodeWhisperer format.
pub fn openai_to_kiro_request(
    model: &str,
    body: Json,
    stream: bool,
    credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    build_kiro_payload("AI_EDITOR", model, &body, stream, credentials, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_to_kiro_minimal() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let result = openai_to_kiro_request("kiro-claude-sonnet-4", body, true, None, &Default::default());
        assert!(!result.is_null(), "{result}");
        assert_eq!(result["conversationState"]["chatTriggerType"], "MANUAL");
        // Single user turn: nothing left in history, the turn is the current message.
        assert_eq!(result["conversationState"]["history"].as_array().map(|h| h.len()), Some(0));
        let content = result["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap();
        assert!(content.contains("[Context: Current time is"), "{content}");
        assert!(content.ends_with("hi"), "{content}");
        assert_eq!(result["conversationState"]["currentMessage"]["userInputMessage"]["origin"], "AI_EDITOR");
        assert_eq!(result["inferenceConfig"]["maxTokens"], 32000);
        assert!(result.get("_kiroUpstreamModel").and_then(|v| v.as_str()).unwrap().contains("sonnet"));
    }

    #[test]
    fn test_openai_to_kiro_tool_call_and_result() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "search"},
                {"role": "assistant", "content": "", "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "search", "arguments": "{\"q\":\"x\"}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": "found"},
                {"role": "assistant", "content": "done"},
                {"role": "user", "content": "next"}
            ],
            "tools": [{"type": "function", "function": {"name": "search", "description": "d", "parameters": {"type": "object"}}}]
        });
        let result = openai_to_kiro_request("kiro-claude-sonnet-4", body, false, None, &Default::default());
        assert!(!result.is_null(), "{result}");
        let history = result["conversationState"]["history"].as_array().unwrap();
        let has_call = history.iter().any(|t| t.get("assistantResponseMessage").and_then(|a| a.get("toolUses")).and_then(|u| u.as_array()).map(|u| !u.is_empty()).unwrap_or(false));
        assert!(has_call, "{result}");
    }

    #[test]
    fn test_openai_to_kiro_system_wraps_instructions() {
        let body = json!({"messages": [{"role": "system", "content": "be terse"}, {"role": "user", "content": "hi"}]});
        let result = openai_to_kiro_request("kiro-claude-sonnet-4", body, true, None, &Default::default());
        // System turns become user turns wrapped in <instructions>; consecutive user
        // turns merge, so the text lands in the single current message.
        let content = result["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap();
        assert!(content.contains("<instructions>\nbe terse\n</instructions>"), "{content}");
        assert!(content.ends_with("hi"), "{content}");
    }

    #[test]
    fn test_openai_to_kiro_tool_image_falls_back_to_text() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
        ]}]});
        let result = openai_to_kiro_request("kiro-claude-sonnet-4", body, false, None, &Default::default());
        let content = result["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap();
        assert!(content.contains("[Image: https://example.com/a.png]"), "{content}");
    }
}
