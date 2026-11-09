//! Grok Web client-tool bridge — port of open-sse/executors/grok-web-tools.js
//! (itself ported from OmniRoute tool-bridge/native-tools).
//!
//! Grok's web stream exposes four native tool cards (bash / readFile /
//! webSearch / browsePage). When the client declares OpenAI `tools`, this
//! bridge (1) injects a CLIENT_TOOLS manifest into the prompt so Grok can also
//! call arbitrary tools via `<tool_call>` markup, and (2) maps native cards onto
//! the best-matching declared tool by name/description/schema scoring — in both
//! cases emitting standard OpenAI tool_calls back to the client.

use std::collections::HashSet;
use std::sync::OnceLock;

use indexmap::IndexMap;
use nr_core::Json;
use regex::Regex;
use serde_json::{json, Map};
use uuid::Uuid;

// ── small JS helpers ────────────────────────────────────────────────────────

/// `String(x)` for the value types the bridge meets.
fn js_string(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Null => "null".to_string(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n.to_string(),
        // JS String(object) is the raw toString ("[object Object]"), which
        // normalizeLike() leaves untouched, so mirror that byte-for-byte.
        _ => "[object Object]".to_string(),
    }
}

/// `extractTextContent(msg)`.
pub fn extract_text_content(msg: &Json) -> String {
    let Some(content) = msg.get("content") else {
        return String::new();
    };
    match content {
        Json::String(s) => s.clone(),
        Json::Array(parts) => parts
            .iter()
            .map(|part| match part {
                Json::String(s) => s.clone(),
                other => other
                    .get("text")
                    .map(js_string)
                    .filter(|t| !t.is_empty())
                    .unwrap_or_default(),
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `getLastUserText(messages)`.
pub fn get_last_user_text(messages: &[Json]) -> String {
    for msg in messages.iter().rev() {
        let role = msg.get("role").map(js_string).unwrap_or_default();
        if role == "user" {
            return extract_text_content(msg);
        }
    }
    String::new()
}

/// `stableJson(value)` — object keys sorted at every level.
fn stable_json(value: &Json) -> String {
    fn sorted(value: &Json) -> Json {
        match value {
            Json::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Json::Object(out)
            }
            Json::Array(items) => Json::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

/// `normalizeLike(value)` — collapse whitespace, drop quotes, lowercase.
fn normalize_like(value: &Json) -> String {
    match value {
        Json::String(s) => {
            static WS: OnceLock<Regex> = OnceLock::new();
            static QUOTES: OnceLock<Regex> = OnceLock::new();
            let ws = WS.get_or_init(|| Regex::new(r"\s+").unwrap());
            let quotes = QUOTES.get_or_init(|| Regex::new(r#"["'`]"#).unwrap());
            quotes.replace_all(&ws.replace_all(s, " "), "").to_lowercase()
        }
        other => js_string(other),
    }
}

/// `semanticToolKey(name, args)`.
pub fn semantic_tool_key(name: &str, args: &Json) -> String {
    let Some(record) = args.as_object() else {
        return format!("{name}()");
    };
    let mut keys: Vec<&String> = record.keys().collect();
    keys.sort();
    let parts: Vec<String> = keys
        .into_iter()
        .map(|key| format!("{key}={}", normalize_like(&record[key])))
        .collect();
    format!("{name}({})", parts.join(","))
}

/// `summarizeCompletedToolCall(name, args)`.
fn summarize_completed_tool_call(name: &str, args: &Json) -> String {
    let Some(record) = args.as_object() else {
        return format!("{name}()");
    };
    let mut keys: Vec<&String> = record.keys().collect();
    keys.sort();
    let brief: Vec<String> = keys
        .into_iter()
        .map(|key| js_string(&record[key]).chars().take(60).collect::<String>())
        .collect();
    format!("{name}({})", brief.join(", "))
}

/// `getExecutedToolState(messages)`.
fn get_executed_tool_state(messages: &[Json]) -> (HashSet<String>, Vec<String>) {
    let mut keys = HashSet::new();
    let mut summaries = Vec::new();
    for msg in messages {
        let role = msg.get("role").map(js_string).unwrap_or_default();
        if role != "assistant" {
            continue;
        }
        let Some(calls) = msg.get("tool_calls").and_then(Json::as_array) else {
            continue;
        };
        for call in calls {
            let fn_obj = call.get("function").cloned().unwrap_or(Json::Null);
            let args: Json = fn_obj
                .get("arguments")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| Json::Object(Map::new()));
            let name = fn_obj
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            keys.insert(semantic_tool_key(&name, &args));
            summaries.push(summarize_completed_tool_call(
                if name.is_empty() { "tool" } else { &name },
                &args,
            ));
        }
    }
    (keys, summaries)
}

/// `extractFirstUrl(text)`.
fn extract_first_url(text: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"https?://[^\s"'<>)]+"#).unwrap());
    re.find(text).map(|m| m.as_str().to_string())
}

/// `wantsUrlFetch(text)`.
fn wants_url_fetch(text: &str) -> bool {
    text.contains("http://") || text.contains("https://")
}

/// `forcedToolChoiceName(toolChoice)`.
fn forced_tool_choice_name(tool_choice: &Json) -> Option<String> {
    tool_choice
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Json::as_str)
        .map(String::from)
}

/// `getSchemaProperties(parameters)`.
fn get_schema_properties(parameters: &Json) -> Map<String, Json> {
    parameters
        .get("properties")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default()
}

/// `getSchemaRequired(parameters)`.
fn get_schema_required(parameters: &Json) -> Vec<String> {
    parameters
        .get("required")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(Json::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// `formatToolArgsSummary(parameters)`.
fn format_tool_args_summary(parameters: &Json) -> String {
    let props = get_schema_properties(parameters);
    let required = get_schema_required(parameters);
    let mut segments = Vec::new();
    if !props.is_empty() {
        segments.push(format!(
            "args={}",
            props.keys().cloned().collect::<Vec<_>>().join(",")
        ));
    }
    if !required.is_empty() {
        segments.push(format!("required={}", required.join(",")));
    }
    if segments.is_empty() {
        String::new()
    } else {
        format!(" ({})", segments.join("; "))
    }
}

fn tool_text(tool: &RegistryTool) -> String {
    format!(
        "{} {}",
        tool.name,
        tool.description.as_deref().unwrap_or("")
    )
    .to_lowercase()
}

fn has_any_property(tool: &RegistryTool, names: &[&str]) -> bool {
    let props: HashSet<String> = get_schema_properties(&tool.parameters)
        .keys()
        .map(|k| k.to_lowercase())
        .collect();
    names.iter().any(|n| props.contains(&n.to_lowercase()))
}

fn re_is_match(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).map(|r| r.is_match(text)).unwrap_or(false)
}

fn is_meta_or_infrastructure_tool(tool: &RegistryTool) -> bool {
    re_is_match(
        r"\b(mcp|mcpproxy|upstream|registry|oauth|cache key|token usage|handoff|context management|memory|memories|lsp|language server|plan file|server management|tool discovery)\b",
        &tool_text(tool),
    )
}

fn is_context_memory_tool(tool: &RegistryTool) -> bool {
    re_is_match(
        r"\b(ctx_|memory|memories|conversation history|session notes|project memories|context\.db)\b",
        &tool_text(tool),
    )
}

fn is_terminal_tool(tool: &RegistryTool) -> bool {
    if is_meta_or_infrastructure_tool(tool) {
        return false;
    }
    let text = tool_text(tool);
    let name = tool.name.to_lowercase();
    let explicit_name = re_is_match(
        r"\b(bash|shell|terminal|run_command|execute_command|exec|command)\b",
        &name,
    );
    let explicit_text = re_is_match(
        r"\b(?:run|execute).{0,24}\b(?:shell|bash|terminal|command)\b|\b(?:shell|bash|terminal)\b",
        &text,
    );
    explicit_name || (has_any_property(tool, &["command", "cmd", "shell"]) && explicit_text)
}

fn is_file_read_tool(tool: &RegistryTool) -> bool {
    let text = tool_text(tool);
    has_any_property(tool, &["filePath", "file_path", "path"])
        && re_is_match(r"\b(read|file|filesystem|open)\b", &text)
        && !re_is_match(
            r"\b(write|edit|patch|delete|remove|grep|search|bash|shell|command)\b",
            &text,
        )
}

fn is_url_fetch_tool(tool: &RegistryTool) -> bool {
    let text = tool_text(tool);
    let name = tool.name.to_lowercase();
    let explicit_name = re_is_match(
        r"\b(webfetch|web.fetch|fetch_url|url_fetch|read_url|browse_page|browsepage)\b",
        &name,
    );
    let explicit_url_text = re_is_match(
        r"\b(?:fetch|browse|read).{0,32}\b(?:url|uri|web page|page content)\b|\b(?:url|uri|web page|page content).{0,32}\b(?:fetch|browse|read)\b",
        &text,
    );
    explicit_name
        || (!is_meta_or_infrastructure_tool(tool)
            && has_any_property(tool, &["url", "uri"])
            && explicit_url_text)
}

fn is_web_search_tool(tool: &RegistryTool) -> bool {
    let text = tool_text(tool);
    has_any_property(tool, &["query", "search"])
        && re_is_match(r"\b(web|internet|exa|browser|browse|serp)\b", &text)
        && !is_meta_or_infrastructure_tool(tool)
        && !is_context_memory_tool(tool)
}

// ── registry ────────────────────────────────────────────────────────────────

/// One declared OpenAI function tool, normalized by `buildToolRegistry`.
#[derive(Clone, Debug, Default)]
pub struct RegistryTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Json,
}

/// `buildToolRegistry(body)` result.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    pub enabled: bool,
    pub tools_by_name: IndexMap<String, RegistryTool>,
    pub last_user_text: String,
    pub executed_tool_keys: HashSet<String>,
    pub completed_tool_calls: Vec<String>,
}

impl ToolRegistry {
    pub fn find(&self, name: &str) -> Option<&RegistryTool> {
        self.tools_by_name.get(name)
    }
}

/// `buildToolRegistry(body)`.
pub fn build_tool_registry(body: &Json) -> ToolRegistry {
    let tools = body
        .get("tools")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let messages = body
        .get("messages")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let tool_choice = body.get("tool_choice").cloned().unwrap_or(json!("auto"));
    let (executed_keys, summaries) = get_executed_tool_state(&messages);

    let last_user_text = get_last_user_text(&messages);

    if tool_choice.as_str() == Some("none") {
        return ToolRegistry {
            enabled: false,
            tools_by_name: IndexMap::new(),
            last_user_text,
            executed_tool_keys: executed_keys,
            completed_tool_calls: summaries,
        };
    }

    let mut function_tools: Vec<RegistryTool> = Vec::new();
    for tool in &tools {
        if tool.get("type").and_then(Json::as_str) != Some("function") {
            continue;
        }
        let Some(fn_obj) = tool.get("function").and_then(Json::as_object) else {
            continue;
        };
        let name = fn_obj
            .get("name")
            .and_then(Json::as_str)
            .map(str::trim)
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        function_tools.push(RegistryTool {
            name: name.to_string(),
            description: fn_obj
                .get("description")
                .and_then(Json::as_str)
                .map(String::from),
            parameters: fn_obj
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        });
    }

    let forced = forced_tool_choice_name(&tool_choice);
    let visible: Vec<RegistryTool> = match forced {
        Some(name) => function_tools.into_iter().filter(|t| t.name == name).collect(),
        None => function_tools,
    };

    ToolRegistry {
        enabled: !visible.is_empty(),
        tools_by_name: visible.into_iter().map(|t| (t.name.clone(), t)).collect(),
        last_user_text,
        executed_tool_keys: executed_keys,
        completed_tool_calls: summaries,
    }
}

fn base_tool_order_score(tool: &RegistryTool) -> i64 {
    if is_url_fetch_tool(tool) {
        return 90;
    }
    if is_web_search_tool(tool) {
        return 85;
    }
    if is_file_read_tool(tool) {
        return 75;
    }
    if is_terminal_tool(tool) {
        return 70;
    }
    let text = tool_text(tool);
    if re_is_match(r"\b(glob|grep|search files?|file search|content search)\b", &text) {
        return 60;
    }
    if re_is_match(r"\b(edit|write|patch|modify|apply)\b", &text) {
        return 50;
    }
    if re_is_match(r"\b(task|agent|delegate|subagent)\b", &text) {
        return 40;
    }
    if is_context_memory_tool(tool) {
        return 20;
    }
    if is_meta_or_infrastructure_tool(tool) {
        return 10;
    }
    30
}

/// `orderedTools(registry)` — descending score, declaration order on ties.
fn ordered_tools(registry: &ToolRegistry) -> Vec<&RegistryTool> {
    let mut scored: Vec<(usize, i64, &RegistryTool)> = registry
        .tools_by_name
        .values()
        .enumerate()
        .map(|(index, tool)| (index, base_tool_order_score(tool), tool))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(_, _, tool)| tool).collect()
}

/// `buildClientToolManifest(registry, toolChoice)`.
pub fn build_client_tool_manifest(registry: &ToolRegistry, tool_choice: &Json) -> String {
    if !registry.enabled {
        return String::new();
    }
    let mut lines = vec![
        "CLIENT_TOOLS: use this caller-runtime tool list as the tool interface for this request. To call one, respond only with <tool_call>{\"name\":\"exact_tool_name\",\"arguments\":{...}}</tool_call>. After tool results, answer normally.".to_string(),
        format!("tool_choice={}", tool_choice.to_string()),
    ];
    if !registry.completed_tool_calls.is_empty() {
        lines.push("completed_tool_calls:".to_string());
        for summary in &registry.completed_tool_calls {
            lines.push(format!("- {summary}"));
        }
        lines.push(
            "Do not repeat completed tool calls unless a different result is required; use their tool results to answer."
                .to_string(),
        );
    }
    lines.push("tools (priority order for this request):".to_string());
    for (index, tool) in ordered_tools(registry).into_iter().enumerate() {
        let mut line = format!(
            "{}. name: {}{}",
            index + 1,
            tool.name,
            format_tool_args_summary(&tool.parameters)
        );
        if let Some(desc) = tool.description.as_deref().filter(|d| !d.is_empty()) {
            line.push_str(&format!("\n   description: {desc}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// `buildGrokMessage(messages, registry, toolChoice)` — tool-aware single-message packing.
pub fn build_grok_message(
    messages: &[Json],
    registry: &ToolRegistry,
    tool_choice: &Json,
) -> String {
    let manifest = build_client_tool_manifest(registry, tool_choice);
    let last_user_source_idx = messages
        .iter()
        .rposition(|msg| msg.get("role").map(js_string).unwrap_or_default() == "user");

    let mut extracted: Vec<(String, String)> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        let raw_role = msg.get("role").map(js_string).unwrap_or_else(|| "user".into());
        let role = if raw_role == "developer" {
            "system".to_string()
        } else {
            raw_role
        };
        let mut content = extract_text_content(msg);
        if role == "tool" {
            if Some(i) < last_user_source_idx {
                continue;
            }
            let tool_name = msg
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or("unknown_tool");
            let call_id = msg
                .get("tool_call_id")
                .and_then(Json::as_str)
                .unwrap_or("unknown_call");
            content = format!(
                "CLIENT TOOL RESULT from caller runtime for {tool_name} ({call_id}). Use this result to answer; do not call the same tool again:\n{content}"
            );
        } else if role == "assistant" && msg.get("tool_calls").and_then(Json::as_array).is_some() {
            if Some(i) < last_user_source_idx {
                continue;
            }
            let calls: Vec<Json> = msg
                .get("tool_calls")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .map(|call| {
                            json!({
                                "id": call.get("id").cloned().unwrap_or(Json::Null),
                                "function": call.get("function").cloned().unwrap_or(Json::Null),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let rendered = Json::Array(calls).to_string();
            content = [content, format!("Previous assistant tool calls: {rendered}")]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
        }
        if content.trim().is_empty() {
            continue;
        }
        extracted.push((role, content));
    }

    let last_user_idx = extracted.iter().rposition(|(role, _)| role == "user");
    let mut parts: Vec<String> = Vec::new();
    for (i, (role, text)) in extracted.iter().enumerate() {
        if Some(i) == last_user_idx {
            parts.push(text.clone());
        } else {
            parts.push(format!("{role}: {text}"));
        }
    }
    if !manifest.trim().is_empty() {
        parts.push(manifest.trim().to_string());
    }
    parts.join("\n\n")
}

// ── argument adaptation ─────────────────────────────────────────────────────

fn first_string(values: &[&Json]) -> Option<String> {
    values
        .iter()
        .find_map(|v| v.as_str())
        .map(String::from)
}

fn has_value(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn default_required_value(key: &str, args: &Json, intent: &str) -> Option<String> {
    let lower = key.to_lowercase();
    let command = first_string(&[&args["command"], &args["cmd"], &args["shell"], &args["input"]]);
    let path = first_string(&[
        &args["filePath"],
        &args["file_path"],
        &args["path"],
        &args["filename"],
    ]);
    let query = first_string(&[&args["query"], &args["search"], &args["input"]]);
    let url = first_string(&[&args["url"], &args["uri"]]);
    match lower.as_str() {
        "command" | "cmd" => command,
        "filepath" | "file_path" | "path" => path,
        "query" | "search" => query,
        "url" | "uri" => url,
        "input" => query.or(url).or(command).or(path),
        "description" | "reason" => {
            if let Some(c) = command {
                Some(format!("Execute shell command: {c}"))
            } else if let Some(p) = path {
                Some(format!("Read file: {p}"))
            } else if let Some(u) = url {
                Some(format!("Fetch URL: {u}"))
            } else if let Some(q) = query {
                Some(format!("Search: {q}"))
            } else {
                Some(format!("Grok Web {intent} tool call"))
            }
        }
        _ => None,
    }
}

/// `adaptArguments(toolName, args, registry, intent)` — map common aliases onto
/// the declared schema, fill required keys, drop keys the schema does not know.
pub fn adapt_arguments(tool_name: &str, args: Json, registry: &ToolRegistry, intent: &str) -> Json {
    let Some(tool) = registry.find(tool_name) else {
        return args;
    };
    let properties = get_schema_properties(&tool.parameters);
    let required = get_schema_required(&tool.parameters);
    let mut out: Map<String, Json> = args.as_object().cloned().unwrap_or_default();

    let alias = |out: &mut Map<String, Json>, target: &str, sources: &[&str]| {
        if !properties.contains_key(target) {
            return;
        }
        if out.get(target).is_some_and(has_value) {
            return;
        }
        let candidates: Vec<&Json> = sources
            .iter()
            .map(|s| out.get(*s).unwrap_or(&Json::Null))
            .collect();
        if let Some(picked) = first_string(&candidates) {
            out.insert(target.to_string(), Json::String(picked));
        }
    };

    alias(&mut out, "filePath", &["filePath", "file_path", "path"]);
    alias(&mut out, "file_path", &["file_path", "filePath", "path"]);
    alias(&mut out, "path", &["path", "filePath", "file_path"]);
    alias(&mut out, "query", &["query", "search", "input"]);
    alias(&mut out, "url", &["url", "uri"]);
    alias(&mut out, "uri", &["uri", "url"]);
    alias(&mut out, "input", &["input", "query", "command"]);

    for key in &required {
        if out.get(key).is_some_and(has_value) {
            continue;
        }
        let args_view = Json::Object(out.clone());
        if let Some(value) = default_required_value(key, &args_view, intent) {
            out.insert(key.clone(), Json::String(value));
        }
    }

    let mut known: HashSet<String> = properties.keys().cloned().collect();
    known.extend(required.iter().cloned());
    if known.is_empty() {
        return Json::Object(out);
    }
    let filtered: Map<String, Json> = out.into_iter().filter(|(k, _)| known.contains(k)).collect();
    Json::Object(filtered)
}

// ── native card mapping (bash / readFile / webSearch / browsePage) ──────────

fn tool_score(tool: &RegistryTool, intent: &str, last_user_text: &str) -> i64 {
    let name = tool.name.to_lowercase();
    let description = tool.description.as_deref().unwrap_or("").to_lowercase();
    let prop_names: HashSet<String> = get_schema_properties(&tool.parameters)
        .keys()
        .map(|k| k.to_lowercase())
        .collect();
    let text = format!("{name} {description}");
    let user_text = last_user_text.to_lowercase();
    let mut score: i64 = 0;

    let has = |k: &str| prop_names.contains(k);
    let name_in = |names: &[&str]| names.iter().any(|n| *n == name);
    let text_matches = |pattern: &str| re_is_match(pattern, &text);
    let name_matches = |pattern: &str| re_is_match(pattern, &name);

    if intent == "bash" {
        if !is_terminal_tool(tool) {
            score -= 80;
        }
        if name == "bash" {
            score += 100;
        }
        if name_in(&[
            "shell",
            "terminal",
            "run_command",
            "execute_command",
            "exec",
            "command",
        ]) {
            score += 80;
        }
        if has("command") || has("cmd") {
            score += 60;
        }
        if text_matches(r"bash|shell|terminal|command|execute|run") {
            score += 25;
        }
        if name_matches(r"read|search|grep|web|http|browser|context|note|memory") {
            score -= 50;
        }
    } else if intent == "readFile" {
        if !is_file_read_tool(tool) {
            score -= 60;
        }
        if name_in(&["read", "read_file", "readfile", "file_read"]) {
            score += 100;
        }
        if has("filepath") || has("file_path") || has("path") {
            score += 50;
        }
        if text_matches(r"read.*file|file.*read|filesystem") {
            score += 25;
        }
        if text_matches(r"write|edit|delete|remove|bash|shell|command") {
            score -= 50;
        }
    } else if intent == "webSearch" || intent == "browsePage" {
        if is_context_memory_tool(tool) || is_meta_or_infrastructure_tool(tool) {
            score -= 180;
        }
        if intent == "browsePage" || wants_url_fetch(&user_text) {
            if !is_url_fetch_tool(tool) {
                score -= 60;
            }
            if name_matches(r"webfetch|web_fetch|fetch|browse|browse_page|read_url|url_fetch|page") {
                score += 140;
            }
            if has("url") || has("uri") {
                score += 90;
            }
            if text_matches(r"fetch|browse|url|web page|page content|extract.*url|read.*url") {
                score += 55;
            }
            if name_matches(r"websearch|web_search|search") && !(has("url") || has("uri")) {
                score -= 80;
            }
        }
        if intent == "webSearch" && !is_web_search_tool(tool) {
            score -= 60;
        }
        if name_in(&["web_search", "websearch", "search"]) {
            score += 100;
        }
        if has("query") || has("search") {
            score += 50;
        }
        if text_matches(r"web.*search|search.*web|internet|browse") {
            score += 25;
        }
        if text_matches(r"file|bash|shell|command|write|edit") {
            score -= 50;
        }
    }
    score
}

fn pick_declared_tool(intent: &str, registry: &ToolRegistry) -> Option<String> {
    let mut best: Option<(String, i64)> = None;
    for tool in registry.tools_by_name.values() {
        let score = tool_score(tool, intent, &registry.last_user_text);
        if score <= 0 {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| score > *b) {
            best = Some((tool.name.clone(), score));
        }
    }
    best.map(|(name, _)| name)
}

/// `mapNativeToolCard(card, cardId, registry)` — native card → OpenAI tool_call.
pub fn map_native_tool_card(
    card: &Json,
    card_id: Option<&str>,
    registry: &ToolRegistry,
) -> Option<Json> {
    if !registry.enabled || !card.is_object() {
        return None;
    }
    let id = match card_id.filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => format!("call_{}", Uuid::new_v4()),
    };

    let try_intent = |intent: &str, raw_args: Option<&Json>| -> Option<Json> {
        let args = raw_args?;
        if !args.is_object() {
            return None;
        }
        let name = pick_declared_tool(intent, registry)?;
        let adapted = adapt_arguments(&name, args.clone(), registry, intent);
        if registry
            .executed_tool_keys
            .contains(&semantic_tool_key(&name, &adapted))
        {
            return None;
        }
        Some(json!({
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": adapted.to_string()},
        }))
    };

    if let Some(call) = try_intent("bash", card.get("bash").and_then(|b| b.get("args"))) {
        return Some(call);
    }
    let read_file = card.get("readFile").or_else(|| card.get("read_file"));
    if let Some(call) = try_intent("readFile", read_file.and_then(|b| b.get("args"))) {
        return Some(call);
    }
    if let Some(call) = try_intent(
        "webSearch",
        card.get("webSearch").and_then(|b| b.get("args")),
    ) {
        return Some(call);
    }
    let browse = card.get("browsePage").or_else(|| card.get("browse_page"));
    if let Some(call) = try_intent("browsePage", browse.and_then(|b| b.get("args"))) {
        return Some(call);
    }
    None
}

// ── markup fallback for arbitrary tools ────────────────────────────────────

fn tag_re(pattern: &str) -> &'static Regex {
    // Two fixed patterns only; cache them so streaming calls stay cheap.
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static CLOSE: OnceLock<Regex> = OnceLock::new();
    if pattern == "<tool_call>" {
        OPEN.get_or_init(|| Regex::new(r"<tool_call>").unwrap())
    } else {
        CLOSE.get_or_init(|| Regex::new(r"</tool_call>").unwrap())
    }
}

/// `findTagBlocks(text, openRe, closeRe)` — yields each block's inner text.
fn find_tag_blocks(text: &str, open_re: &Regex, close_re: &Regex) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = text.to_string();
    loop {
        let Some(open) = open_re.find(&rest) else {
            return blocks;
        };
        let inner_start = open.end();
        let tail = &rest[inner_start..];
        let Some(close) = close_re.find(tail) else {
            return blocks;
        };
        blocks.push(tail[..close.start()].to_string());
        rest = tail[close.end()..].to_string();
        if !rest.contains("<tool_call>") {
            return blocks;
        }
    }
}

/// `normalizeArbitraryArgs(value)`.
fn normalize_arbitrary_args(value: &Json) -> Json {
    match value {
        Json::Null | Json::Bool(false) => Json::Object(Map::new()),
        Json::String(s) => match serde_json::from_str::<Json>(s) {
            Ok(parsed) if parsed.is_object() => parsed,
            _ => json!({ "input": s }),
        },
        Json::Object(_) => value.clone(),
        Json::Array(_) => value.clone(),
        _ => Json::Object(Map::new()),
    }
}

/// `parseClientToolCallMarkup(text, registry)` — extract `<tool_call>` blocks.
pub fn parse_client_tool_call_markup(text: &str, registry: &ToolRegistry) -> Option<Vec<Json>> {
    if !registry.enabled || !text.contains("<tool_call>") {
        return None;
    }
    let mut calls = Vec::new();
    for inner in find_tag_blocks(text, tag_re("<tool_call>"), tag_re("</tool_call>")) {
        let parsed: Json = match serde_json::from_str(inner.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !parsed.is_object() {
            continue;
        }
        let name = parsed
            .get("name")
            .and_then(Json::as_str)
            .map(str::trim)
            .unwrap_or("");
        if name.is_empty() || registry.find(name).is_none() {
            continue;
        }
        let args = adapt_arguments(
            name,
            normalize_arbitrary_args(parsed.get("arguments").unwrap_or(&Json::Null)),
            registry,
            "clientTool",
        );
        if registry
            .executed_tool_keys
            .contains(&semantic_tool_key(name, &args))
        {
            continue;
        }
        let id = parsed
            .get("id")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("call_{}", Uuid::new_v4()));
        calls.push(json!({
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": args.to_string()},
        }));
    }
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// `hasOpenToolCallMarkup(text)` — true while a `<tool_call>` block may still
/// be streaming in.
pub fn has_open_tool_call_markup(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<tool(?:_call)?$|<tool_call[^>]*$").unwrap());
    re.is_match(text) || (text.contains("<tool_call>") && !text.contains("</tool_call>"))
}

/// `stripClientToolManifest(text)` — remove the injected manifest from text
/// destined back to the client.
pub fn strip_client_tool_manifest(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    // JS: /CLIENT_TOOLS: [\s\S]*?(?=\n\n|$)/. Rust `regex` has no lookahead,
    // so drive the lazy scan by hand and fall back to the end of the text.
    let re = RE.get_or_init(|| Regex::new("CLIENT_TOOLS: ").unwrap());
    let Some(m) = re.find(text) else {
        return text.trim().to_string();
    };
    let start = m.start();
    let rest = &text[m.end()..];
    let end = match rest.find("\n\n") {
        Some(idx) => m.end() + idx,
        None => text.len(),
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry_with(tools: Json) -> ToolRegistry {
        build_tool_registry(&json!({
            "tools": tools,
            "messages": [{"role": "user", "content": "hi"}],
        }))
    }

    fn terminal_tool(name: &str) -> Json {
        json!({"type": "function", "function": {
            "name": name,
            "description": "run a shell command",
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
        }})
    }

    #[test]
    fn extract_text_content_matches_js() {
        assert_eq!(
            extract_text_content(&json!({"content": [{"type": "text", "text": "a"}, {"type": "image_url"}, "b"]})),
            "a\nb"
        );
        assert_eq!(extract_text_content(&json!({"content": "raw"})), "raw");
        assert_eq!(extract_text_content(&json!({"content": null})), "");
    }

    #[test]
    fn normalize_like_collapses_and_lowercases() {
        // JS keeps the leading/trailing single spaces (\s+ collapses to one).
        assert_eq!(normalize_like(&json!(" Hello \"World\" ")), " hello world ");
        assert_eq!(normalize_like(&json!("a   b\tc")), "a b c");
        assert_eq!(normalize_like(&json!(42)), "42");
    }

    #[test]
    fn semantic_key_sorts_keys() {
        assert_eq!(semantic_tool_key("t", &json!({"b": "x", "a": "y"})), "t(a=y,b=x)");
        assert_eq!(semantic_tool_key("t", &Json::Null), "t()");
    }

    #[test]
    fn summarize_truncates_to_60_chars() {
        let long = "x".repeat(80);
        let summary = summarize_completed_tool_call("t", &json!({"arg": long}));
        assert_eq!(summary, format!("t({})", "x".repeat(60)));
    }

    #[test]
    fn tool_choice_none_disables_registry() {
        let registry = build_tool_registry(&json!({
            "tools": [terminal_tool("bash")],
            "tool_choice": "none",
            "messages": [],
        }));
        assert!(!registry.enabled);
        assert!(registry.tools_by_name.is_empty());
    }

    #[test]
    fn non_function_tools_are_skipped() {
        let registry = registry_with(json!([
            {"type": "web_search"},
            {"type": "function", "function": {}},
            {"type": "function", "function": {"name": "   "}},
        ]));
        assert!(!registry.enabled);
    }

    #[test]
    fn forced_tool_choice_keeps_only_named() {
        let registry = build_tool_registry(&json!({
            "tools": [terminal_tool("bash"), terminal_tool("run_cmd")],
            "tool_choice": {"type": "function", "function": {"name": "run_cmd"}},
            "messages": [],
        }));
        assert!(registry.enabled);
        assert_eq!(registry.tools_by_name.len(), 1);
        assert!(registry.find("run_cmd").is_some());
    }

    #[test]
    fn manifest_orders_by_priority() {
        let registry = registry_with(json!([
            terminal_tool("run_cmd"),
            {"type": "function", "function": {
                "name": "web_fetch",
                "description": "fetch a url",
                "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}
            }}
        ]));
        let manifest = build_client_tool_manifest(&registry, &json!("auto"));
        assert!(manifest.starts_with("CLIENT_TOOLS:"));
        assert!(manifest.contains("tool_choice=\"auto\""));
        let fetch_pos = manifest.find("name: web_fetch").expect("fetch listed");
        let cmd_pos = manifest.find("name: run_cmd").expect("cmd listed");
        assert!(fetch_pos < cmd_pos, "url fetch outranks terminal");
        assert!(manifest.contains("1. name: web_fetch (args=url)"));
        assert!(manifest.contains("2. name: run_cmd (args=command)"));
    }

    #[test]
    fn manifest_lists_completed_calls() {
        let registry = build_tool_registry(&json!({
            "tools": [terminal_tool("bash")],
            "messages": [{
                "role": "assistant",
                "tool_calls": [{"function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}]
            }],
        }));
        let manifest = build_client_tool_manifest(&registry, &Json::Null);
        assert!(manifest.contains("completed_tool_calls:"));
        assert!(manifest.contains("- bash(ls)"));
        assert!(manifest.contains("Do not repeat completed tool calls"));
    }

    #[test]
    fn manifest_empty_when_disabled() {
        assert_eq!(
            build_client_tool_manifest(&ToolRegistry::default(), &json!("auto")),
            ""
        );
    }

    #[test]
    fn adapt_arguments_allies_aliases_and_drops_unknown() {
        let registry = registry_with(json!([{"type": "function", "function": {
            "name": "read_file",
            "description": "read a file from disk",
            "parameters": {"type": "object", "properties": {"filePath": {"type": "string"}}, "required": ["filePath"]}
        }}]));
        let adapted = adapt_arguments("read_file", json!({"path": "/tmp/x"}), &registry, "readFile");
        assert_eq!(adapted["filePath"], "/tmp/x");
        assert_eq!(adapted.as_object().unwrap().len(), 1, "unknown keys dropped");
    }

    #[test]
    fn adapt_arguments_fills_required_defaults() {
        let registry = registry_with(json!([{"type": "function", "function": {
            "name": "bash",
            "description": "run a shell command",
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string"}, "description": {"type": "string"}},
                "required": ["command", "description"]
            }
        }}]));
        // `cmd` is not in the schema, so it is dropped, but the required
        // `command` is backfilled from it before filtering.
        let adapted = adapt_arguments("bash", json!({"cmd": "ls -la"}), &registry, "bash");
        assert_eq!(adapted["command"], "ls -la");
        assert_eq!(adapted["description"], "Execute shell command: ls -la");
    }

    #[test]
    fn adapt_arguments_passes_through_undeclared_tool() {
        let registry = ToolRegistry::default();
        let args = json!({"anything": 1});
        assert_eq!(adapt_arguments("nope", args.clone(), &registry, "bash"), args);
    }

    #[test]
    fn map_native_tool_card_bash() {
        let registry = registry_with(json!([terminal_tool("bash")]));
        let call = map_native_tool_card(&json!({"bash": {"args": {"command": "ls"}}}), Some("call_1"), &registry)
            .expect("mapped");
        assert_eq!(call["function"]["name"], "bash");
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["function"]["arguments"], r#"{"command":"ls"}"#);
    }

    #[test]
    fn map_native_tool_card_read_file_alias() {
        let registry = registry_with(json!([{"type": "function", "function": {
            "name": "read",
            "description": "read a file from disk",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
        }}]));
        let call = map_native_tool_card(
            &json!({"read_file": {"args": {"path": "/etc/hosts"}}}),
            None,
            &registry,
        )
        .expect("mapped");
        assert_eq!(call["function"]["name"], "read");
        assert_eq!(call["function"]["arguments"], r#"{"path":"/etc/hosts"}"#);
        assert!(call["id"].as_str().unwrap().starts_with("call_"));
    }

    #[test]
    fn map_native_tool_card_prefers_bash_over_others() {
        let registry = registry_with(json!([terminal_tool("bash"), {
            "type": "function",
            "function": {"name": "fetch_url", "description": "fetch a url", "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}}
        }]));
        let call = map_native_tool_card(
            &json!({"bash": {"args": {"command": "ls"}}, "webSearch": {"args": {"query": "x"}}}),
            Some("c"),
            &registry,
        )
        .expect("mapped");
        assert_eq!(call["function"]["name"], "bash");
    }

    #[test]
    fn map_native_tool_card_no_match_returns_none() {
        let registry = registry_with(json!([{
            "type": "function",
            "function": {"name": "memory_save", "description": "save project memories", "parameters": {"type": "object", "properties": {}}}
        }]));
        assert!(map_native_tool_card(&json!({"bash": {"args": {"command": "ls"}}}), None, &registry).is_none());
        assert!(map_native_tool_card(&Json::Null, None, &registry).is_none());
    }

    #[test]
    fn map_native_tool_card_skips_executed() {
        let registry = build_tool_registry(&json!({
            "tools": [terminal_tool("bash")],
            "messages": [{
                "role": "assistant",
                "tool_calls": [{"function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}]
            }],
        }));
        assert!(map_native_tool_card(&json!({"bash": {"args": {"command": "ls"}}}), None, &registry).is_none());
    }

    #[test]
    fn parse_markup_extracts_calls() {
        let registry = registry_with(json!([{
            "type": "function",
            "function": {"name": "weather", "description": "weather lookup", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}
        }]));
        let text = "<tool_call>{\"name\":\"weather\",\"arguments\":{\"city\":\"Paris\"}}</tool_call>";
        let calls = parse_client_tool_call_markup(text, &registry).expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "weather");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"Paris"}"#);
        assert!(calls[0]["id"].as_str().unwrap().starts_with("call_"));
    }

    #[test]
    fn parse_markup_keeps_supplied_id_and_back_to_back_blocks() {
        let registry = registry_with(json!([{
            "type": "function",
            "function": {"name": "weather", "description": "weather lookup", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}
        }]));
        let text = "<tool_call>{\"id\":\"a\",\"name\":\"weather\",\"arguments\":{\"city\":\"A\"}}</tool_call>\
                    <tool_call>{\"id\":\"b\",\"name\":\"weather\",\"arguments\":{\"city\":\"B\"}}</tool_call>";
        let calls = parse_client_tool_call_markup(text, &registry).expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "a");
        assert_eq!(calls[1]["id"], "b");
    }

    #[test]
    fn parse_markup_drops_unknown_and_malformed() {
        let registry = registry_with(json!([{
            "type": "function",
            "function": {"name": "weather", "description": "weather lookup", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}
        }]));
        assert!(parse_client_tool_call_markup("<tool_call>{oops}</tool_call>", &registry).is_none());
        assert!(parse_client_tool_call_markup("<tool_call>{\"name\":\"nope\"}</tool_call>", &registry).is_none());
        assert!(parse_client_tool_call_markup("plain text", &registry).is_none());
        assert!(parse_client_tool_call_markup("<tool_call>{\"name\":\"weather\"}</tool_call>", &ToolRegistry::default()).is_none());
    }

    #[test]
    fn parse_markup_stringifies_string_arguments() {
        let registry = registry_with(json!([{
            "type": "function",
            "function": {"name": "weather", "description": "weather lookup", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}
        }]));
        let text = "<tool_call>{\"name\":\"weather\",\"arguments\":\"{\\\"city\\\":\\\"Oslo\\\"}\"}</tool_call>";
        let calls = parse_client_tool_call_markup(text, &registry).expect("calls");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"Oslo"}"#);
    }

    #[test]
    fn has_open_tool_call_markup_detects_partials() {
        assert!(has_open_tool_call_markup("answer <tool"));
        assert!(has_open_tool_call_markup("answer <tool_call>{\"name\":"));
        assert!(has_open_tool_call_markup("<tool_call>{}"));
        assert!(!has_open_tool_call_markup("plain answer"));
        assert!(!has_open_tool_call_markup("<tool_call>{}</tool_call>"));
    }

    #[test]
    fn strip_manifest_removes_injected_block() {
        // JS leaves the surrounding blank lines; only the ends are trimmed.
        let text = "answer here\n\nCLIENT_TOOLS: list\n1. name: bash\n   description: shell\n\nafter";
        assert_eq!(strip_client_tool_manifest(text), "answer here\n\n\n\nafter");
        assert_eq!(strip_client_tool_manifest(""), "");
        assert_eq!(strip_client_tool_manifest("no manifest"), "no manifest");
        assert_eq!(
            strip_client_tool_manifest("prefix CLIENT_TOOLS: only manifest"),
            "prefix"
        );
    }

    #[test]
    fn grok_message_keeps_tail_tool_results_only() {
        let messages = json!([
            {"role": "tool", "name": "bash", "tool_call_id": "old", "content": "stale"},
            {"role": "user", "content": "run it"},
            {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "bash", "arguments": "{}"}}]},
            {"role": "tool", "name": "bash", "tool_call_id": "c1", "content": "ok"}
        ]);
        let message = build_grok_message(messages.as_array().unwrap(), &ToolRegistry::default(), &json!("auto"));
        assert!(!message.contains("stale"));
        assert!(message.contains("CLIENT TOOL RESULT from caller runtime for bash (c1)"));
        assert!(message.contains("Previous assistant tool calls:"));
        // The last user turn is unprefixed; everything else is role-prefixed.
        assert!(message.starts_with("run it\n\nassistant: Previous assistant tool calls:"));
        assert!(message.contains("\n\ntool: CLIENT TOOL RESULT"));
    }

    #[test]
    fn grok_message_plain_roles_and_last_user_unprefixed() {
        let messages = json!([
            {"role": "developer", "content": "be nice"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "hi"}
        ]);
        let message = build_grok_message(messages.as_array().unwrap(), &ToolRegistry::default(), &Json::Null);
        assert_eq!(message, "system: be nice\n\nassistant: hello\n\nhi");
    }

    #[test]
    fn grok_message_appends_manifest() {
        let messages = json!([{"role": "user", "content": "hi"}]);
        let registry = registry_with(json!([terminal_tool("bash")]));
        let message = build_grok_message(messages.as_array().unwrap(), &registry, &json!("auto"));
        assert!(message.starts_with("hi\n\nCLIENT_TOOLS:"));
    }

    #[test]
    fn extract_first_url_and_wants_url_fetch() {
        assert_eq!(
            extract_first_url("see https://example.com/x now").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(extract_first_url("no url"), None);
        assert!(wants_url_fetch("open https://a.b"));
        assert!(!wants_url_fetch("open a.b"));
    }

    #[test]
    fn stable_json_sorts_keys() {
        assert_eq!(stable_json(&json!({"b": 1, "a": {"d": 2, "c": 3}})), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }
}
