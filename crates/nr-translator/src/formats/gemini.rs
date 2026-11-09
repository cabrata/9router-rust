//! Gemini format helpers. Port of open-sse/translator/formats/gemini.js.

use std::collections::HashSet;
use once_cell::sync::Lazy;
use rand::Rng;
use serde_json::json;
use uuid::Uuid;

use nr_core::Json;
use crate::schema::{defaults, gemini_finish, gemini_roles, openai_blocks, openai_finish, roles};

/// Unsupported JSON Schema constraints that should be removed for Antigravity / Gemini.
pub const UNSUPPORTED_SCHEMA_CONSTRAINTS: &[&str] = &[
    "minLength", "maxLength", "exclusiveMinimum", "exclusiveMaximum",
    "minItems", "maxItems", "format", "multipleOf", "uniqueItems", "contains",
    "unevaluatedProperties", "unevaluatedItems", "contentSchema", "prefixItems",
    "additionalItems", "default", "examples", "$schema", "$defs", "definitions",
    "const", "$ref", "$comment", "deprecated", "readOnly", "writeOnly",
    "additionalProperties", "propertyNames", "patternProperties", "enumDescriptions",
    "anyOf", "oneOf", "allOf", "not", "dependencies", "dependentSchemas", "dependentRequired",
    "title", "optional", "if", "then", "else", "contentMediaType", "contentEncoding",
    "cornerRadius", "fillColor", "fontFamily", "fontSize", "fontWeight",
    "gap", "padding", "strokeColor", "strokeThickness", "textColor",
    "errorMessage", "errorMessages", "x-errorMessage", "x-errorMessages",
    "markdownDescription", "x-intellij-html-description", "x-taplo-info", "x-taplo",
    "doNotSuggest", "suggestSortText", "minProperties", "maxProperties",
];

static UNSUPPORTED_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| UNSUPPORTED_SCHEMA_CONSTRAINTS.iter().copied().collect());

/// Default safety settings
pub fn default_safety_settings() -> Json {
    json!([
        {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "OFF"}
    ])
}

fn remote_image_mime_type(url: &str) -> &'static str {
    let pathname = match reqwest::Url::parse(url) {
        Ok(u) => u.path().to_lowercase(),
        Err(_) => return defaults::DEFAULT_IMAGE_MIME,
    };
    if pathname.ends_with(".jpg") || pathname.ends_with(".jpeg") {
        "image/jpeg"
    } else if pathname.ends_with(".webp") {
        "image/webp"
    } else if pathname.ends_with(".gif") {
        "image/gif"
    } else if pathname.ends_with(".bmp") {
        "image/bmp"
    } else {
        defaults::DEFAULT_IMAGE_MIME
    }
}

/// Convert OpenAI content (string or array of blocks) to Gemini parts.
pub fn convert_openai_content_to_parts(content: &Json) -> Vec<Json> {
    let mut parts = Vec::new();
    if let Some(text) = content.as_str() {
        parts.push(json!({"text": text}));
        return parts;
    }
    let Some(items) = content.as_array() else {
        return parts;
    };

    for item in items {
        let block_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match block_type {
            openai_blocks::TEXT => {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(json!({"text": text}));
                }
            }
            openai_blocks::IMAGE_URL => {
                let url = item
                    .get("image_url")
                    .and_then(|v| v.as_str().or_else(|| v.get("url").and_then(|u| u.as_str())))
                    .unwrap_or("");
                if let Some(rest) = url.strip_prefix("data:") {
                    if let Some((mime_part, data)) = rest.split_once(',') {
                        let mime_type = mime_part.split(';').next().unwrap_or(defaults::DEFAULT_IMAGE_MIME);
                        parts.push(json!({
                            "inlineData": {
                                "mime_type": mime_type,
                                "data": data
                            }
                        }));
                    }
                } else if url.starts_with("http://") || url.starts_with("https://") {
                    parts.push(json!({
                        "fileData": {
                            "fileUri": url,
                            "mimeType": remote_image_mime_type(url)
                        }
                    }));
                }
            }
            openai_blocks::INPUT_AUDIO => {
                if let Some(data) = item.get("input_audio").and_then(|a| a.get("data")).and_then(|v| v.as_str()) {
                    let format = item
                        .get("input_audio")
                        .and_then(|a| a.get("format"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("wav");
                    let mime_type = if format == "mp3" { "audio/mpeg".to_string() } else { format!("audio/{format}") };
                    parts.push(json!({
                        "inlineData": {
                            "mime_type": mime_type,
                            "data": data
                        }
                    }));
                }
            }
            openai_blocks::AUDIO_URL => {
                let url = item.get("audio_url").and_then(|a| a.get("url")).and_then(|v| v.as_str()).unwrap_or("");
                if let Some(rest) = url.strip_prefix("data:") {
                    if let Some((mime_part, data)) = rest.split_once(',') {
                        let mime_type = mime_part.split(';').next().unwrap_or("audio/wav");
                        parts.push(json!({
                            "inlineData": {
                                "mime_type": mime_type,
                                "data": data
                            }
                        }));
                    }
                }
            }
            openai_blocks::FILE => {
                let url = item
                    .get("file")
                    .and_then(|f| f.get("file_data"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(rest) = url.strip_prefix("data:") {
                    if let Some((mime_part, data)) = rest.split_once(',') {
                        let mime_type = mime_part.split(';').next().unwrap_or("application/pdf");
                        parts.push(json!({
                            "inlineData": {
                                "mime_type": mime_type,
                                "data": data
                            }
                        }));
                    }
                }
            }
            _ => {}
        }
    }

    parts
}

/// Extract text content from OpenAI content.
pub fn extract_text_content(content: &Json, separator: &str) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::TEXT))
            .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(separator);
    }
    String::new()
}

