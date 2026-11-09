//! Kiro conversation/history management. Port of open-sse/translator/concerns/kiroConversation.js.
//! Reconciles conversation shape between client format and Kiro's GenerateAssistantResponse API.

use std::collections::HashMap;

use nr_core::Json;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::json;

fn kiro_tool_constant(name: &str) -> u64 {
    nr_data::export("open-sse/config/kiroConstants", name)
        .and_then(|v| v.as_u64())
        .unwrap_or(64)
}

fn max_tool_name() -> usize {
    kiro_tool_constant("KIRO_TOOL_NAME_MAX_LENGTH") as usize
}

fn max_tool_description() -> usize {
    kiro_tool_constant("KIRO_TOOL_DESCRIPTION_MAX_LENGTH") as usize
}

fn max_tool_id() -> usize {
    kiro_tool_constant("KIRO_TOOL_ID_MAX_LENGTH") as usize
}

pub const KIRO_TOOL_RESULTS_PLACEHOLDER: &str = "Tool results provided.";
pub const KIRO_EMPTY_USER_PLACEHOLDER: &str = "continue";

/// Placeholder content for a user turn with no text of its own.
/// Kiro rejects user turns with empty `content`, so a turn that only carries
/// tool results needs placeholder text. It must not read like a user
/// instruction: with "continue", models answer the word itself ("Nothing in
/// progress to continue") and drop the task they were in the middle of.
pub fn kiro_empty_user_content(has_tool_results: bool) -> &'static str {
    if has_tool_results {
        KIRO_TOOL_RESULTS_PLACEHOLDER
    } else {
        KIRO_EMPTY_USER_PLACEHOLDER
    }
}

fn clone_value(value: &Json) -> Json {
    serde_json::from_str(&value.to_string()).unwrap_or(Json::Null)
}

