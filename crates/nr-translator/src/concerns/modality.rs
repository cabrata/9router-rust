//! Strip multimodal content blocks a model cannot read. Port of open-sse/translator/concerns/modality.js.
//!
//! Driven by getCapabilitiesForModel: vision/audioInput/pdf. Replaces removed
//! media with a short text placeholder so messages never become empty.

use std::collections::BTreeSet;

use nr_core::Json;
use serde_json::json;

/// Placeholder text inserted where a media block was removed.
/// Current turn: explain the active model can't read what the user just sent.
const PLACEHOLDER_CURRENT: &[( &str, &str)] = &[
    ("vision", "[image omitted: model has no vision support]"),
    ("audioInput", "[audio omitted: model has no audio support]"),
    ("pdf", "[file omitted: model has no document support]"),
];

/// Earlier turns: neutral (a combo may route to a different model each turn).
const PLACEHOLDER_PREV: &[(&str, &str)] = &[
    ("vision", "[Previous image omitted from context.]"),
    ("audioInput", "[Previous audio omitted from context.]"),
    ("pdf", "[Previous file omitted from context.]"),
];

fn placeholder(cap: &str, is_last: bool) -> &'static str {
    let table = if is_last { PLACEHOLDER_CURRENT } else { PLACEHOLDER_PREV };
    table.iter().find(|(c, _)| *c == cap).map(|(_, t)| *t).unwrap_or("[media omitted]")
}

/// Map gemini inlineData/fileData mime prefix → capability it requires.
fn cap_for_mime(mime: Option<&Json>) -> Option<&'static str> {
    let mime = mime.and_then(|v| v.as_str())?;
    if mime.starts_with("image/") {
        Some("vision")
    } else if mime.starts_with("audio/") {
        Some("audioInput")
    } else if mime == "application/pdf" {
        Some("pdf")
    } else {
        None
    }
}

/// OpenAI chat content block → required capability (None = plain text/other, keep).
fn cap_for_openai_block(block: &Json) -> Option<&'static str> {
    match block.get("type").and_then(|v| v.as_str()) {
        Some("image_url") | Some("image") => Some("vision"),
        Some("input_audio") | Some("audio_url") => Some("audioInput"),
        Some("file") => Some("pdf"),
        _ => None,
    }
}

/// Claude content block → required capability.
fn cap_for_claude_block(block: &Json) -> Option<&'static str> {
    match block.get("type").and_then(|v| v.as_str()) {
        Some("image") => Some("vision"),
        Some("document") => Some("pdf"),
        _ => None,
    }
}

fn missing(caps: &Json, cap: &str) -> bool {
    caps.get(cap).and_then(|v| v.as_bool()) == Some(false)
}

/// Filter an array of content blocks; drop unsupported, inject one placeholder per kind.
fn filter_blocks(blocks: &[Json], cap_of: fn(&Json) -> Option<&'static str>, caps: &Json, is_last: bool) -> Vec<Json> {
    let mut out = Vec::new();
    let mut removed: BTreeSet<&str> = BTreeSet::new();
    for block in blocks {
        match cap_of(block) {
            Some(cap) if missing(caps, cap) => {
                removed.insert(cap);
            }
            _ => out.push(block.clone()),
        }
    }
    for cap in removed {
        out.push(json!({"type": "text", "text": placeholder(cap, is_last)}));
    }
    out
}

fn strip_openai_messages(body: &mut Json, caps: &Json) {
    let last_idx = body.get("messages").and_then(|m| m.as_array()).map(|m| m.len()).unwrap_or(0);
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for (i, msg) in messages.iter_mut().enumerate() {
        let is_last = i == last_idx.saturating_sub(1);
        if missing(caps, "vision") {
            if msg.get("images").map(|v| v.is_array()).unwrap_or(false) {
                if let Some(obj) = msg.as_object_mut() {
                    obj.remove("images");
                }
            }
            for key in ["experimental_attachments", "attachments"] {
                if let Some(atts) = msg.get_mut(key).and_then(|a| a.as_array_mut()) {
                    atts.retain(|a| {
                        let content_type = a.get("contentType").and_then(|v| v.as_str()).unwrap_or("");
                        let url = a.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        !(content_type.starts_with("image/") || url.starts_with("data:image/"))
                    });
                }
            }
        }
        if !msg.get("content").map(|c| c.is_array()).unwrap_or(false) {
            continue;
        }
        let blocks = msg["content"].as_array().cloned().unwrap_or_default();
        msg["content"] = Json::Array(filter_blocks(&blocks, cap_for_openai_block, caps, is_last));
    }
}

fn strip_claude(body: &mut Json, caps: &Json) {
    let last_idx = body.get("messages").and_then(|m| m.as_array()).map(|m| m.len()).unwrap_or(0);
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for (i, msg) in messages.iter_mut().enumerate() {
        if !msg.get("content").map(|c| c.is_array()).unwrap_or(false) {
            continue;
        }
        let blocks = msg["content"].as_array().cloned().unwrap_or_default();
        msg["content"] = Json::Array(filter_blocks(&blocks, cap_for_claude_block, caps, i == last_idx.saturating_sub(1)));
    }
}