pub fn try_parse_json(s: &str) -> Option<Json> {
    serde_json::from_str(s).ok()
}

pub fn generate_request_id() -> String {
    format!("agent-{}", Uuid::new_v4())
}

pub fn generate_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{}{}", Uuid::new_v4(), now)
}

pub fn generate_project_id() -> String {
    let adjectives = ["useful", "bright", "swift", "calm", "bold"];
    let nouns = ["fuze", "wave", "spark", "flow", "core"];
    let mut rng = rand::rng();
    let adj = adjectives[rng.random_range(0..adjectives.len())];
    let noun = nouns[rng.random_range(0..nouns.len())];
    let uuid_str = Uuid::new_v4().to_string();
    format!("{adj}-{noun}-{}", &uuid_str[..5])
}

/// Sanitize function names for Gemini API: starts with [a-zA-Z_], then [a-zA-Z0-9_.:-], <= 64 chars.
pub fn sanitize_gemini_function_name(name: &str) -> String {
    if name.is_empty() {
        return "_unknown".to_string();
    }
    let mut sanitized = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ':' || c == '-' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }
    let first = sanitized.chars().next().unwrap_or('_');
    if !first.is_ascii_alphabetic() && first != '_' {
        sanitized.insert(0, '_');
    }
    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }
    sanitized
}

fn remove_unsupported_keywords(val: &mut Json) {
    match val {
        Json::Array(arr) => {
            for item in arr {
                remove_unsupported_keywords(item);
            }
        }
        Json::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if UNSUPPORTED_SET.contains(key.as_str()) || key.starts_with("x-") {
                    map.remove(&key);
                    continue;
                }
                if let Some(child) = map.get_mut(&key) {
                    remove_unsupported_keywords(child);
                }
            }
        }
        _ => {}
    }
}

fn convert_const_to_enum(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if let Some(c) = map.remove("const") {
            if !map.contains_key("enum") {
                map.insert("enum".to_string(), json!([c]));
            }
        }
        for child in map.values_mut() {
            convert_const_to_enum(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            convert_const_to_enum(child);
        }
    }
}

fn convert_enum_values_to_strings(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if let Some(arr) = map.get_mut("enum").and_then(|v| v.as_array_mut()) {
            for elem in arr.iter_mut() {
                if !elem.is_string() {
                    let s = match elem {
                        Json::Bool(b) => b.to_string(),
                        Json::Number(n) => n.to_string(),
                        other => other.to_string(),
                    };
                    *elem = Json::String(s);
                }
            }
            if !map.contains_key("type") {
                map.insert("type".to_string(), json!("string"));
            }
        }
        for child in map.values_mut() {
            convert_enum_values_to_strings(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            convert_enum_values_to_strings(child);
        }
    }
}