fn text_of(value: Option<&Json>) -> String {
    let Some(value) = value else { return String::new() };
    match value {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn append_text(target: &mut Json, extra: &str) {
    if extra.is_empty() {
        return;
    }
    let existing = target.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    target["content"] = if existing.is_empty() {
        Json::String(extra.to_string())
    } else {
        Json::String(format!("{existing}\n\n{extra}"))
    };
}

fn trim_code_points(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn unique_name(raw_name: Option<&str>, index: usize, used_names: &mut std::collections::HashSet<String>) -> String {
    static PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-zA-Z0-9_-]").unwrap());
    static EDGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^_+|_+$").unwrap());
    let cleaned = EDGE.replace_all(
        &PATTERN.replace_all(raw_name.unwrap_or("").trim(), "_"),
        "",
    )
    .to_string();
    let base = trim_code_points(
        if cleaned.is_empty() {
            format!("tool_{}", index + 1)
        } else {
            cleaned
        }
        .as_str(),
        max_tool_name(),
    );
    let mut candidate = base.clone();
    let mut suffix = 2;
    while used_names.contains(&candidate) {
        let tail = format!("_{suffix}");
        suffix += 1;
        let keep = max_tool_name().saturating_sub(tail.len());
        candidate = format!("{}{}", &base[..keep], tail);
    }
    used_names.insert(candidate.clone());
    candidate
}

fn clean_schema_value(value: &Json) -> Json {
    match value {
        Json::Array(arr) => Json::Array(arr.iter().map(clean_schema_value).collect()),
        Json::Object(obj) => {
            let mut cleaned = serde_json::Map::new();
            for (key, child) in obj {
                if key == "additionalProperties" {
                    continue;
                }
                if key == "required" && child.as_array().map(|a| a.is_empty()).unwrap_or(false) {
                    continue;
                }
                cleaned.insert(key.clone(), clean_schema_value(child));
            }
            Json::Object(cleaned)
        }
        other => other.clone(),
    }
}

fn normalize_root_schema(schema: &Json) -> Json {
    let empty = Json::Object(Default::default());
    let source = if schema.is_object() { schema } else { &empty };
    let mut cleaned = clean_schema_value(source);
    cleaned["type"] = json!("object");
    let props_is_obj = cleaned.get("properties").map(|p| p.is_object()).unwrap_or(false);
    if !props_is_obj {
        cleaned["properties"] = Json::Object(Default::default());
    }
    if let Some(required) = cleaned.get("required").and_then(|v| v.as_array()).cloned() {
        let properties = cleaned.get("properties").and_then(|p| p.as_object()).unwrap();
        let mut deduped: Vec<Json> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for name in required.iter().filter_map(|v| v.as_str()) {
            if !properties.contains_key(name) || !seen.insert(name.to_string()) {
                continue;
            }
            deduped.push(json!(name));
        }
        if deduped.is_empty() {
            if let Some(obj) = cleaned.as_object_mut() {
                obj.remove("required");
            }
        } else {
            cleaned["required"] = Json::Array(deduped);
        }
    }
    cleaned
}

/// Normalize OpenAI- or Claude-shaped tool definitions into Kiro tool specs.
pub fn normalize_kiro_tool_specs(tools: Option<&Json>) -> (Vec<Json>, HashMap<String, String>) {
    let mut specs: Vec<Json> = Vec::new();
    let mut name_map: HashMap<String, String> = HashMap::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    let Some(arr) = tools.and_then(|t| t.as_array()) else {
        return (specs, name_map);
    };

    for (index, tool) in arr.iter().enumerate() {
        if !tool.is_object() {
            continue;
        }
        let raw_name = tool
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| tool.get("name").and_then(|v| v.as_str()));
        let Some(raw_name) = raw_name.filter(|s| !s.trim().is_empty()) else {
            continue;
        };
        // A repeated definition with the same source name describes the same tool.
        if name_map.contains_key(raw_name) {
            continue;
        }
        let name = unique_name(Some(raw_name), index, &mut used_names);
        name_map.insert(raw_name.to_string(), name.clone());

        let raw_description = tool
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(|v| v.as_str())
            .or_else(|| tool.get("description").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("Tool: {raw_name}"));
        let description = trim_code_points(&raw_description, max_tool_description());
        let schema = tool
            .get("function")
            .and_then(|f| f.get("parameters"))
            .or_else(|| tool.get("parameters"))
            .or_else(|| tool.get("input_schema"))
            .cloned()
            .unwrap_or(Json::Object(Default::default()));
        specs.push(json!({
            "toolSpecification": {
                "name": name,
                "description": description,
                "inputSchema": {"json": normalize_root_schema(&schema)}
            }
        }));
    }

    (specs, name_map)
}

fn tool_call_text(tool_use: Option<&Json>) -> String {
    let name = tool_use.and_then(|t| t.get("name")).and_then(|v| v.as_str()).unwrap_or("unknown");
    let input = tool_use.and_then(|t| t.get("input"));
    format!("[Tool call: {name}({})]", text_of(input.or(Some(&Json::Object(Default::default())))))
}

fn tool_result_text(tool_result: Option<&Json>) -> String {
    let Some(tool_result) = tool_result else { return String::new() };
    let content = match tool_result.get("content").and_then(|c| c.as_array()) {
        Some(arr) => arr
            .iter()
            .map(|part| text_of(Some(part.get("text").unwrap_or(part))))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        None => text_of(Some(tool_result.get("content").unwrap_or(&Json::Null))),
    };
    let status = if tool_result.get("status").and_then(|v| v.as_str()) == Some("error") {
        " (error)"
    } else {
        ""
    };
    format!("[Tool result{status}: {content}]")
}

fn merge_user(target: &mut Json, source: &Json) {
    append_text(target, &text_of(Some(source.get("content").unwrap_or(&Json::Null))));
    if let Some(images) = source.get("images").and_then(|v| v.as_array()).cloned() {
        if !images.is_empty() {
            let mut all = target.get("images").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            all.extend(images);
            target["images"] = Json::Array(all);
        }
    }
    if let Some(results) = source
        .get("userInputMessageContext")
        .and_then(|c| c.get("toolResults"))
        .and_then(|v| v.as_array())
        .cloned()
    {
        if !results.is_empty() {
            if target.get("userInputMessageContext").is_none() {
                target["userInputMessageContext"] = Json::Object(Default::default());
            }
            let mut all = target
                .get("userInputMessageContext")
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            all.extend(results);
            target["userInputMessageContext"]["toolResults"] = Json::Array(all);
        }
    }
}

fn merge_assistant(target: &mut Json, source: &Json) {
    append_text(target, &text_of(Some(source.get("content").unwrap_or(&Json::Null))));
    if let Some(uses) = source.get("toolUses").and_then(|v| v.as_array()).cloned() {
        if !uses.is_empty() {
            let mut all = target.get("toolUses").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            all.extend(uses);
            target["toolUses"] = Json::Array(all);
        }
    }
}

fn normalize_turns(history: Option<&Json>, current_message: Option<&Json>, model_id: &str) -> Vec<Json> {
    let mut raw_turns: Vec<Json> = Vec::new();
    if let Some(arr) = history.and_then(|h| h.as_array()) {
        raw_turns.extend(arr.iter().cloned());
    }
    if let Some(cur) = current_message {
        raw_turns.push(cur.clone());
    }
    let mut turns: Vec<Json> = Vec::new();

    for raw in raw_turns {
        let is_user = raw.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false);
        let is_assistant = raw.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false);
        if is_user == is_assistant {
            continue;
        }
        let turn = if is_user {
            json!({"userInputMessage": clone_value(raw.get("userInputMessage").unwrap_or(&Json::Null))})
        } else {
            json!({"assistantResponseMessage": clone_value(raw.get("assistantResponseMessage").unwrap_or(&Json::Null))})
        };
        let previous = turns.last();
        if turn.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false)
            && previous.map(|p| p.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false)).unwrap_or(false)
        {
            let previous = turns.last_mut().unwrap();
            let user = turn.get("userInputMessage").cloned().unwrap();
            merge_user(previous.get_mut("userInputMessage").unwrap(), &user);
        } else if turn.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false)
            && previous.map(|p| p.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false)).unwrap_or(false)
        {
            let previous = turns.last_mut().unwrap();
            let assistant = turn.get("assistantResponseMessage").cloned().unwrap();
            merge_assistant(previous.get_mut("assistantResponseMessage").unwrap(), &assistant);
        } else {
            turns.push(turn);
        }
    }

    if turns.first().map(|t| t.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false)).unwrap_or(false) {
        turns.insert(0, json!({"userInputMessage": {"content": "continue", "modelId": model_id}}));
    }
    let ends_with_assistant = turns.last().map(|t| t.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false)).unwrap_or(false);
    if turns.is_empty() || ends_with_assistant {
        turns.push(json!({"userInputMessage": {"content": "continue", "modelId": model_id}}));
    }

    for turn in turns.iter_mut() {
        if turn.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false) {
            let user = turn.get_mut("userInputMessage").unwrap();
            let content = text_of(Some(user.get("content").unwrap_or(&Json::Null))).trim().to_string();
            let has_results = user
                .get("userInputMessageContext")
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            user["content"] = if content.is_empty() {
                Json::String(kiro_empty_user_content(has_results).to_string())
            } else {
                Json::String(content)
            };
            let model_present = user.get("modelId").map(|v| !v.is_null()).unwrap_or(false);
            if !model_present {
                user["modelId"] = json!(model_id);
            }
            if user
                .get("userInputMessageContext")
                .and_then(|c| c.as_object())
                .map(|c| c.contains_key("tools"))
                .unwrap_or(false)
            {
                if let Some(ctx) = user.get_mut("userInputMessageContext").and_then(|c| c.as_object_mut()) {
                    ctx.remove("tools");
                }
            }
        } else {
            let assistant = turn.get_mut("assistantResponseMessage").unwrap();
            let content = text_of(Some(assistant.get("content").unwrap_or(&Json::Null))).trim().to_string();
            assistant["content"] = if content.is_empty() {
                Json::String("...".to_string())
            } else {
                Json::String(content)
            };
        }
    }
    turns
}