fn strip_responses(body: &mut Json, caps: &Json) {
    let last_idx = body.get("input").and_then(|m| m.as_array()).map(|m| m.len()).unwrap_or(0);
    let Some(items) = body.get_mut("input").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for (i, item) in items.iter_mut().enumerate() {
        if !item.get("content").map(|c| c.is_array()).unwrap_or(false) {
            continue;
        }
        let is_last = i == last_idx.saturating_sub(1);
        let mut out = Vec::new();
        let mut removed: BTreeSet<&str> = BTreeSet::new();
        for block in item["content"].as_array().cloned().unwrap_or_default() {
            let cap = match block.get("type").and_then(|v| v.as_str()) {
                Some("input_image") => Some("vision"),
                Some("input_file") => Some("pdf"),
                _ => None,
            };
            match cap {
                Some(cap) if missing(caps, cap) => {
                    removed.insert(cap);
                }
                _ => out.push(block),
            }
        }
        for cap in removed {
            out.push(json!({"type": "input_text", "text": placeholder(cap, is_last)}));
        }
        item["content"] = Json::Array(out);
    }
}

fn strip_gemini_parts(body: &mut Json, path: Option<&str>, caps: &Json) {
    let container = match path {
        Some(path) => body.get_mut(path),
        None => Some(body),
    };
    let Some(contents) = container.and_then(|c| c.get_mut("contents")).and_then(|c| c.as_array_mut()) else {
        return;
    };
    let last_idx = contents.len();
    for (i, c) in contents.iter_mut().enumerate() {
        if !c.get("parts").map(|p| p.is_array()).unwrap_or(false) {
            continue;
        }
        let is_last = i == last_idx.saturating_sub(1);
        let mut out = Vec::new();
        let mut removed: BTreeSet<&str> = BTreeSet::new();
        for part in c["parts"].as_array().cloned().unwrap_or_default() {
            let mime = part.get("inlineData").and_then(|d| d.get("mimeType")).or_else(|| {
                part.get("fileData").and_then(|d| d.get("mimeType"))
            });
            match cap_for_mime(mime) {
                Some(cap) if missing(caps, cap) => {
                    removed.insert(cap);
                }
                _ => out.push(part),
            }
        }
        for cap in removed {
            out.push(json!({"text": placeholder(cap, is_last)}));
        }
        c["parts"] = Json::Array(out);
    }
}

/// Remove media blocks the model can't read, in-place on the source-format body.
/// Returns true if anything was strip-eligible (cap false for some modality).
pub fn strip_unsupported_modalities(body: &mut Json, source_format: &str, caps: &Json) -> bool {
    if !body.is_object() {
        return false;
    }
    // Fast exit: model supports everything we'd strip.
    if !missing(caps, "vision") && !missing(caps, "audioInput") && !missing(caps, "pdf") {
        return false;
    }
    match source_format {
        "openai" | "ollama" | "kiro" | "cursor" | "commandcode" => strip_openai_messages(body, caps),
        "claude" => strip_claude(body, caps),
        "openai-responses" | "openai-response" | "codex" => strip_responses(body, caps),
        "gemini" | "gemini-cli" | "vertex" => strip_gemini_parts(body, None, caps),
        "antigravity" => strip_gemini_parts(body, Some("request"), caps),
        _ => strip_openai_messages(body, caps),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(v: &Json) -> String {
        v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()
    }

    #[test]
    fn test_strip_unsupported_modalities_noop() {
        let mut body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let caps = json!({"vision": true, "audioInput": true, "pdf": true});
        assert!(!strip_unsupported_modalities(&mut body, "openai", &caps));
    }

    #[test]
    fn test_strip_openai_image_no_vision() {
        let mut body = json!({"messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "http://x/y.png"}}
            ]
        }]});
        let caps = json!({"vision": false});
        assert!(strip_unsupported_modalities(&mut body, "openai", &caps));
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "look");
        assert!(text_of(&content[1]).contains("image omitted"));
    }

    #[test]
    fn test_strip_claude_document_no_pdf() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "document", "source": {"type": "base64"}}
        ]}]});
        let caps = json!({"pdf": false});
        strip_unsupported_modalities(&mut body, "claude", &caps);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert!(text_of(&content[0]).contains("file omitted"));
    }

    #[test]
    fn test_strip_gemini_inline_audio() {
        let mut body = json!({"contents": [{"role": "user", "parts": [
            {"inlineData": {"mimeType": "audio/mp3", "data": "x"}},
            {"text": "hi"}
        ]}]});
        strip_unsupported_modalities(&mut body, "gemini", &json!({"audioInput": false}));
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert!(text_of(&parts[1]).contains("audio omitted"));
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_strip_antigravity_envelope() {
        let mut body = json!({"request": {"contents": [{"role": "user", "parts": [
            {"fileData": {"mimeType": "image/png", "fileUri": "x"}}
        ]}]}});
        strip_unsupported_modalities(&mut body, "antigravity", &json!({"vision": false}));
        let parts = body["request"]["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert!(text_of(&parts[0]).contains("image omitted"));
    }

    #[test]
    fn test_placeholder_varies_by_turn() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "a"}}]},
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "b"}}]}
        ]});
        strip_unsupported_modalities(&mut body, "openai", &json!({"vision": false}));
        assert_eq!(body["messages"][0]["content"][0]["text"], "[Previous image omitted from context.]");
        assert_eq!(body["messages"][1]["content"][0]["text"], "[image omitted: model has no vision support]");
    }
}