fn merge_all_of(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if let Some(all_of) = map.remove("allOf").and_then(|v| v.as_array().cloned()) {
            let mut merged_props = serde_json::Map::new();
            let mut merged_req: Vec<String> = Vec::new();
            for item in all_of {
                if let Some(props) = item.get("properties").and_then(|p| p.as_object()) {
                    for (k, v) in props {
                        merged_props.insert(k.clone(), v.clone());
                    }
                }
                if let Some(req) = item.get("required").and_then(|r| r.as_array()) {
                    for r in req {
                        if let Some(s) = r.as_str() {
                            if !merged_req.iter().any(|existing| existing == s) {
                                merged_req.push(s.to_string());
                            }
                        }
                    }
                }
            }
            if !merged_props.is_empty() {
                let existing = map.entry("properties".to_string()).or_insert_with(|| json!({}));
                if let Some(existing_map) = existing.as_object_mut() {
                    for (k, v) in merged_props {
                        existing_map.entry(k).or_insert(v);
                    }
                }
            }
            if !merged_req.is_empty() {
                let existing = map.entry("required".to_string()).or_insert_with(|| json!([]));
                if let Some(arr) = existing.as_array_mut() {
                    for r in merged_req {
                        if !arr.iter().any(|e| e.as_str() == Some(&r)) {
                            arr.push(Json::String(r));
                        }
                    }
                }
            }
        }
        for child in map.values_mut() {
            merge_all_of(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            merge_all_of(child);
        }
    }
}