fn raw_id(value: Option<&Json>) -> String {
    value.and_then(|v| v.as_str()).unwrap_or("").to_string()
}

static TOOL_ID_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap());
static TOOL_ID_SANITIZE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-zA-Z0-9_-]").unwrap());

fn reserve_tool_id(
    value: Option<&str>,
    turn_index: usize,
    call_index: usize,
    name: &str,
    used_ids: &mut std::collections::HashSet<String>,
) -> String {
    let sanitized = value.map(|v| TOOL_ID_SANITIZE.replace_all(v, "").to_string()).unwrap_or_default();
    let generated = format!("call_msg{turn_index}_tc{call_index}_{}", if name.is_empty() { "tool" } else { name });
    let chosen = if !sanitized.is_empty() && TOOL_ID_PATTERN.is_match(&sanitized) {
        sanitized
    } else {
        generated
    };
    let base = trim_code_points(&chosen, max_tool_id());
    let mut candidate = base.clone();
    let mut suffix = 2;
    while used_ids.contains(&candidate) {
        let tail = format!("_{suffix}");
        suffix += 1;
        let keep = max_tool_id().saturating_sub(tail.len());
        candidate = format!("{}{}", &base[..keep], tail);
    }
    used_ids.insert(candidate.clone());
    candidate
}

