//! Web tool-calling shims — faithful port of open-sse/translator/webTools.js,
//! deepseekWebTools.js, and deepseekWebToolBinding.js.
//!
//! Web-cookie providers (DeepSeek Web, Perplexity Web, ChatGPT Web) accept only
//! a single prompt string or have no native function calling. To allow agentic clients
//! to invoke tools, we serialize the OpenAI `tools[]` into a prompt contract and parse
//! tool invocations back into OpenAI `tool_calls`.

use std::collections::{HashMap, HashSet};
use regex::Regex;
use serde_json::{json, Value as Json};

// ── Nonce generation ─────────────────────────────────────────────────────────

/// Generate a deterministic 8-character hex nonce for a tools array.
/// Uses FNV-1a hash of the serialized tools to guarantee consistency between
/// prompt serialization and response parsing for the same tool set.
pub fn get_tool_nonce(tools: &[Json]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let serialized = serde_json::to_string(tools).unwrap_or_default();
    let mut hash: u32 = 0x811c9dc5;
    for byte in serialized.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{:08x}", hash)
}

// ── Tool Name Resolution and Fuzzy Matching ─────────────────────────────────

#[derive(Clone, Debug)]
pub struct RequestedToolName {
    pub original: String,
    pub normalized: String,
}

pub fn normalize_tool_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

pub fn get_requested_tool_names(tools: &[Json]) -> Vec<RequestedToolName> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();

    for tool in tools {
        let fn_obj = tool.get("function").and_then(|f| f.as_object());
        let name = fn_obj
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.trim())
            .unwrap_or("");
        if name.is_empty() || seen.contains(name) {
            continue;
        }
        seen.insert(name.to_string());
        names.push(RequestedToolName {
            original: name.to_string(),
            normalized: normalize_tool_name(name),
        });
    }
    names
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.chars().count();
    }
    if b.is_empty() {
        return a.chars().count();
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, &ca) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_chars.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1)
                .min(prev[j + 1] + 1)
                .min(prev[j] + cost);
        }
        prev.copy_from_slice(&curr);
    }
    prev[b_chars.len()]
}

fn score_tool_name(emitted: &str, requested: &RequestedToolName) -> f64 {
    if emitted == requested.original {
        return 1.0;
    }
    let normalized = normalize_tool_name(emitted);
    if normalized.is_empty() || requested.normalized.is_empty() {
        return 0.0;
    }
    if normalized == requested.normalized {
        return 0.98;
    }

    let shorter = normalized.len().min(requested.normalized.len());
    let longer = normalized.len().max(requested.normalized.len());
    if shorter >= 4 && (normalized.contains(&requested.normalized) || requested.normalized.contains(&normalized)) {
        return 0.86 - (longer - shorter) as f64 / longer.max(1) as f64 / 4.0;
    }

    let distance = levenshtein_distance(&normalized, &requested.normalized);
    let similarity = 1.0 - distance as f64 / longer.max(1) as f64;
    if similarity >= 0.72 {
        similarity
    } else {
        0.0
    }
}

pub fn resolve_requested_tool_name(emitted: &str, requested_tools: &[RequestedToolName]) -> Option<String> {
    if requested_tools.is_empty() {
        return Some(emitted.to_string());
    }

    let mut best_name: Option<String> = None;
    let mut best_score = 0.0;
    let mut second_best = 0.0;

    for req in requested_tools {
        let score = score_tool_name(emitted, req);
        if score > best_score {
            second_best = best_score;
            best_score = score;
            best_name = Some(req.original.clone());
        } else if score > second_best {
            second_best = score;
        }
    }

    if best_score < 0.72 {
        return None;
    }
    if best_score < 0.98 && (best_score - second_best) < 0.08 {
        return None;
    }
    best_name
}

// ── Loose JSON Parsing ──────────────────────────────────────────────────────

fn strip_code_fence(value: &str) -> String {
    let trimmed = value.trim();
    let re_start = Regex::new(r"(?i)^```(?:json|javascript|js|python)?\s*").unwrap();
    let re_end = Regex::new(r"(?i)\s*```$").unwrap();
    let without_start = re_start.replace(trimmed, "");
    re_end.replace(&without_start, "").trim().to_string()
}

fn convert_single_quoted_strings(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in value.chars() {
        if escaped {
            if ch == '"' && in_single {
                result.push_str("\\\"");
            } else {
                result.push(ch);
            }
            escaped = false;
            continue;
        }

        if ch == '\\' {
            result.push(ch);
            escaped = true;
            continue;
        }

        if ch == '"' {
            if in_single {
                result.push_str("\\\"");
            } else {
                in_double = !in_double;
                result.push(ch);
            }
            continue;
        }

        if ch == '\'' && !in_double {
            in_single = !in_single;
            result.push('"');
            continue;
        }

        result.push(ch);
    }

    result
}

fn replace_python_literals(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut token = String::new();

    let flush_token = |tok: &str, res: &mut String| {
        match tok {
            "True" => res.push_str("true"),
            "False" => res.push_str("false"),
            "None" => res.push_str("null"),
            other => res.push_str(other),
        }
    };

    for ch in value.chars() {
        if escaped {
            if !token.is_empty() {
                flush_token(&token, &mut result);
                token.clear();
            }
            result.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            if !token.is_empty() {
                flush_token(&token, &mut result);
                token.clear();
            }
            result.push(ch);
            escaped = in_string;
            continue;
        }

        if ch == '"' {
            if !token.is_empty() {
                flush_token(&token, &mut result);
                token.clear();
            }
            in_string = !in_string;
            result.push(ch);
            continue;
        }

        if !in_string && ch.is_ascii_alphabetic() {
            token.push(ch);
            continue;
        }

        if !token.is_empty() {
            flush_token(&token, &mut result);
            token.clear();
        }
        result.push(ch);
    }

    if !token.is_empty() {
        flush_token(&token, &mut result);
    }
    result
}