fn select_best(items: &[Json]) -> usize {
    let mut best_idx = 0;
    let mut best_score = -1;
    for (i, item) in items.iter().enumerate() {
        let mut score = 0;
        let t = item.get("type").and_then(|v| v.as_str());
        if t == Some("object") || item.get("properties").is_some() {
            score = 3;
        } else if t == Some("array") || item.get("items").is_some() {
            score = 2;
        } else if t.is_some() && t != Some("null") {
            score = 1;
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    best_idx
}

fn flatten_any_of_one_of(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        for key in ["anyOf", "oneOf"] {
            if let Some(choices) = map.remove(key).and_then(|v| v.as_array().cloned()) {
                let non_null: Vec<_> = choices.into_iter().filter(|s| s.get("type").and_then(|v| v.as_str()) != Some("null")).collect();
                if !non_null.is_empty() {
                    let best = select_best(&non_null);
                    if let Some(selected_obj) = non_null[best].as_object() {
                        for (k, v) in selected_obj {
                            map.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
            }
        }
        for child in map.values_mut() {
            flatten_any_of_one_of(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            flatten_any_of_one_of(child);
        }
    }
}

fn flatten_type_arrays(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if let Some(arr) = map.get("type").and_then(|t| t.as_array()).cloned() {
            let non_null: Vec<&str> = arr.iter().filter_map(|t| t.as_str()).filter(|s| *s != "null").collect();
            let chosen = if non_null.is_empty() {
                "string"
            } else if non_null.len() == 1 {
                non_null[0]
            } else if map.contains_key("properties") && non_null.contains(&"object") {
                "object"
            } else if map.contains_key("items") && non_null.contains(&"array") {
                "array"
            } else if non_null.contains(&"string") {
                "string"
            } else if non_null.contains(&"number") {
                "number"
            } else if non_null.contains(&"integer") {
                "integer"
            } else if non_null.contains(&"boolean") {
                "boolean"
            } else {
                non_null[0]
            };
            map.insert("type".to_string(), json!(chosen));
        }
        for child in map.values_mut() {
            flatten_type_arrays(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            flatten_type_arrays(child);
        }
    }
}

fn ensure_object_type(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if map.contains_key("properties") && !map.contains_key("type") {
            map.insert("type".to_string(), json!("object"));
        }
        for child in map.values_mut() {
            ensure_object_type(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            ensure_object_type(child);
        }
    }
}

fn convert_prefix_items(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if let Some(prefix) = map.remove("prefixItems").and_then(|p| p.as_array().cloned()) {
            let non_null: Vec<_> = prefix.into_iter().filter(|s| s.get("type").and_then(|v| v.as_str()) != Some("null")).collect();
            if !map.contains_key("items") {
                if non_null.len() == 1 {
                    map.insert("items".to_string(), non_null.into_iter().next().unwrap());
                } else if non_null.len() > 1 {
                    map.insert("items".to_string(), json!({"anyOf": non_null}));
                }
            }
        }
        for child in map.values_mut() {
            convert_prefix_items(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            convert_prefix_items(child);
        }
    }
}

fn ensure_array_items(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if map.get("type").and_then(|v| v.as_str()) == Some("array") {
            let items = map.entry("items".to_string()).or_insert_with(|| json!({"type": "string"}));
            if !items.is_object() || items.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                *items = json!({"type": "string"});
            } else if items.get("type").is_none() {
                if items.get("properties").is_some() {
                    items["type"] = json!("object");
                } else if items.get("items").is_some() {
                    items["type"] = json!("array");
                } else {
                    items["type"] = json!("string");
                }
            }
        }
        for child in map.values_mut() {
            ensure_array_items(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            ensure_array_items(child);
        }
    }
}

fn enforce_type_constraints(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        for child in map.values_mut() {
            enforce_type_constraints(child);
        }
        let t = map.get("type").and_then(|v| v.as_str()).map(|s| s.to_string());
        if t.as_deref() == Some("array") {
            map.remove("properties");
            map.remove("required");
            let items = map.entry("items".to_string()).or_insert_with(|| json!({"type": "string"}));
            if !items.is_object() || items.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                *items = json!({"type": "string"});
            } else if items.get("type").is_none() {
                if items.get("properties").is_some() {
                    items["type"] = json!("object");
                } else if items.get("items").is_some() {
                    items["type"] = json!("array");
                } else {
                    items["type"] = json!("string");
                }
            }
        } else {
            map.remove("items");
        }
        if t.as_deref() != Some("object") {
            map.remove("properties");
            map.remove("required");
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            enforce_type_constraints(child);
        }
    }
}

fn cleanup_required(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if let Some(req_arr) = map.get("required").and_then(|r| r.as_array()).cloned() {
            if let Some(props) = map.get("properties").and_then(|p| p.as_object()) {
                let valid: Vec<Json> = req_arr
                    .into_iter()
                    .filter(|f| f.as_str().map(|k| props.contains_key(k)).unwrap_or(false))
                    .collect();
                if valid.is_empty() {
                    map.remove("required");
                } else {
                    map.insert("required".to_string(), Json::Array(valid));
                }
            } else {
                map.remove("required");
            }
        }
        for child in map.values_mut() {
            cleanup_required(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            cleanup_required(child);
        }
    }
}

fn add_placeholders(val: &mut Json) {
    if let Some(map) = val.as_object_mut() {
        if map.is_empty() {
            map.insert("type".to_string(), json!("object"));
            map.insert("properties".to_string(), json!({
                "reason": {
                    "type": "string",
                    "description": "Brief explanation of why you are calling this tool"
                }
            }));
            map.insert("required".to_string(), json!(["reason"]));
            return;
        }
        if map.get("type").and_then(|v| v.as_str()) == Some("object") {
            let no_props = map.get("properties").and_then(|p| p.as_object()).map(|p| p.is_empty()).unwrap_or(true);
            if no_props {
                map.insert("properties".to_string(), json!({
                    "reason": {
                        "type": "string",
                        "description": "Brief explanation of why you are calling this tool"
                    }
                }));
                map.insert("required".to_string(), json!(["reason"]));
            }
        }
        for child in map.values_mut() {
            add_placeholders(child);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for child in arr {
            add_placeholders(child);
        }
    }
}

/// Clean JSON Schema for Antigravity / Gemini compatibility.
pub fn clean_json_schema_for_antigravity(mut schema: Json) -> Json {
    if !schema.is_object() {
        return schema;
    }
    convert_const_to_enum(&mut schema);
    convert_enum_values_to_strings(&mut schema);
    merge_all_of(&mut schema);
    convert_prefix_items(&mut schema);
    flatten_any_of_one_of(&mut schema);
    flatten_type_arrays(&mut schema);
    ensure_object_type(&mut schema);
    ensure_array_items(&mut schema);
    remove_unsupported_keywords(&mut schema);
    cleanup_required(&mut schema);
    add_placeholders(&mut schema);
    enforce_type_constraints(&mut schema);
    schema
}

/// Recursively rename reserved `$ref` keys in tool response payload (`$ref` → `_ref`).
pub fn sanitize_function_response_payload(mut val: Json) -> Json {
    match &mut val {
        Json::Array(arr) => {
            for item in arr.iter_mut() {
                *item = sanitize_function_response_payload(item.clone());
            }
        }
        Json::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map.iter() {
                let key = if k == "$ref" { "_ref" } else { k.as_str() };
                out.insert(key.to_string(), sanitize_function_response_payload(v.clone()));
            }
            return Json::Object(out);
        }
        _ => {}
    }
    val
}

fn move_function_responses_last(parts: Vec<Json>) -> Vec<Json> {
    let mut responses = Vec::new();
    let mut rest = Vec::new();
    for p in parts {
        if p.get("functionResponse").is_some() {
            responses.push(p);
        } else {
            rest.push(p);
        }
    }
    if responses.is_empty() {
        rest
    } else {
        rest.extend(responses);
        rest
    }
}

/// Merge adjacent same-role messages, strip empty parts, ensure initial and terminal user turns.
pub fn normalize_gemini_contents(contents: &[Json], model: Option<&str>) -> Vec<Json> {
    let fr_last = model.map_or(false, |m| m.to_lowercase().contains("gemini-4"));
    let mut out: Vec<Json> = Vec::new();

    for c in contents {
        let role = match c.get("role").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => continue,
        };
        let Some(parts) = c.get("parts").and_then(|p| p.as_array()) else { continue };
        let mut clean_parts: Vec<Json> = Vec::new();
        for p in parts {
            if !p.is_object() || p.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                continue;
            }
            if let Some(resp) = p.get("functionResponse").and_then(|fr| fr.get("response")) {
                let mut p = p.clone();
                p["functionResponse"]["response"] = sanitize_function_response_payload(resp.clone());
                clean_parts.push(p);
            } else {
                clean_parts.push(p.clone());
            }
        }
        if clean_parts.is_empty() {
            continue;
        }

        let same_as_last = out.last().and_then(|l| l.get("role").and_then(|v| v.as_str())) == Some(role);
        if same_as_last {
            let last = out.last_mut().unwrap();
            let mut existing = last.get("parts").and_then(|p| p.as_array()).cloned().unwrap_or_default();
            existing.extend(clean_parts);
            if fr_last {
                existing = move_function_responses_last(existing);
            }
            last["parts"] = Json::Array(existing);
        } else {
            out.push(json!({
                "role": role,
                "parts": clean_parts
            }));
        }
    }

    if !out.is_empty() && out[0].get("role").and_then(|v| v.as_str()) != Some(gemini_roles::USER) {
        out.insert(0, json!({"role": gemini_roles::USER, "parts": [{"text": "..."}]}));
    }

    if !out.is_empty() && out.last().and_then(|l| l.get("role").and_then(|v| v.as_str())) == Some(gemini_roles::MODEL) {
        let fn_calls: Vec<Json> = out.last()
            .and_then(|l| l.get("parts").and_then(|p| p.as_array()))
            .map(|parts| parts.iter().filter(|p| p.get("functionCall").is_some()).cloned().collect())
            .unwrap_or_default();
        if !fn_calls.is_empty() {
            let responses: Vec<Json> = fn_calls
                .into_iter()
                .map(|p| {
                    let call = p.get("functionCall").cloned().unwrap_or_else(|| json!({}));
                    let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let mut fr = json!({
                        "name": name,
                        "response": {"result": "Continue."}
                    });
                    if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                        fr["id"] = json!(id);
                    }
                    json!({"functionResponse": fr})
                })
                .collect();
            out.push(json!({"role": gemini_roles::USER, "parts": responses}));
        } else {
            out.push(json!({"role": gemini_roles::USER, "parts": [{"text": "Continue."}]}));
        }
    }

    out
}