fn normalize_tool_input(input: Option<&Json>) -> Option<Json> {
    let Some(input) = input else { return Some(Json::Object(Default::default())) };
    match input {
        Json::Object(_) => Some(clone_value(input)),
        Json::String(s) => match serde_json::from_str::<Json>(s) {
            Ok(v) if v.is_object() => Some(v),
            _ => None,
        },
        Json::Null => Some(Json::Object(Default::default())),
        _ => None,
    }
}

fn normalize_tool_result(result: Option<&Json>) -> Json {
    let Some(result) = result else { return json!({"toolUseId": "", "status": "success", "content": [{"text": ""}]}) };
    let content = match result.get("content").and_then(|c| c.as_array()).cloned() {
        Some(arr) => {
            let mapped: Vec<Json> = arr
                .iter()
                .map(|part| json!({ "text": text_of(Some(part.get("text").unwrap_or(part))) }))
                .collect();
            if mapped.is_empty() {
                vec![json!({"text": ""})]
            } else {
                mapped
            }
        }
        None => vec![json!({"text": text_of(Some(result.get("content").unwrap_or(&Json::Null)))})],
    };
    json!({
        "toolUseId": raw_id(result.get("toolUseId")),
        "status": if result.get("status").and_then(|v| v.as_str()) == Some("error") { "error" } else { "success" },
        "content": content
    })
}

fn flatten_results(user_message: &mut Json, results: &[Json]) {
    for result in results {
        let text = tool_result_text(Some(result));
        append_text(user_message, &text);
    }
}