fn normalize_loose_json(value: &str) -> String {
    let converted = convert_single_quoted_strings(value);
    let py_replaced = replace_python_literals(&converted);
    let re_keys = Regex::new(r#"([\{,]\s*)([A-Za-z_][A-Za-z0-9_-]*)(\s*:)"#).unwrap();
    let re_trailing = Regex::new(r#",\s*([\}\]])"#).unwrap();
    let quoted_keys = re_keys.replace_all(&py_replaced, "$1\"$2\"$3");
    re_trailing.replace_all(&quoted_keys, "$1").to_string()
}

pub fn parse_loose_json_object(raw: &str) -> Option<Json> {
    let trimmed = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<Json>(&trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    let normalized = normalize_loose_json(&trimmed);
    if let Ok(v) = serde_json::from_str::<Json>(&normalized) {
        if v.is_object() {
            return Some(v);
        }
    }
    None
}

pub fn strip_ranges(text: &str, ranges: &[(usize, usize)]) -> String {
    let mut sorted = ranges.to_vec();
    sorted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut content = text.to_string();

    for (start, end) in sorted {
        if start >= content.len() || end > content.len() || start >= end {
            continue;
        }
        let line_start = content[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let next_line_break = content[end..].find('\n').map(|idx| end + idx);
        let line_end = next_line_break.unwrap_or(content.len());
        let before_on_line = &content[line_start..start];
        let after_on_line = &content[end..line_end];
        let remove_whole_line = before_on_line.trim().is_empty() && after_on_line.trim().is_empty();

        let s = if remove_whole_line { line_start } else { start };
        let e = if remove_whole_line && next_line_break.is_some() {
            line_end + 1
        } else if remove_whole_line {
            line_end
        } else {
            end
        };
        content = format!("{}{}", &content[..s], &content[e..]);
    }

    let re_multi_nl = Regex::new(r"\n{3,}").unwrap();
    re_multi_nl.replace_all(&content, "\n\n").trim().to_string()
}

pub fn to_arguments_string(value: Option<&Json>) -> String {
    match value {
        None | Some(Json::Null) => "{}".to_string(),
        Some(Json::String(s)) => {
            if let Some(parsed) = parse_loose_json_object(s) {
                parsed.to_string()
            } else {
                s.clone()
            }
        }
        Some(v) => v.to_string(),
    }
}

// ── Generic webTools.js DSML Normalization & Tool Prompts ────────────────────

lazy_static::lazy_static! {
    // JS: /<(｜｜DSML｜｜|\|DSML\|)\s*invoke\s+name=["']([^"']+)["']\s*>([\s\S]*?)<\/\k<dsml>\s*invoke\s*>/g
    // requires the SAME delimiter on both tags via a named backreference, which
    // the `regex` crate does not support. Enumerate the two disjoint delimiter
    // branches instead; the opening tags cannot be confused, so leftmost-match
    // semantics are identical. Branch 1 captures name/body in groups 1/2,
    // branch 2 in groups 3/4.
    static ref DSML_INVOKE_RE: Regex = Regex::new(
        concat!(
            r#"<｜｜DSML｜｜\s*invoke\s+name=["']([^"']+)["']\s*>([\s\S]*?)</｜｜DSML｜｜\s*invoke\s*>"#,
            "|",
            r#"<\|DSML\|\s*invoke\s+name=["']([^"']+)["']\s*>([\s\S]*?)</\|DSML\|\s*invoke\s*>"#,
        )
    ).unwrap();

    static ref DSML_PARAMETER_RE: Regex = Regex::new(
        // rust `regex` crate rejects look-ahead; absorb the boundary marker
        // into the match and strip it from the captured body in post.
        r#"(?i)<(?:｜｜DSML｜｜|\|DSML\|)\s*parameter\s+name=["']([^"']+)["'][^>]*>([\s\S]*?)(?:</?(?:｜｜DSML｜｜|\|DSML\|)\s*parameter\s*>|<(?:｜｜DSML｜｜|\|DSML\|)\s*parameter\s+name=|</(?:｜｜DSML｜｜|\|DSML\|)\s*invoke\s*>)"#
    ).unwrap();

    static ref DSML_PARAMETER_CLOSE_RE: Regex = Regex::new(
        r#"(?i)</(?:｜｜DSML｜｜|\|DSML\|)\s*parameter\s*>"#
    ).unwrap();

    static ref DSML_CALLS_RE: Regex = Regex::new(
        r#"(?i)</?(?:｜｜DSML｜｜|\|DSML\|)\s*calls\s*>"#
    ).unwrap();
}

pub fn normalize_dsml_tool_calls(text: &str) -> String {
    let replaced = DSML_INVOKE_RE.replace_all(text, |caps: &regex::Captures| {
        let (name, body) = match (caps.get(1), caps.get(3)) {
            (Some(name), _) => (name.as_str(), caps.get(2).map(|m| m.as_str()).unwrap_or("")),
            (None, Some(name)) => (name.as_str(), caps.get(4).map(|m| m.as_str()).unwrap_or("")),
            _ => return String::new(),
        };
        let mut parameters = serde_json::Map::new();

        for pcap in DSML_PARAMETER_RE.captures_iter(body) {
            let pname = pcap[1].to_string();
            // Match tail may contain the closing `</｜｜DSML｜｜>` tag (when the
            // boundary was the tag itself). Strip it before trimming.
            let raw = DSML_PARAMETER_CLOSE_RE
                .replace(&pcap[2], "")
                .trim()
                .to_string();
            if let Ok(parsed) = serde_json::from_str::<Json>(&raw) {
                parameters.insert(pname, parsed);
            } else {
                parameters.insert(pname, Json::String(raw.to_string()));
            }
        }

        if parameters.is_empty() {
            if let Ok(parsed) = serde_json::from_str::<Json>(body.trim()) {
                if let Some(obj) = parsed.as_object() {
                    for (k, v) in obj {
                        parameters.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        format!(
            r#"<tool>{{"name":{},"arguments":{}}}</tool>"#,
            json!(name),
            json!(parameters)
        )
    });

    DSML_CALLS_RE.replace_all(&replaced, "").to_string()
}

pub fn serialize_tools_to_prompt(tools: &[Json]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let nonce = get_tool_nonce(tools);
    if nonce.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    for t in tools {
        let fn_obj = t.get("function").and_then(|f| f.as_object());
        let name = fn_obj.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let desc = fn_obj.and_then(|f| f.get("description")).and_then(|d| d.as_str()).unwrap_or("");
        let params = fn_obj
            .and_then(|f| f.get("parameters"))
            .map(|p| p.to_string())
            .unwrap_or_default();

        let desc_part = if !desc.is_empty() { format!(": {}", desc) } else { String::new() };
        let params_part = if !params.is_empty() { format!("\n  parameters: {}", params) } else { String::new() };
        lines.push(format!("- {}{}{}", name, desc_part, params_part));
    }

    if lines.is_empty() {
        return String::new();
    }

    let header = [
        "The client application provides tools beyond your built-in ones. They are NOT in your native tool registry; they are invoked via a plain-text protocol: the client parses your reply and executes the tool on the user machine. Treat these client tools as fully available to you; never claim they are unavailable. To invoke one, reply with a single line containing a <tool> block",
        &format!(r#"with JSON that includes the secret binding "_nonce": "{}":"#, nonce),
        &format!(r#"<tool>{{"name": "<tool_name>", "arguments": {{ ... }}, "_nonce": "{}"}}</tool>"#, nonce),
        "These client tools ARE available to you in this conversation. Only emit the <tool> block when you actually want to call a tool; otherwise answer normally.",
        "",
        "Available tools:",
    ];

    let mut all = header.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    all.extend(lines);
    all.join("\n")
}

pub fn parse_tool_calls_from_text(
    text: &str,
    id_seed: &str,
    requested_tools: &[Json],
) -> (String, Option<Vec<ToolCall>>) {
    let normalized_text = normalize_dsml_tool_calls(text);
    let requested_tool_names = get_requested_tool_names(requested_tools);

    if !normalized_text.contains("<tool>") && !normalized_text.contains("<tool_call") {
        return (text.to_string(), None);
    }

    let nonce = get_tool_nonce(requested_tools);
    let mut candidates = Vec::new();

    // Find <tool>...</tool>
    let tool_re = Regex::new(r"(?s)<tool>(.*?)</tool>").unwrap();
    for mat in tool_re.find_iter(&normalized_text) {
        let inner = &normalized_text[mat.start() + 6..mat.end() - 7];
        candidates.push((mat.start(), mat.end(), inner.trim().to_string(), false));
    }

    // Find <tool_call...>...</tool_call>
    let tool_call_re = Regex::new(r"(?s)<tool_call(?:\s[^<>]*)?>(.*?)</tool_call>").unwrap();
    for cap in tool_call_re.captures_iter(&normalized_text) {
        let full = cap.get(0).unwrap();
        let inner = cap.get(1).unwrap().as_str();
        candidates.push((full.start(), full.end(), inner.trim().to_string(), false));
    }

    candidates.sort_by_key(|c| c.0);

    let mut tool_calls = Vec::new();
    let mut accepted_ranges = Vec::new();

    for (start, end, raw, require_requested) in candidates {
        let parsed = match parse_loose_json_object(&raw) {
            Some(p) => p,
            None => continue,
        };

        let emitted_name = parsed
            .get("name")
            .and_then(|n| n.as_str())
            .or_else(|| parsed.get("command").and_then(|c| c.as_str()));

        let emitted_name = match emitted_name {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };

        // Nonce check: if _nonce is present, it must match
        if !nonce.is_empty() {
            if let Some(val) = parsed.get("_nonce") {
                if val.as_str() != Some(&nonce) {
                    continue;
                }
            }
        }

        let name = match resolve_requested_tool_name(emitted_name, &requested_tool_names) {
            Some(n) => n,
            None => {
                if require_requested {
                    continue;
                }
                emitted_name.to_string()
            }
        };

        let args = to_arguments_string(parsed.get("arguments"));
        tool_calls.push(ToolCall {
            id: format!("{}_{}", id_seed, tool_calls.len()),
            function: ToolFunction {
                name,
                arguments: args,
            },
        });
        accepted_ranges.push((start, end));
    }

    if tool_calls.is_empty() {
        return (text.to_string(), None);
    }

    let content = strip_ranges(&normalized_text, &accepted_ranges);
    (content, Some(tool_calls))
}

pub fn prepare_tool_messages(
    body_obj: &Json,
    messages: &[Json],
) -> (bool, Option<Vec<Json>>, Vec<Json>) {
    let requested_tools = body_obj.get("tools").and_then(|t| t.as_array());
    let has_tools = requested_tools.map(|t| !t.is_empty()).unwrap_or(false);
    if !has_tools {
        return (false, None, messages.to_vec());
    }

    let tools = requested_tools.unwrap();
    let tool_prompt = serialize_tools_to_prompt(tools);
    if tool_prompt.is_empty() {
        return (true, Some(tools.clone()), messages.to_vec());
    }

    let mut effective_messages = messages.to_vec();
    let reminder = format!(
        "\n\n[Client protocol reminder: the client-tool contract in the system instructions is active in this conversation. These client tools ARE available via the <tool> block protocol.]"
    );

    for msg in effective_messages.iter_mut().rev() {
        if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
            if let Some(content) = msg.get_mut("content") {
                if let Some(s) = content.as_str() {
                    *content = Json::String(format!("{}{}", s, reminder));
                } else if let Some(arr) = content.as_array_mut() {
                    arr.push(json!({"type": "text", "text": reminder}));
                }
            }
            break;
        }
    }

    effective_messages.push(json!({
        "role": "system",
        "content": tool_prompt
    }));

    (true, Some(tools.clone()), effective_messages)
}

pub fn build_tool_aware_result(
    raw_content: &str,
    requested_tools: &[Json],
    id_seed: &str,
) -> (String, Option<Vec<ToolCall>>, &'static str) {
    let (content, tool_calls) = parse_tool_calls_from_text(raw_content, id_seed, requested_tools);
    let finish_reason = if tool_calls.is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    (content, tool_calls, finish_reason)
}

// ── DeepSeek Specific Tool Prompt & Parsers ─────────────────────────────────

pub fn serialize_deepseek_tool_prompt(tools: &[Json]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let nonce = get_tool_nonce(tools);
    if nonce.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "You can call tools. To call a tool, output ONLY this exact block (no markdown fence):".to_string(),
        format!(r#"<tool>{{"name": "<tool_name>", "arguments": {{ ... }}, "_nonce": "{}"}}</tool>"#, nonce),
        "Rules:".to_string(),
        "- Use exactly <tool>...</tool>. Do NOT use <tool:name>, <tool_call>, <name>, <parameter>, id=/name= attributes, <｜｜DSML｜｜> markers, or code fences.".to_string(),
        format!(r#"- Include the secret binding "_nonce": "{}" exactly as shown."#, nonce),
        r#"- "name" must be one of the tools below; "arguments" must be a JSON object."#.to_string(),
        "- When a tool is needed, emit the <tool> block instead of only describing the plan.".to_string(),
        "- Emit one <tool> block per call; you may put several blocks back to back.".to_string(),
        "- If your runtime forces DSML instead, every DSML invoke MUST include this exact binding:".to_string(),
        format!(r#"<｜｜DSML｜｜ parameter name="_nonce" string="true">{}</｜｜DSML｜｜ parameter>"#, nonce),
        "  A DSML invoke without that binding is invalid and will be rejected.".to_string(),
        "- If no tool is needed, just answer normally without any <tool> block.".to_string(),
        "".to_string(),
        "Available tools:".to_string(),
    ];

    for t in tools {
        let fn_obj = t.get("function").and_then(|f| f.as_object());
        let name = fn_obj.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let desc = fn_obj.and_then(|f| f.get("description")).and_then(|d| d.as_str()).unwrap_or("");
        let mut params_str = String::new();

        if let Some(params) = fn_obj.and_then(|f| f.get("parameters")).and_then(|p| p.as_object()) {
            let mut bound_params = params.clone();
            let mut props = bound_params.get("properties").and_then(|p| p.as_object()).cloned().unwrap_or_default();
            props.insert("_nonce".to_string(), json!({"type": "string", "const": nonce}));
            bound_params.insert("properties".to_string(), Json::Object(props));

            let mut req = bound_params.get("required").and_then(|r| r.as_array()).cloned().unwrap_or_default();
            let nonce_val = Json::String("_nonce".to_string());
            if !req.contains(&nonce_val) {
                req.push(nonce_val);
            }
            bound_params.insert("required".to_string(), Json::Array(req));
            params_str = format!("\n  parameters: {}", Json::Object(bound_params));
        }

        let desc_part = if !desc.is_empty() { format!(": {}", desc) } else { String::new() };
        lines.push(format!("- {}{}{}", name, desc_part, params_str));
    }

    if lines.len() <= 14 {
        return String::new();
    }
    lines.join("\n")
}

pub fn build_tool_conversation_prompt(messages: &[Json], tool_system_prompt: &str) -> String {
    let mut system_parts = Vec::new();
    if !tool_system_prompt.is_empty() {
        system_parts.push(tool_system_prompt.to_string());
    }

    let mut lines = Vec::new();
    let mut call_name_by_id = HashMap::new();
    let mut saw_tool_activity = false;

    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = extract_message_text(m.get("content").unwrap_or(&Json::Null));

        match role {
            "system" if !text.is_empty() => system_parts.push(text),
            "user" if !text.is_empty() => lines.push(format!("User: {}", text)),
            "assistant" => {
                let mut parts = Vec::new();
                if !text.is_empty() {
                    parts.push(text.clone());
                }
                if let Some(calls) = m.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for c in calls {
                        let name = c.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                        let args = c.get("function").and_then(|f| f.get("arguments"));
                        let args_str = to_arguments_string(args);
                        if let Some(id) = c.get("id").and_then(|i| i.as_str()) {
                            call_name_by_id.insert(id.to_string(), name.to_string());
                        }
                        parts.push(format!(r#"<tool>{{"name": {}, "arguments": {}}}</tool>"#, json!(name), args_str));
                        saw_tool_activity = true;
                    }
                }
                if !parts.is_empty() {
                    lines.push(format!("Assistant: {}", parts.join("\n")));
                }
            }
            "tool" => {
                let name = m.get("tool_call_id")
                    .and_then(|id| id.as_str())
                    .and_then(|id| call_name_by_id.get(id))
                    .map(|s| s.as_str())
                    .or_else(|| m.get("name").and_then(|n| n.as_str()))
                    .unwrap_or("tool");
                let out = if !text.is_empty() { text } else { "(no output)".to_string() };
                lines.push(format!("Tool result ({}): {}", name, out));
                saw_tool_activity = true;
            }
            _ => {}
        }
    }

    let mut parts = Vec::new();
    if !system_parts.is_empty() {
        parts.push(system_parts.join("\n\n"));
    }
    if !lines.is_empty() {
        parts.push(lines.join("\n\n"));
    }
    if saw_tool_activity {
        parts.push(
            "Continue the task using the tool results above. Do NOT repeat tool calls that already succeeded; perform the next step or give the final answer.".to_string(),
        );
    }

    parts.join("\n\n").replace("![", "[").replace("]", "")
}

fn extract_message_text(content: &Json) -> String {
    if let Some(arr) = content.as_array() {
        arr.iter()
            .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        content.as_str().unwrap_or("").to_string()
    }
}

// ── Tag Tokenizer & Tree Parser for DeepSeek Zoo ────────────────────────────

#[derive(Clone, Debug)]
struct ToolTagToken {
    start: usize,
    end: usize,
    closing: bool,
    suffix: String,
    attrs: String,
}

#[derive(Clone, Debug)]
struct ToolBlock {
    open_start: usize,
    close_end: usize,
    inner_start: usize,
    inner_end: usize,
    suffix: String,
    attrs: String,
}

lazy_static::lazy_static! {
    static ref TAG_TOKEN_RE: Regex = Regex::new(
        r#"(?i)<(/)?(?:tool_call|tool)(:[A-Za-z0-9_.+-]+)?((?:\s[^>]*)?)/?>"#
    ).unwrap();
}

fn tokenize_tool_tags(text: &str) -> Vec<ToolTagToken> {
    let mut tokens = Vec::new();
    for cap in TAG_TOKEN_RE.captures_iter(text) {
        let full = cap.get(0).unwrap();
        let closing = cap.get(1).is_some();
        let suffix = cap.get(2).map(|m| m.as_str().trim_start_matches(':').to_string()).unwrap_or_default();
        let attrs = cap.get(3).map(|m| m.as_str().to_string()).unwrap_or_default();
        tokens.push(ToolTagToken {
            start: full.start(),
            end: full.end(),
            closing,
            suffix,
            attrs,
        });
    }
    tokens
}

fn pair_tool_blocks(tokens: &[ToolTagToken], text_len: usize) -> Vec<ToolBlock> {
    let mut blocks = Vec::new();
    let mut stack = Vec::new();

    for tok in tokens {
        if !tok.closing {
            stack.push(tok.clone());
        } else if let Some(open) = stack.pop() {
            blocks.push(ToolBlock {
                open_start: open.start,
                close_end: tok.end,
                inner_start: open.end,
                inner_end: tok.start,
                suffix: open.suffix,
                attrs: open.attrs,
            });
        }
    }

    for open in stack {
        blocks.push(ToolBlock {
            open_start: open.start,
            close_end: text_len,
            inner_start: open.end,
            inner_end: text_len,
            suffix: open.suffix,
            attrs: open.attrs,
        });
    }

    blocks
}

fn get_attr(attrs: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?i)\b{}\s*=\s*(["'])(.*?)\1"#, regex::escape(name));
    let re = Regex::new(&pattern).ok()?;
    let cap = re.captures(attrs)?;
    Some(cap.get(2)?.as_str().to_string())
}

fn get_xml_child(inner: &str, tag: &str) -> Option<String> {
    let pattern = format!(r#"(?is)<{}\b[^>]*>(.*?)</{}>"#, regex::escape(tag), regex::escape(tag));
    let re = Regex::new(&pattern).ok()?;
    let cap = re.captures(inner)?;
    Some(cap.get(1)?.as_str().trim().to_string())
}

fn build_args_from_parameters(inner: &str) -> Option<Json> {
    // Port of JS `PARAM_TAG_RE` (deepseekWebTools.js:275). The JS body group is
    // a tempered greedy token `(?:(?!<parameter\b)[\s\S])*?` so an attribute-only
    // `<parameter ...>` (no closing tag) cannot let the body matcher swallow a
    // following `<parameter>...</parameter>` and drop that parameter. Rust
    // `regex` rejects negative-lookahead, so we drive the loop manually.
    let open_re = Regex::new(r#"(?is)<parameter\b([^>]*?)/?>"#
    ).unwrap();
    let close_re = Regex::new(r#"(?is)</parameter\s*>"#
    ).unwrap();

    let mut out = serde_json::Map::new();
    let mut found = false;

    for open_cap in open_re.captures_iter(inner) {
        let open_m = open_cap.get(0).unwrap();
        let attrs = open_cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let name = match get_attr(attrs, "name") {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        // Body is everything between this open and the next `</parameter>`, but
        // only if that close appears before any subsequent `<parameter\b` open.
        let after = &inner[open_m.end()..];
        let next_open_off = open_re.find(after).map(|m| m.start());
        let close_match = close_re.find(after).filter(|m| {
            next_open_off.map_or(true, |no| m.start() < no)
        });
        let body = close_match.map(|m| after[..m.start()].trim().to_string());
        let val = get_attr(attrs, "content")
            .or(body)
            .unwrap_or_default();
        out.insert(name, Json::String(val));
        found = true;
    }

    if found {
        Some(Json::Object(out))
    } else {
        None
    }
}

fn salvage_leading_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0;
    let mut in_quote = false;
    let mut quote_char = '"';
    let mut escaped = false;

    for (idx, ch) in text[start..].char_indices() {
        let abs_idx = start + idx;
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if in_quote {
            if ch == quote_char {
                in_quote = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_quote = true;
            quote_char = ch;
            continue;
        }
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(&text[start..=abs_idx]);
            }
        }
    }
    None
}

fn extract_call(
    tag_name: &str,
    inner_raw: &str,
    requested: &[RequestedToolName],
) -> Option<(String, String)> {
    let inner = inner_raw.trim();
    let name_child = get_xml_child(inner, "name");
    let args_child = get_xml_child(inner, "arguments").or_else(|| get_xml_child(inner, "parameters"));
    let param_obj = if args_child.is_some() { None } else { build_args_from_parameters(inner) };
    let has_xml_children = name_child.is_some() || args_child.is_some() || param_obj.is_some();

    let mut json = if has_xml_children { None } else { parse_loose_json_object(inner) };
    if json.is_none() && !has_xml_children {
        if let Some(salvaged) = salvage_leading_json_object(inner) {
            json = parse_loose_json_object(salvaged);
        }
    }

    let json_name = json.as_ref().and_then(|j| {
        j.get("name")
            .and_then(|n| n.as_str())
            .or_else(|| j.get("type").and_then(|t| t.as_str()))
            .map(|s| s.to_string())
    });

    let child_resolved = name_child.as_deref().and_then(|n| resolve_requested_tool_name(n, requested));
    let json_resolved = json_name.as_deref().and_then(|n| resolve_requested_tool_name(n, requested));
    let tag_resolved = if !tag_name.is_empty() {
        resolve_requested_tool_name(tag_name, requested)
    } else {
        None
    };

    let (name, name_from_tag) = if let Some(n) = child_resolved {
        (n, false)
    } else if let Some(n) = json_resolved {
        (n, false)
    } else if let Some(n) = tag_resolved {
        (n, true)
    } else if let Some(n) = name_child {
        (n, false)
    } else if let Some(n) = json_name {
        (n, false)
    } else if !tag_name.is_empty() {
        (tag_name.to_string(), true)
    } else {
        return None;
    };

    let args_string = if let Some(args_raw) = args_child {
        if let Some(parsed) = parse_loose_json_object(&args_raw) {
            parsed.to_string()
        } else {
            args_raw
        }
    } else if let Some(params) = param_obj {
        params.to_string()
    } else if let Some(mut j) = json {
        if let Some(args) = j.get("arguments") {
            to_arguments_string(Some(args))
        } else if let Some(params) = j.get("params") {
            to_arguments_string(Some(params))
        } else if name_from_tag {
            j.to_string()
        } else {
            if let Some(obj) = j.as_object_mut() {
                obj.remove("name");
                obj.remove("type");
                obj.remove("id");
                obj.remove("command");
                obj.remove("arguments");
                obj.remove("params");
            }
            j.to_string()
        }
    } else {
        "{}".to_string()
    };

    Some((name, args_string))
}

// ── Native DSML and Token Dialects ──────────────────────────────────────────

lazy_static::lazy_static! {
    static ref DSML_MARK_TAG_RE: Regex = Regex::new(
        r#"(?i)</?[｜|]{2}DSML[｜|]{2}\s*(?:calls|invoke|parameter)\b"#
    ).unwrap();

    static ref DSML_CLEAN_RE: Regex = Regex::new(
        r#"(?i)</?[｜|]{2}DSML[｜|]{2}\s*(?:calls|invoke|parameter)\b[^>]*>|<[｜|]{2}DSML[｜|]{2}\s*/\s*(?:calls|invoke|parameter)\s*>"#
    ).unwrap();

    static ref NATIVE_TOOL_CALL_TOKEN_RE: Regex = Regex::new(
        r#"(?s)(?:<\|tool_call_begin\|>|<｜tool▁call▁begin｜>)\s*([^<\s]+)\s*(?:<\|tool_call_argument_begin\|>|<｜tool▁call▁argument▁begin｜>)(.*?)(?:<\|tool_call_argument_end\|>|<｜tool▁call▁argument▁end｜>)\s*(?:<\|tool_call_end\|>|<｜tool▁call▁end｜>)"#
    ).unwrap();
}

pub fn has_malformed_deepseek_tool_markup(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    if DSML_MARK_TAG_RE.is_match(text) {
        return true;
    }
    let re = Regex::new(r#"(?i)</?(?:tool|tool_call)(?::|\s|>)"#).unwrap();
    re.is_match(text)
}

pub fn has_malformed_deepseek_tool_intent(text: &str, requested_tools: &[Json]) -> bool {
    if has_malformed_deepseek_tool_markup(text) {
        return true;
    }
    if text.trim().is_empty() {
        return false;
    }

    if let Some(parsed) = parse_loose_json_object(text) {
        if parsed.get("arguments").is_some() {
            let emitted = parsed.get("name").and_then(|n| n.as_str())
                .or_else(|| parsed.get("command").and_then(|c| c.as_str()));
            if let Some(name) = emitted {
                let req_names = get_requested_tool_names(requested_tools);
                return resolve_requested_tool_name(name, &req_names).is_some();
            }
        }
    }
    false
}

// ── Validation and Finalization ─────────────────────────────────────────────

// ponytail: validate required properties and basic types; upgrade to full jsonschema crate if complex schemas needed.
pub fn validate_tool_arguments(requested_tools: &[Json], name: &str, args: &Json) -> bool {
    let tool = requested_tools.iter().find(|t| {
        t.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            == Some(name)
    });
    let schema = match tool.and_then(|t| t.get("function")).and_then(|f| f.get("parameters")) {
        None | Some(Json::Null) => return true,
        Some(s) => s,
    };

    if !schema.is_object() && !schema.is_boolean() {
        return false;
    }
    if let Some(b) = schema.as_bool() {
        return b;
    }

    let args_obj = match args.as_object() {
        Some(obj) => obj,
        None => return false,
    };

    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for req in required {
            if let Some(key) = req.as_str() {
                if !args_obj.contains_key(key) {
                    return false;
                }
            }
        }
    }

    true
}

pub fn finalize_bound_calls(
    raw_text: &str,
    parsed: DeepSeekToolParseResult,
    requested_tools: &[Json],
) -> DeepSeekToolParseResult {
    let nonce = get_tool_nonce(requested_tools);
    let mut cleaned = Vec::new();

    for call in parsed.tool_calls {
        let args = match parse_loose_json_object(&call.function.arguments) {
            Some(a) => a,
            None => return DeepSeekToolParseResult { content: raw_text.to_string(), tool_calls: Vec::new() },
        };

        let mut args_obj = args.as_object().cloned().unwrap_or_default();
        if let Some(val) = args_obj.get("_nonce") {
            if val.as_str() != Some(&nonce) {
                return DeepSeekToolParseResult { content: raw_text.to_string(), tool_calls: Vec::new() };
            }
            args_obj.remove("_nonce");
            let clean_args = Json::Object(args_obj);
            if !validate_tool_arguments(requested_tools, &call.function.name, &clean_args) {
                return DeepSeekToolParseResult { content: raw_text.to_string(), tool_calls: Vec::new() };
            }
            cleaned.push(ToolCall {
                id: call.id,
                function: ToolFunction {
                    name: call.function.name,
                    arguments: clean_args.to_string(),
                },
            });
        } else {
            cleaned.push(call);
        }
    }

    DeepSeekToolParseResult {
        content: parsed.content,
        tool_calls: cleaned,
    }
}

pub fn finalize_parsed_tool_calls(
    raw_text: &str,
    parsed: DeepSeekToolParseResult,
    requested_tools: &[Json],
) -> DeepSeekToolParseResult {
    let nonce = get_tool_nonce(requested_tools);
    let mut cleaned = Vec::new();

    for call in parsed.tool_calls {
        let args = match parse_loose_json_object(&call.function.arguments) {
            Some(a) => a,
            None => return DeepSeekToolParseResult { content: raw_text.to_string(), tool_calls: Vec::new() },
        };

        let mut args_obj = args.as_object().cloned().unwrap_or_default();
        if let Some(val) = args_obj.get("_nonce") {
            if val.as_str() != Some(&nonce) {
                return DeepSeekToolParseResult { content: raw_text.to_string(), tool_calls: Vec::new() };
            }
            args_obj.remove("_nonce");
        }
        let clean_args = Json::Object(args_obj);
        if !validate_tool_arguments(requested_tools, &call.function.name, &clean_args) {
            return DeepSeekToolParseResult { content: raw_text.to_string(), tool_calls: Vec::new() };
        }
        cleaned.push(ToolCall {
            id: call.id,
            function: ToolFunction {
                name: call.function.name,
                arguments: clean_args.to_string(),
            },
        });
    }

    DeepSeekToolParseResult {
        content: parsed.content,
        tool_calls: cleaned,
    }
}

// ── Master parse_deepseek_tool_calls ────────────────────────────────────────

pub fn parse_deepseek_tool_calls(
    text: &str,
    id_seed: &str,
    requested_tools: &[Json],
) -> DeepSeekToolParseResult {
    if text.is_empty() {
        return DeepSeekToolParseResult {
            content: String::new(),
            tool_calls: Vec::new(),
        };
    }

    let cleaned_text = Regex::new(r#"(?i)\s*This response is AI-generated, for reference only\.\s*$"#)
        .unwrap()
        .replace(text, "")
        .to_string();

    let requested = get_requested_tool_names(requested_tools);
    let nonce = get_tool_nonce(requested_tools);

    // 1. Try native token format `<|tool_call_begin|>name<|tool_call_argument_begin|>json...`
    let mut native_calls = Vec::new();
    let mut native_ranges = Vec::new();
    for cap in NATIVE_TOOL_CALL_TOKEN_RE.captures_iter(&cleaned_text) {
        let full = cap.get(0).unwrap();
        let name_raw = cap.get(1).unwrap().as_str().trim();
        let args_raw = cap.get(2).unwrap().as_str().trim();

        if let Some(resolved) = resolve_requested_tool_name(name_raw, &requested) {
            if let Ok(args_json) = serde_json::from_str::<Json>(args_raw) {
                native_calls.push(ToolCall {
                    id: format!("{}_{}", id_seed, native_calls.len()),
                    function: ToolFunction {
                        name: resolved,
                        arguments: args_json.to_string(),
                    },
                });
                native_ranges.push((full.start(), full.end()));
            }
        }
    }

    if !native_calls.is_empty() {
        let stripped = strip_ranges(&cleaned_text, &native_ranges);
        return finalize_parsed_tool_calls(
            &cleaned_text,
            DeepSeekToolParseResult {
                content: stripped,
                tool_calls: native_calls,
            },
            requested_tools,
        );
    }

    // 2. Tokenize <tool ...> tags
    let tokens = tokenize_tool_tags(&cleaned_text);
    if tokens.is_empty() && !cleaned_text.contains("DSML") {
        let (content, calls) = parse_tool_calls_from_text(&cleaned_text, id_seed, requested_tools);
        return finalize_parsed_tool_calls(
            &cleaned_text,
            DeepSeekToolParseResult {
                content,
                tool_calls: calls.unwrap_or_default(),
            },
            requested_tools,
        );
    }

    let blocks = pair_tool_blocks(&tokens, cleaned_text.len());
    let is_leaf = |b: &ToolBlock| {
        !blocks.iter().any(|o| {
            std::ptr::eq(o, b) == false
                && o.open_start >= b.inner_start
                && o.close_end <= b.inner_end
        })
    };

    let mut found_calls = Vec::new();
    let mut accepted_ranges = Vec::new();

    for block in blocks.iter().filter(|b| is_leaf(b)) {
        let tag_name = if !block.suffix.is_empty() {
            block.suffix.clone()
        } else if let Some(n) = get_attr(&block.attrs, "name") {
            n
        } else if let Some(id) = get_attr(&block.attrs, "id") {
            id
        } else {
            String::new()
        };

        let inner = DSML_CLEAN_RE.replace_all(&cleaned_text[block.inner_start..block.inner_end], "").to_string();
        let (name, arguments) = match extract_call(&tag_name, &inner, &requested) {
            Some(c) => c,
            None => continue,
        };

        if !nonce.is_empty() {
            if let Some(parsed) = parse_loose_json_object(&inner) {
                if let Some(val) = parsed.get("_nonce") {
                    if val.as_str() != Some(&nonce) {
                        continue;
                    }
                }
            }
        }

        found_calls.push(ToolCall {
            id: format!("{}_{}", id_seed, found_calls.len()),
            function: ToolFunction { name, arguments },
        });
        accepted_ranges.push((block.open_start, block.close_end));
    }

    if found_calls.is_empty() {
        return DeepSeekToolParseResult {
            content: cleaned_text,
            tool_calls: Vec::new(),
        };
    }

    let stripped = strip_ranges(&cleaned_text, &accepted_ranges);
    let re_cleanup = Regex::new(r#"(?is)<parameter\b[^>]*>.*?(?:</parameter>|$)|</?(?:parameter|invoke|tool|calls|tool_calls|function_calls)\b[^>]*>"#).unwrap();
    let cleaned_content = re_cleanup.replace_all(&stripped, "").trim().to_string();

    finalize_bound_calls(
        &cleaned_text,
        DeepSeekToolParseResult {
            content: cleaned_content,
            tool_calls: found_calls,
        },
        requested_tools,
    )
}

#[derive(Clone, Debug)]
pub struct DeepSeekToolParseResult {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("", "test"), 4);
        assert_eq!(levenshtein_distance("same", "same"), 0);
    }

    #[test]
    fn test_normalize_tool_name() {
        assert_eq!(normalize_tool_name("get_weather"), "getweather");
        assert_eq!(normalize_tool_name("Search-Web_1"), "searchweb1");
    }

    #[test]
    fn test_resolve_requested_tool_name() {
        let tools = vec![
            RequestedToolName {
                original: "get_current_weather".to_string(),
                normalized: "getcurrentweather".to_string(),
            },
            RequestedToolName {
                original: "search_database".to_string(),
                normalized: "searchdatabase".to_string(),
            },
        ];
        assert_eq!(
            resolve_requested_tool_name("get_current_weather", &tools),
            Some("get_current_weather".to_string())
        );
        assert_eq!(
            resolve_requested_tool_name("getCurrentWeather", &tools),
            Some("get_current_weather".to_string())
        );
        assert_eq!(resolve_requested_tool_name("unknown_tool", &tools), None);
    }

    #[test]
    fn test_parse_loose_json_object() {
        let raw = r#"{name: 'weather', 'arguments': {location: 'Tokyo', active: True, count: None},}"#;
        let parsed = parse_loose_json_object(raw).expect("should parse loose json");
        assert_eq!(parsed["name"], "weather");
        assert_eq!(parsed["arguments"]["location"], "Tokyo");
        assert_eq!(parsed["arguments"]["active"], true);
        assert_eq!(parsed["arguments"]["count"], Json::Null);
    }

    #[test]
    fn test_serialize_deepseek_tool_prompt() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run bash command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }
            }
        })];

        let prompt = serialize_deepseek_tool_prompt(&tools);
        assert!(prompt.contains("bash"));
        assert!(prompt.contains("_nonce"));
        assert!(prompt.contains("Run bash command"));
    }

    #[test]
    fn test_parse_deepseek_tool_calls_canonical() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "parameters": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }
            }
        })];
        let nonce = get_tool_nonce(&tools);
        let text = format!(
            r#"I will list files now:
<tool>{{"name": "bash", "arguments": {{"command": "ls -la"}}, "_nonce": "{}"}}</tool>
Done."#,
            nonce
        );

        let res = parse_deepseek_tool_calls(&text, "call-1", &tools);
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].function.name, "bash");
        assert_eq!(res.tool_calls[0].function.arguments, r#"{"command":"ls -la"}"#);
        assert!(res.content.contains("I will list files now:"));
        assert!(res.content.contains("Done."));
        assert!(!res.content.contains("<tool>"));
    }

    #[test]
    fn test_parse_deepseek_tool_calls_tag_suffix() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "parameters": {"type": "object"}
            }
        })];
        let text = r#"<tool:bash>{"command": "pwd"}</tool>"#;
        let res = parse_deepseek_tool_calls(text, "call-2", &tools);
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].function.name, "bash");
    }

    #[test]
    fn test_parse_deepseek_tool_calls_xml_children() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "parameters": {"type": "object"}
            }
        })];
        let text = r#"<tool><name>read_file</name><arguments>{"path": "Cargo.toml"}</arguments></tool>"#;
        let res = parse_deepseek_tool_calls(text, "call-3", &tools);
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].function.name, "read_file");
        assert!(res.tool_calls[0].function.arguments.contains("Cargo.toml"));
    }

    #[test]
    fn test_has_malformed_deepseek_tool_markup() {
        assert!(has_malformed_deepseek_tool_markup("<｜｜DSML｜｜ invoke name=\"bash\">"));
        assert!(has_malformed_deepseek_tool_markup("<tool name=\"x\">"));
        assert!(!has_malformed_deepseek_tool_markup("Here is the answer to your question."));
    }

    #[test]
    fn test_normalize_dsml_requires_matching_delimiters() {
        // Regression: DSML_INVOKE_RE used a JS backreference (\1), which the
        // `regex` crate rejects at construction time and which panicked on
        // first use, poisoning the lazy static for every later caller.
        let same = "<｜｜DSML｜｜ invoke name=\"bash\"><｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke>";
        let normalized = normalize_dsml_tool_calls(same);
        assert!(normalized.contains("<tool>"), "got {normalized}");
        assert!(normalized.contains("\"name\":\"bash\""));
        assert!(normalized.contains("\"command\":\"ls\""));

        // Pipe delimiters work too.
        let piped = "<|DSML| invoke name=\"bash\"><|DSML| parameter name=\"command\">pwd</|DSML| parameter></|DSML| invoke>";
        let normalized = normalize_dsml_tool_calls(piped);
        assert!(normalized.contains("\"name\":\"bash\""), "got {normalized}");
        assert!(normalized.contains("\"command\":\"pwd\""));

        // Mismatched delimiters must NOT pair up.
        let mixed = "<｜｜DSML｜｜ invoke name=\"bash\"><｜｜DSML｜｜ parameter name=\"command\">ls</｜｜DSML｜｜ parameter></|DSML| invoke>";
        assert!(!normalize_dsml_tool_calls(mixed).contains("<tool>"));
    }

    #[test]
    fn test_parse_tool_calls_from_text_strips_block() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "weather", "parameters": {"type": "object"}}
        })];
        let (content, calls) =
            parse_tool_calls_from_text("before <tool>{\"name\":\"weather\",\"arguments\":{\"city\":\"Oslo\"}}</tool> after", "cgpt", &tools);
        let calls = calls.expect("calls");
        assert_eq!(calls[0].function.name, "weather");
        assert_eq!(calls[0].id, "cgpt_0");
        assert_eq!(content, "before  after");
    }

    #[test]
    fn test_build_tool_conversation_prompt() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": "calling tool",
                "tool_calls": [{
                    "id": "c1",
                    "function": {"name": "lookup", "arguments": "{\"query\":\"rust\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "found 42 crates"}),
        ];
        let prompt = build_tool_conversation_prompt(&messages, "system rules");
        assert!(prompt.contains("system rules"));
        assert!(prompt.contains("User: hello"));
        assert!(prompt.contains("Tool result (lookup): found 42 crates"));
        assert!(prompt.contains("Continue the task using the tool results above"));
    }
}