/// Build Gemini request body from OpenAI-format messages.
pub fn build_gemini_request(body: &Json) -> Json {
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("gemini-2.5-flash");
    let mut result = json!({
        "model": model,
        "contents": [],
        "generationConfig": {},
        "safetySettings": default_safety_settings()
    });

    if let Some(temp) = body.get("temperature").and_then(|v| v.as_f64()) {
        result["generationConfig"]["temperature"] = json!(temp);
    }
    if let Some(top_p) = body.get("top_p").and_then(|v| v.as_f64()) {
        result["generationConfig"]["topP"] = json!(top_p);
    }
    if let Some(max_tokens) = body.get("max_tokens").and_then(|v| v.as_i64()) {
        result["generationConfig"]["maxOutputTokens"] = json!(max_tokens);
    }

    let mut contents = Vec::new();
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let g_role = if role == roles::ASSISTANT { gemini_roles::MODEL } else { gemini_roles::USER };
            let parts = convert_openai_content_to_parts(&msg.get("content").cloned().unwrap_or(Json::Null));
            if !parts.is_empty() {
                contents.push(json!({"role": g_role, "parts": parts}));
            }
        }
    }
    result["contents"] = Json::Array(normalize_gemini_contents(&contents, Some(model)));
    result
}

/// Parse Gemini response into OpenAI completion format.
pub fn parse_gemini_response(response: &Json) -> Json {
    let candidate = response
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    let text: String = candidate
        .and_then(|c| c.get("content"))
        .and_then(|cnt| cnt.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let finish_reason = candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(|v| v.as_str())
        .map(|r| match r {
            gemini_finish::STOP => openai_finish::STOP,
            gemini_finish::MAX_TOKENS => openai_finish::LENGTH,
            gemini_finish::SAFETY | gemini_finish::RECITATION | gemini_finish::BLOCKLIST => openai_finish::CONTENT_FILTER,
            _ => openai_finish::STOP,
        })
        .unwrap_or(openai_finish::STOP);

    json!({
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": roles::ASSISTANT,
                    "content": text
                },
                "finish_reason": finish_reason
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_gemini_function_name() {
        assert_eq!(sanitize_gemini_function_name("get-weather.now"), "get-weather.now");
        assert_eq!(sanitize_gemini_function_name("123bad"), "_123bad");
        assert_eq!(sanitize_gemini_function_name("tool/slash"), "tool_slash");
    }

    #[test]
    fn test_clean_schema_removes_unsupported() {
        let input = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1, "description": "hi"},
                "extra": {"type": "string", "$comment": "internal", "x-foo": true}
            },
            "required": ["name", "missing_field"],
            "additionalProperties": false
        });
        let cleaned = clean_json_schema_for_antigravity(input);
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(cleaned["properties"]["name"].get("minLength").is_none());
        assert!(cleaned["properties"]["extra"].get("$comment").is_none());
        assert!(cleaned["properties"]["extra"].get("x-foo").is_none());
        // Invalid required field stripped
        assert_eq!(cleaned["required"], json!(["name"]));
    }

    #[test]
    fn test_clean_schema_empty_object_adds_reason_placeholder() {
        let input = json!({"type": "object", "properties": {}});
        let cleaned = clean_json_schema_for_antigravity(input);
        assert!(cleaned["properties"].get("reason").is_some());
        assert_eq!(cleaned["required"], json!(["reason"]));
    }

    #[test]
    fn test_sanitize_function_response_payload_renames_ref() {
        let input = json!({
            "status": "ok",
            "$ref": "#/$defs/MyType",
            "nested": [{"$ref": "other"}]
        });
        let sanitized = sanitize_function_response_payload(input);
        assert!(sanitized.get("$ref").is_none());
        assert_eq!(sanitized["_ref"], "#/$defs/MyType");
        assert_eq!(sanitized["nested"][0]["_ref"], "other");
    }

    #[test]
    fn test_normalize_gemini_contents_merges_roles_and_ensures_user_ends() {
        let input = vec![
            json!({"role": "user", "parts": [{"text": "a"}]}),
            json!({"role": "user", "parts": [{"text": "b"}]}),
            json!({"role": "model", "parts": [{"text": "c"}]}),
        ];
        let normalized = normalize_gemini_contents(&input, None);
        // Merged user turns into 1, model into 1, plus automatic terminal user turn
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0]["role"], "user");
        assert_eq!(normalized[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(normalized[1]["role"], "model");
        assert_eq!(normalized[2]["role"], "user");
        assert_eq!(normalized[2]["parts"][0]["text"], "Continue.");
    }
}