fn clean_user_context(user_message: &mut Json) {
    let Some(ctx) = user_message.get_mut("userInputMessageContext") else { return };
    let is_object = ctx.is_object();
    if !is_object {
        user_message.as_object_mut().unwrap().remove("userInputMessageContext");
        return;
    }
    if ctx.get("toolResults").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
        ctx.as_object_mut().unwrap().remove("toolResults");
    }
    if ctx.get("tools").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
        ctx.as_object_mut().unwrap().remove("tools");
    }
    if ctx.as_object().map(|c| c.is_empty()).unwrap_or(true) {
        user_message.as_object_mut().unwrap().remove("userInputMessageContext");
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_tool_pair(
    assistant: &mut Json,
    user: &mut Json,
    turn_index: usize,
    name_map: &HashMap<String, String>,
    spec_names: &std::collections::HashSet<String>,
    used_ids: &mut std::collections::HashSet<String>,
    repairs: &mut serde_json::Map<String, Json>,
) {
    let calls: Vec<Json> = assistant
        .get("toolUses")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let results: Vec<Json> = user
        .get("userInputMessageContext")
        .and_then(|c| c.get("toolResults"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| normalize_tool_result(Some(r)))
        .collect();

    if calls.is_empty() {
        if !results.is_empty() {
            flatten_results(user, &results);
            increment(repairs, "orphanResults", results.len());
        }
        if let Some(ctx) = user.get_mut("userInputMessageContext").and_then(|c| c.as_object_mut()) {
            ctx.remove("toolResults");
        }
        clean_user_context(user);
        return;
    }

    // key -> indices into call_records sharing the same id
    let mut call_records: Vec<(Json, usize, String, String, Option<Json>, Option<Json>)> = Vec::new();
    let mut queues: HashMap<String, Vec<usize>> = HashMap::new();
    for (call_index, call) in calls.iter().enumerate() {
        let key = raw_id(call.get("toolUseId"));
        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let mapped_name = name_map.get(name).cloned().unwrap_or_else(|| name.to_string());
        let input = normalize_tool_input(call.get("input"));
        queues.entry(key.clone()).or_default().push(call_index);
        call_records.push((call.clone(), call_index, key, mapped_name, input, None));
    }

    let mut orphan_results: Vec<Json> = Vec::new();
    for result in results.into_iter() {
        let id = raw_id(result.get("toolUseId"));
        let matched = queues
            .get(&id)
            .and_then(|indices| indices.iter().copied().find(|i| call_records[*i].5.is_none()));
        if let Some(i) = matched {
            call_records[i].5 = Some(result);
        } else {
            orphan_results.push(result);
        }
    }

    let mut kept_calls: Vec<Json> = Vec::new();
    let mut kept_results: Vec<Json> = Vec::new();
    for (call, call_index, _key, mapped_name, input, result) in call_records {
        let has_spec = spec_names.contains(&mapped_name);
        let valid = result.is_some() && has_spec && input.is_some();
        if !valid {
            append_text(assistant, &tool_call_text(Some(&json!({"name": mapped_name, "input": call.get("input").cloned().unwrap_or(Json::Null)}))));
            if result.is_none() {
                increment(repairs, "missingResults", 1);
            }
            if has_spec && input.is_some() {
                // hasSpec && input != null counted only when invalid due to missing result
            } else {
                increment(repairs, "invalidToolUses", 1);
            }
            if let Some(res) = result {
                flatten_results(user, std::slice::from_ref(&res));
                increment(repairs, "orphanResults", 1);
            }
            continue;
        }
        let tool_use_id = reserve_tool_id(Some(&mapped_name), turn_index, call_index, &mapped_name, used_ids);
        kept_calls.push(json!({
            "toolUseId": tool_use_id,
            "name": mapped_name,
            "input": input.unwrap_or(Json::Object(Default::default()))
        }));
        let mut res = result.unwrap();
        res["toolUseId"] = json!(tool_use_id);
        kept_results.push(res);
    }

    if !orphan_results.is_empty() {
        flatten_results(user, &orphan_results);
        increment(repairs, "orphanResults", orphan_results.len());
    }

    if !kept_calls.is_empty() {
        assistant["toolUses"] = Json::Array(kept_calls);
    } else if let Some(obj) = assistant.as_object_mut() {
        obj.remove("toolUses");
    }
    if user.get("userInputMessageContext").is_none() {
        user["userInputMessageContext"] = Json::Object(Default::default());
    }
    if !kept_results.is_empty() {
        user["userInputMessageContext"]["toolResults"] = Json::Array(kept_results);
    } else if let Some(ctx) = user.get_mut("userInputMessageContext").and_then(|c| c.as_object_mut()) {
        ctx.remove("toolResults");
    }
    clean_user_context(user);
}

fn increment(repairs: &mut serde_json::Map<String, Json>, key: &str, by: usize) {
    let current = repairs.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    repairs.insert(key.to_string(), json!(current + by as u64));
}

/// Validate the final Kiro wire conversation without mutating it.
pub fn validate_kiro_conversation(
    history: Option<&Json>,
    current_message: Option<&Json>,
    tool_specs: &[Json],
) -> (bool, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();
    let mut turns: Vec<Json> = Vec::new();
    if let Some(arr) = history.and_then(|h| h.as_array()) {
        turns.extend(arr.iter().filter(|t| !t.is_null()).cloned());
    }
    if let Some(cur) = current_message.filter(|t| !t.is_null()) {
        turns.push(cur.clone());
    }
    let spec_names = spec_names(tool_specs);
    let mut used_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (index, turn) in turns.iter().enumerate() {
        let expected_user = index % 2 == 0;
        let is_user = turn.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false);
        if is_user != expected_user {
            errors.push(format!("role:{index}"));
        }
        if !is_user {
            let calls: &[Json] = turn
                .get("assistantResponseMessage")
                .and_then(|a| a.get("toolUses"))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let results: &[Json] = turns
                .get(index + 1)
                .and_then(|t| t.get("userInputMessage"))
                .and_then(|u| u.get("userInputMessageContext"))
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let call_ids: Vec<String> = calls.iter().map(|c| raw_id(c.get("toolUseId"))).collect();
            let result_ids: Vec<String> = results.iter().map(|r| raw_id(r.get("toolUseId"))).collect();
            if calls.len() != results.len() || call_ids.iter().any(|id| !result_ids.contains(id)) {
                errors.push(format!("pair:{index}"));
            }
            for call in calls {
                let id = raw_id(call.get("toolUseId"));
                if id.is_empty() || !used_ids.insert(id) {
                    errors.push(format!("id:{index}"));
                }
                let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if !spec_names.contains(name) {
                    errors.push(format!("spec:{index}"));
                }
            }
        } else if index == 0 {
            let has_results = turn
                .get("userInputMessage")
                .and_then(|u| u.get("userInputMessageContext"))
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if has_results {
                errors.push("orphan:0".to_string());
            }
        }
    }
    let current_has_content = current_message
        .and_then(|c| c.get("userInputMessage"))
        .and_then(|u| u.get("content"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !current_has_content {
        errors.push("current".to_string());
    }
    (errors.is_empty(), errors)
}

fn spec_names(tool_specs: &[Json]) -> std::collections::HashSet<String> {
    tool_specs
        .iter()
        .filter_map(|spec| {
            spec.get("toolSpecification")
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Produce a strict Kiro conversation: alternating turns, current user message,
/// adjacent one-to-one tool use/result pairs, and tool specs only on currentMessage.
pub fn canonicalize_kiro_conversation(
    history: Option<&Json>,
    current_message: Option<&Json>,
    model_id: &str,
    tool_specs: &[Json],
    name_map: &HashMap<String, String>,
) -> Json {
    let mut turns = normalize_turns(history, current_message, model_id);
    let mut repairs = serde_json::Map::new();
    repairs.insert("missingResults".into(), json!(0));
    repairs.insert("orphanResults".into(), json!(0));
    repairs.insert("invalidToolUses".into(), json!(0));
    let specs = spec_names(tool_specs);
    let mut used_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut index = 0;
    while index < turns.len() {
        let is_user = turns[index].get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false);
        if index == 0 && is_user {
            let leading: Vec<Json> = turns[0]
                .get("userInputMessage")
                .and_then(|u| u.get("userInputMessageContext"))
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if !leading.is_empty() {
                let user = turns[0].get_mut("userInputMessage").unwrap();
                flatten_results(user, &leading);
                increment(&mut repairs, "orphanResults", leading.len());
                if let Some(ctx) = user.get_mut("userInputMessageContext").and_then(|c| c.as_object_mut()) {
                    ctx.remove("toolResults");
                }
                clean_user_context(user);
            }
        }
        let assistant_is = turns
            .get(index + 1)
            .map(|t| t.get("assistantResponseMessage").map(|v| !v.is_null()).unwrap_or(false))
            .unwrap_or(false);
        let next_user_is = turns
            .get(index + 2)
            .map(|t| t.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false))
            .unwrap_or(false);
        if assistant_is && next_user_is {
            let (left, rest) = turns.split_at_mut(index + 2);
            let assistant = left[index + 1].get_mut("assistantResponseMessage").unwrap();
            let user = rest[0].get_mut("userInputMessage").unwrap();
            reconcile_tool_pair(assistant, user, index + 1, name_map, &specs, &mut used_ids, &mut repairs);
        }
        index += 2;
    }

    let last = turns.len() - 1;
    let current = turns[last].get_mut("userInputMessage").unwrap();
    if current.get("userInputMessageContext").is_none() {
        current["userInputMessageContext"] = Json::Object(Default::default());
    }
    if !tool_specs.is_empty() {
        current["userInputMessageContext"]["tools"] = Json::Array(tool_specs.to_vec());
    }
    clean_user_context(current);

    let message = |turns: &[Json]| json!({
        "history": &turns[..turns.len() - 1],
        "currentMessage": turns[turns.len() - 1],
    });
    let mut parts = message(&turns);
    let (valid, errors) = validate_kiro_conversation(
        parts.get("history"),
        parts.get("currentMessage"),
        tool_specs,
    );
    if !valid {
        // Second chance: flatten every structured tool turn to text, then re-validate.
        flatten_all_structured_tools(&mut turns, &mut repairs);
        parts = message(&turns);
        let (valid, errors) = validate_kiro_conversation(
            parts.get("history"),
            parts.get("currentMessage"),
            tool_specs,
        );
        return finish(parts, repairs, valid, errors);
    }
    finish(parts, repairs, valid, errors)
}

fn finish(mut parts: Json, repairs: serde_json::Map<String, Json>, valid: bool, errors: Vec<String>) -> Json {
    parts["repairs"] = Json::Object(repairs);
    parts["valid"] = json!(valid);
    parts["errors"] = Json::Array(errors.into_iter().map(Json::String).collect());
    parts
}

fn flatten_all_structured_tools(turns: &mut [Json], repairs: &mut serde_json::Map<String, Json>) {
    for turn in turns.iter_mut() {
        if let Some(assistant) = turn.get_mut("assistantResponseMessage").filter(|v| !v.is_null()) {
            if let Some(calls) = assistant.get("toolUses").and_then(|v| v.as_array()).cloned() {
                if !calls.is_empty() {
                    for call in &calls {
                        append_text(assistant, &tool_call_text(Some(call)));
                    }
                    increment(repairs, "invalidToolUses", calls.len());
                    assistant.as_object_mut().unwrap().remove("toolUses");
                }
            }
        }
        if let Some(user) = turn.get_mut("userInputMessage").filter(|v| !v.is_null()) {
            let results: Vec<Json> = user
                .get("userInputMessageContext")
                .and_then(|c| c.get("toolResults"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if !results.is_empty() {
                flatten_results(user, &results);
                increment(repairs, "orphanResults", results.len());
                if let Some(ctx) = user.get_mut("userInputMessageContext").and_then(|c| c.as_object_mut()) {
                    ctx.remove("toolResults");
                }
                clean_user_context(user);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_specs_openai_shape() {
        let tools = json!([
            {"function": {"name": "a__b", "description": "desc", "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}}}
        ]);
        let (specs, name_map) = normalize_kiro_tool_specs(Some(&tools));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0]["toolSpecification"]["name"], "a__b");
        assert_eq!(name_map.get("a__b"), Some(&"a__b".to_string()));
    }

    #[test]
    fn test_kiro_empty_user_content() {
        assert_eq!(kiro_empty_user_content(true), KIRO_TOOL_RESULTS_PLACEHOLDER);
        assert_eq!(kiro_empty_user_content(false), KIRO_EMPTY_USER_PLACEHOLDER);
    }

    #[test]
    fn test_canonicalize_simple_history() {
        let history = json!([
            {"userInputMessage": {"content": "hi", "modelId": "m"}},
            {"assistantResponseMessage": {"content": "hello"}}
        ]);
        let current = json!({"userInputMessage": {"content": "go", "modelId": "m"}});
        let canonical = canonicalize_kiro_conversation(Some(&history), Some(&current), "m", &[], &HashMap::new());
        assert_eq!(canonical["valid"], true, "{}", canonical);
        let history_arr = canonical["history"].as_array().unwrap();
        assert_eq!(history_arr.len(), 2);
        assert_eq!(canonical["currentMessage"]["userInputMessage"]["content"], "go");
    }

    #[test]
    fn test_canonicalize_tool_pair() {
        let (specs, _map) = normalize_kiro_tool_specs(Some(&json!([
            {"function": {"name": "search", "parameters": {"type": "object"}}}
        ])));
        let history = json!([
            {"userInputMessage": {"content": "search please", "modelId": "m"}},
            {"assistantResponseMessage": {"content": "", "toolUses": [{"toolUseId": "t1", "name": "search", "input": {"q": "x"}}]}},
            {"userInputMessage": {"content": "ok", "modelId": "m", "userInputMessageContext": {"toolResults": [{"toolUseId": "t1", "content": [{"text": "result"}]}]}}}
        ]);
        let current = json!({"userInputMessage": {"content": "next", "modelId": "m"}});
        let canonical = canonicalize_kiro_conversation(Some(&history), Some(&current), "m", &specs, &HashMap::new());
        assert_eq!(canonical["valid"], true, "{}", canonical);
    }
}
