//! Port of open-sse/utils/cursorProtobuf.js
//! Protobuf encoding/decoding for Cursor API

use serde_json::{json, Value, Value as Json};
use std::collections::BTreeMap;

/// Wire type constants
pub const WIRE_TYPE_VARINT: u8 = 0;
pub const WIRE_TYPE_FIXED64: u8 = 1;
pub const WIRE_TYPE_LEN: u8 = 2;
pub const WIRE_TYPE_FIXED32: u8 = 5;

/// Protobuf field constants
pub mod field {
    // StreamUnifiedChatRequestWithTools
    pub const REQUEST: u32 = 1;
    pub const RESPONSE: u32 = 2;

    // StreamUnifiedChatRequest
    pub const MESSAGES: u32 = 1;
    pub const UNKNOWN_2: u32 = 2;
    pub const INSTRUCTION: u32 = 3;
    pub const UNKNOWN_4: u32 = 4;
    pub const MODEL: u32 = 5;
    pub const WEB_TOOL: u32 = 8;
    pub const UNKNOWN_13: u32 = 13;
    pub const CURSOR_SETTING: u32 = 15;
    pub const UNKNOWN_19: u32 = 19;
    pub const CONVERSATION_ID: u32 = 23;
    pub const METADATA: u32 = 26;
    pub const IS_AGENTIC: u32 = 27;
    pub const SUPPORTED_TOOLS: u32 = 29;
    pub const MESSAGE_IDS: u32 = 30;
    pub const MCP_TOOLS: u32 = 34;
    pub const LARGE_CONTEXT: u32 = 35;
    pub const UNKNOWN_38: u32 = 38;
    pub const UNIFIED_MODE: u32 = 46;
    pub const UNKNOWN_47: u32 = 47;
    pub const SHOULD_DISABLE_TOOLS: u32 = 48;
    pub const THINKING_LEVEL: u32 = 49;
    pub const UNKNOWN_51: u32 = 51;
    pub const UNKNOWN_53: u32 = 53;
    pub const UNIFIED_MODE_NAME: u32 = 54;

    // ConversationMessage
    pub const MSG_CONTENT: u32 = 1;
    pub const MSG_ROLE: u32 = 2;
    pub const MSG_ID: u32 = 13;
    pub const MSG_TOOL_RESULTS: u32 = 18;
    pub const MSG_IS_AGENTIC: u32 = 29;
    pub const MSG_SERVER_BUBBLE_ID: u32 = 32;
    pub const MSG_UNIFIED_MODE: u32 = 47;
    pub const MSG_SUPPORTED_TOOLS: u32 = 51;

    // Response
    pub const TOOL_CALL: u32 = 1;
    pub const RESPONSE_TEXT: u32 = 1;
    pub const THINKING: u32 = 25;
    pub const THINKING_TEXT: u32 = 1;
    pub const TOOL_ID: u32 = 3;
    pub const TOOL_NAME: u32 = 9;
    pub const TOOL_RAW_ARGS: u32 = 10;
    pub const TOOL_IS_LAST: u32 = 11;
    pub const TOOL_MCP_PARAMS: u32 = 27;
    pub const MCP_TOOLS_LIST: u32 = 1;
    pub const MCP_NESTED_NAME: u32 = 1;
    pub const MCP_NESTED_PARAMS: u32 = 3;
}

/// Encode a varint
pub fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut result = Vec::new();
    while value >= 0x80 {
        result.push(((value & 0x7F) | 0x80) as u8);
        value >>= 7;
    }
    result.push((value & 0x7F) as u8);
    result
}

/// Encode a protobuf field
pub fn encode_field(field_num: u32, wire_type: u8, value: &[u8]) -> Vec<u8> {
    let tag = (field_num << 3) | (wire_type as u32);
    let mut result = encode_varint(tag as u64);
    match wire_type {
        WIRE_TYPE_VARINT => result.extend(encode_varint(u64::from_le_bytes({
            let mut arr = [0u8; 8];
            arr[..value.len().min(8)].copy_from_slice(&value[..value.len().min(8)]);
            arr
        }))),
        WIRE_TYPE_LEN => {
            result.extend(encode_varint(value.len() as u64));
            result.extend_from_slice(value);
        }
        WIRE_TYPE_FIXED64 => result.extend_from_slice(value),
        WIRE_TYPE_FIXED32 => result.extend_from_slice(value),
        _ => {}
    }
    result
}

/// Decode a varint, returning (value, new_offset)
pub fn decode_varint(data: &[u8], mut offset: usize) -> (u64, usize) {
    let mut result = 0u64;
    let mut shift = 0;
    while offset < data.len() {
        let b = data[offset] as u64;
        result |= (b & 0x7F) << shift;
        offset += 1;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
    }
    (result, offset)
}

/// Decoded protobuf field
#[derive(Clone, Debug)]
pub struct DecodedField {
    pub wire_type: u8,
    pub value: Vec<u8>,
}

/// Decode a protobuf message into a map of field number to values
pub fn decode_message(data: &[u8]) -> BTreeMap<u32, Vec<DecodedField>> {
    let mut fields = BTreeMap::new();
    let mut offset = 0;

    while offset < data.len() {
        let (tag, new_offset) = decode_varint(data, offset);
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u8;
        offset = new_offset;

        let (value, field_offset) = match wire_type {
            WIRE_TYPE_VARINT => {
                let (val, off) = decode_varint(data, offset);
                (val.to_le_bytes().to_vec(), off)
            }
            WIRE_TYPE_LEN => {
                let (len, off) = decode_varint(data, offset);
                let len = len as usize;
                if off + len <= data.len() {
                    (data[off..off + len].to_vec(), off + len)
                } else {
                    break;
                }
            }
            WIRE_TYPE_FIXED64 => {
                if offset + 8 <= data.len() {
                    (data[offset..offset + 8].to_vec(), offset + 8)
                } else {
                    break;
                }
            }
            WIRE_TYPE_FIXED32 => {
                if offset + 4 <= data.len() {
                    (data[offset..offset + 4].to_vec(), offset + 4)
                } else {
                    break;
                }
            }
            _ => break,
        };

        offset = field_offset;
        fields.entry(field_num)
            .or_insert_with(Vec::new)
            .push(DecodedField { wire_type, value });
    }

    fields
}

/// Wrap payload in Connect RPC frame (flags + 4-byte length + payload)
pub fn wrap_connect_rpc_frame(payload: &[u8], compress: bool) -> Vec<u8> {
    let mut result = Vec::new();
    let flags = if compress { 0x01u8 } else { 0x00u8 };
    result.push(flags);

    let len = payload.len() as u32;
    result.extend_from_slice(&len.to_be_bytes());
    result.extend_from_slice(payload);

    result
}

/// Extract text from response payload
pub struct ResponseExtract {
    pub text: Option<String>,
    pub thinking: Option<String>,
    pub tool_call: Option<ToolCall>,
    pub error: Option<String>,
}

/// Tool call extracted from response
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub is_last: bool,
}

/// Extract text and thinking from response
pub fn extract_text_from_response(payload: &[u8]) -> ResponseExtract {
    let fields = decode_message(payload);

    // Try tool_call first (field 1)
    if let Some(tool_fields) = fields.get(&field::TOOL_CALL) {
        if let Some(field) = tool_fields.first() {
            if let Some(tool_call) = extract_tool_call(&field.value) {
                return ResponseExtract {
                    text: None,
                    thinking: None,
                    tool_call: Some(tool_call),
                    error: None,
                };
            }
        }
    }

    // Try response (field 2)
    let mut text = None;
    let mut thinking = None;
    if let Some(resp_fields) = fields.get(&field::RESPONSE) {
        if let Some(field) = resp_fields.first() {
            let nested = decode_message(&field.value);
            if let Some(text_fields) = nested.get(&field::RESPONSE_TEXT) {
                if let Some(text_field) = text_fields.first() {
                    text = String::from_utf8(text_field.value.clone()).ok();
                }
            }
            if let Some(thinking_fields) = nested.get(&field::THINKING) {
                if let Some(thinking_field) = thinking_fields.first() {
                    let thinking_msg = decode_message(&thinking_field.value);
                    if let Some(thinking_text_fields) = thinking_msg.get(&field::THINKING_TEXT) {
                        if let Some(thinking_text_field) = thinking_text_fields.first() {
                            thinking = String::from_utf8(thinking_text_field.value.clone()).ok();
                        }
                    }
                }
            }
        }
    }

    ResponseExtract {
        text,
        thinking,
        tool_call: None,
        error: None,
    }
}

fn extract_tool_call(data: &[u8]) -> Option<ToolCall> {
    let fields = decode_message(data);
    let mut tool_call_id = String::new();
    let mut tool_name = String::new();
    let mut raw_args = String::from("{}");
    let mut is_last = false;

    if let Some(id_fields) = fields.get(&field::TOOL_ID) {
        if let Some(id_field) = id_fields.first() {
            if let Ok(s) = String::from_utf8(id_field.value.clone()) {
                tool_call_id = s.lines().next().unwrap_or("").to_string();
            }
        }
    }

    if let Some(name_fields) = fields.get(&field::TOOL_NAME) {
        if let Some(name_field) = name_fields.first() {
            tool_name = String::from_utf8(name_field.value.clone()).unwrap_or_default();
        }
    }

    if let Some(is_last_fields) = fields.get(&field::TOOL_IS_LAST) {
        if let Some(is_last_field) = is_last_fields.first() {
            is_last = !is_last_field.value.is_empty() && is_last_field.value[0] != 0;
        }
    }

    if let Some(args_fields) = fields.get(&field::TOOL_MCP_PARAMS) {
        if let Some(args_field) = args_fields.first() {
            let mcp_fields = decode_message(&args_field.value);
            if let Some(tools_list) = mcp_fields.get(&field::MCP_TOOLS_LIST) {
                if let Some(tool_field) = tools_list.first() {
                    let tool_msg = decode_message(&tool_field.value);
                    if let Some(nested_name) = tool_msg.get(&field::MCP_NESTED_NAME) {
                        if let Some(name_field) = nested_name.first() {
                            tool_name = String::from_utf8(name_field.value.clone()).unwrap_or_default();
                        }
                    }
                    if let Some(params) = tool_msg.get(&field::MCP_NESTED_PARAMS) {
                        if let Some(params_field) = params.first() {
                            raw_args = String::from_utf8(params_field.value.clone()).unwrap_or_else(|_| "{}".to_string());
                        }
                    }
                }
            }
        }
    }

    if !tool_call_id.is_empty() && !tool_name.is_empty() {
        Some(ToolCall {
            id: tool_call_id,
            name: tool_name,
            arguments: raw_args,
            is_last,
        })
    } else {
        None
    }
}

// ==================== REQUEST ENCODING (port of encodeRequest) ====================

const ROLE_USER: u64 = 1;
const ROLE_ASSISTANT: u64 = 2;
const UNIFIED_MODE_CHAT: u64 = 1;
const UNIFIED_MODE_AGENT: u64 = 2;
const THINKING_LEVEL_UNSPECIFIED: u64 = 0;
const THINKING_LEVEL_MEDIUM: u64 = 1;
const THINKING_LEVEL_HIGH: u64 = 2;
/// `CLIENT_SIDE_TOOL_V2.MCP` — the only client tool Cursor accepts.
const CLIENT_SIDE_TOOL_V2_MCP: u64 = 19;

/// Protobuf fields below the `field` module (CursorSetting, Metadata, …).
mod field2 {
    pub const SETTING_PATH: u32 = 1;
    pub const SETTING_UNKNOWN_3: u32 = 3;
    pub const SETTING_UNKNOWN_6: u32 = 6;
    pub const SETTING_UNKNOWN_8: u32 = 8;
    pub const SETTING_UNKNOWN_9: u32 = 9;
    pub const SETTING6_FIELD_1: u32 = 1;
    pub const SETTING6_FIELD_2: u32 = 2;

    pub const META_PLATFORM: u32 = 1;
    pub const META_ARCH: u32 = 2;
    pub const META_VERSION: u32 = 3;
    pub const META_CWD: u32 = 4;
    pub const META_TIMESTAMP: u32 = 5;

    pub const MSGID_ID: u32 = 1;
    pub const MSGID_ROLE: u32 = 3;

    pub const MCP_TOOL_NAME: u32 = 1;
    pub const MCP_TOOL_DESC: u32 = 2;
    pub const MCP_TOOL_PARAMS: u32 = 3;
    pub const MCP_TOOL_SERVER: u32 = 4;

    pub const INSTRUCTION_TEXT: u32 = 1;
    pub const MODEL_NAME: u32 = 1;
    pub const MODEL_EMPTY: u32 = 4;

    pub const TOOL_RESULT_CALL_ID: u32 = 1;
    pub const TOOL_RESULT_NAME: u32 = 2;
    pub const TOOL_RESULT_INDEX: u32 = 3;
    pub const TOOL_RESULT_RAW_ARGS: u32 = 5;
    pub const TOOL_RESULT_RESULT: u32 = 8;
    pub const TOOL_RESULT_TOOL_CALL: u32 = 11;
    pub const TOOL_RESULT_MODEL_CALL_ID: u32 = 12;

    pub const CV2R_TOOL: u32 = 1;
    pub const CV2R_MCP_RESULT: u32 = 28;
    pub const CV2R_CALL_ID: u32 = 35;
    pub const CV2R_MODEL_CALL_ID: u32 = 48;
    pub const CV2R_TOOL_INDEX: u32 = 49;

    pub const MCPR_SELECTED_TOOL: u32 = 1;
    pub const MCPR_RESULT: u32 = 2;

    pub const CV2C_TOOL: u32 = 1;
    pub const CV2C_MCP_PARAMS: u32 = 27;
    pub const CV2C_CALL_ID: u32 = 3;
    pub const CV2C_NAME: u32 = 9;
    pub const CV2C_RAW_ARGS: u32 = 10;
    pub const CV2C_TOOL_INDEX: u32 = 48;
    pub const CV2C_MODEL_CALL_ID: u32 = 49;

    pub const MSG_TOOL_RESULT_CALL_ID: u32 = 1;
    pub const MSG_TOOL_RESULT_NAME: u32 = 2;
    pub const MSG_TOOL_RESULT_INDEX: u32 = 3;
    pub const MSG_TOOL_RESULT_RAW_ARGS: u32 = 5;
    pub const MSG_TOOL_RESULT_RESULT: u32 = 8;
    pub const MSG_TOOL_RESULT_TOOL_CALL: u32 = 11;
    pub const MSG_TOOL_RESULT_MODEL_CALL_ID: u32 = 12;
}

/// Cursor returns tool-call IDs as `{callId}\nmc_{modelCallId}`.
fn parse_tool_id(id: &str) -> (String, Option<String>) {
    const DELIMITER: &str = "\nmc_";
    match id.find(DELIMITER) {
        Some(idx) => (
            id[..idx].to_string(),
            Some(id[idx + DELIMITER.len()..].to_string()),
        ),
        None => (id.to_string(), None),
    }
}

/// "toolName" → "mcp_custom_toolName"; "mcp__server__tool" → "mcp_server_tool".
fn format_tool_name(name: &str) -> String {
    let base = if name.is_empty() { "tool" } else { name };
    if let Some(rest) = base.strip_prefix("mcp__") {
        if let Some(idx) = rest.find("__") {
            let server = if idx == 0 { "custom" } else { &rest[..idx] };
            let tool_name = &rest[idx + 2..];
            return format!("mcp_{}_{}", server, if tool_name.is_empty() { "tool" } else { tool_name });
        }
        return format!("mcp_custom_{}", if rest.is_empty() { "tool" } else { rest });
    }
    if base.starts_with("mcp_") {
        return base.to_string();
    }
    format!("mcp_custom_{base}")
}

/// Inverse of [`format_tool_name`] for encoding results.
fn parse_tool_name(formatted: &str) -> (String, String) {
    if !formatted.starts_with("mcp_") {
        return ("custom".to_string(), if formatted.is_empty() { "tool".to_string() } else { formatted.to_string() });
    }
    let tail = &formatted["mcp_".len()..];
    match tail.find('_') {
        Some(idx) => (
            if idx == 0 { "custom".to_string() } else { tail[..idx].to_string() },
            if tail[idx + 1..].is_empty() { "tool".to_string() } else { tail[idx + 1..].to_string() },
        ),
        None => (
            "custom".to_string(),
            if tail.is_empty() { "tool".to_string() } else { tail.to_string() },
        ),
    }
}

fn encode_varint_field(field_num: u32, value: u64) -> Vec<u8> {
    let tag = (field_num << 3) | WIRE_TYPE_VARINT as u32;
    let mut out = encode_varint(tag as u64);
    out.extend(encode_varint(value));
    out
}

/// JS `encodeField(fieldNum, WIRE_TYPE.LEN, string)`.
fn encode_len_field(field_num: u32, value: &str) -> Vec<u8> {
    let tag = (field_num << 3) | WIRE_TYPE_LEN as u32;
    let mut out = encode_varint(tag as u64);
    out.extend(encode_varint(value.len() as u64));
    out.extend_from_slice(value.as_bytes());
    out
}

fn concat_many(parts: Vec<Vec<u8>>) -> Vec<u8> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(total);
    for p in parts {
        out.extend_from_slice(&p);
    }
    out
}

fn encode_mcp_tool(tool: &Value) -> Vec<u8> {
    let function = tool.get("function").cloned().unwrap_or(Json::Null);
    let name = function
        .get("name")
        .and_then(Json::as_str)
        .or_else(|| tool.get("name").and_then(Json::as_str))
        .unwrap_or("")
        .to_string();
    let desc = function
        .get("description")
        .and_then(Json::as_str)
        .or_else(|| tool.get("description").and_then(Json::as_str))
        .unwrap_or("")
        .to_string();
    let input_schema = function
        .get("parameters")
        .or_else(|| tool.get("input_schema"))
        .cloned()
        .unwrap_or(Json::Object(Default::default()));

    let mut parts = Vec::new();
    if !name.is_empty() {
        parts.push(encode_len_field(field2::MCP_TOOL_NAME, &name));
    }
    if !desc.is_empty() {
        parts.push(encode_len_field(field2::MCP_TOOL_DESC, &desc));
    }
    if let Json::Object(map) = &input_schema {
        if !map.is_empty() {
            let serialized = serde_json::to_string(&input_schema).unwrap_or_default();
            parts.push(encode_len_field(field2::MCP_TOOL_PARAMS, &serialized));
        }
    }
    parts.push(encode_len_field(field2::MCP_TOOL_SERVER, "custom"));
    concat_many(parts)
}

fn encode_cursor_setting() -> Vec<u8> {
    let unknown6 = concat_many(vec![
        encode_len_field(field2::SETTING6_FIELD_1, ""),
        encode_len_field(field2::SETTING6_FIELD_2, ""),
    ]);
    concat_many(vec![
        encode_len_field(field2::SETTING_PATH, r"cursor\aisettings"),
        encode_len_field(field2::SETTING_UNKNOWN_3, ""),
        {
            let tag = (field2::SETTING_UNKNOWN_6 << 3) | WIRE_TYPE_LEN as u32;
            let mut out = encode_varint(tag as u64);
            out.extend(encode_varint(unknown6.len() as u64));
            out.extend_from_slice(&unknown6);
            out
        },
        encode_varint_field(field2::SETTING_UNKNOWN_8, 1),
        encode_varint_field(field2::SETTING_UNKNOWN_9, 1),
    ])
}

fn current_platform() -> (&'static str, &'static str) {
    #[cfg(target_os = "windows")]
    let platform = "win32";
    #[cfg(target_os = "macos")]
    let platform = "darwin";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let platform = "linux";

    #[cfg(target_arch = "aarch64")]
    let arch = "arm64";
    #[cfg(not(target_arch = "aarch64"))]
    let arch = "x64";

    (platform, arch)
}

fn encode_metadata() -> Vec<u8> {
    let (platform, arch) = current_platform();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string());
    concat_many(vec![
        encode_len_field(field2::META_PLATFORM, platform),
        encode_len_field(field2::META_ARCH, arch),
        // shortcut: rustc's version stands in for `process.version` (node ABI string),
        // upgrade to a pinned node version if Cursor ever validates it
        encode_len_field(field2::META_VERSION, concat!("v", env!("CARGO_PKG_RUST_VERSION"))),
        encode_len_field(field2::META_CWD, &cwd),
        encode_len_field(field2::META_TIMESTAMP, &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    ])
}

fn encode_message_id(message_id: &str, role: u64) -> Vec<u8> {
    concat_many(vec![
        encode_len_field(field2::MSGID_ID, message_id),
        encode_varint_field(field2::MSGID_ROLE, role),
    ])
}

fn encode_model(model_name: &str) -> Vec<u8> {
    concat_many(vec![
        encode_len_field(field2::MODEL_NAME, model_name),
        encode_len_field(field2::MODEL_EMPTY, ""),
    ])
}

fn encode_instruction(text: &str) -> Vec<u8> {
    if text.is_empty() {
        Vec::new()
    } else {
        encode_len_field(field2::INSTRUCTION_TEXT, text)
    }
}

fn encode_mcp_result(selected_tool: &str, result_content: &str) -> Vec<u8> {
    concat_many(vec![
        encode_len_field(field2::MCPR_SELECTED_TOOL, selected_tool),
        encode_len_field(field2::MCPR_RESULT, result_content),
    ])
}

fn encode_client_side_tool_v2_result(
    tool_call_id: &str,
    model_call_id: Option<&str>,
    selected_tool: &str,
    result_content: &str,
    tool_index: u64,
) -> Vec<u8> {
    let mut parts = vec![
        encode_varint_field(field2::CV2R_TOOL, CLIENT_SIDE_TOOL_V2_MCP),
        {
            let nested = encode_mcp_result(selected_tool, result_content);
            let tag = (field2::CV2R_MCP_RESULT << 3) | WIRE_TYPE_LEN as u32;
            let mut out = encode_varint(tag as u64);
            out.extend(encode_varint(nested.len() as u64));
            out.extend_from_slice(&nested);
            out
        },
        encode_len_field(field2::CV2R_CALL_ID, tool_call_id),
    ];
    if let Some(mcid) = model_call_id {
        parts.push(encode_len_field(field2::CV2R_MODEL_CALL_ID, mcid));
    }
    parts.push(encode_varint_field(field2::CV2R_TOOL_INDEX, if tool_index > 0 { tool_index } else { 1 }));
    concat_many(parts)
}

fn encode_mcp_params_for_call(tool_name: &str, raw_args: &str, server_name: &str) -> Vec<u8> {
    let tool = concat_many(vec![
        encode_len_field(field2::MCP_TOOL_NAME, tool_name),
        encode_len_field(field2::MCP_TOOL_PARAMS, raw_args),
        encode_len_field(field2::MCP_TOOL_SERVER, server_name),
    ]);
    let tag = (field::MCP_TOOLS_LIST << 3) | WIRE_TYPE_LEN as u32;
    let mut out = encode_varint(tag as u64);
    out.extend(encode_varint(tool.len() as u64));
    out.extend_from_slice(&tool);
    out
}

fn encode_client_side_tool_v2_call(
    tool_call_id: &str,
    tool_name: &str,
    selected_tool: &str,
    server_name: &str,
    raw_args: &str,
    model_call_id: Option<&str>,
    tool_index: u64,
) -> Vec<u8> {
    let mut parts = vec![
        encode_varint_field(field2::CV2C_TOOL, CLIENT_SIDE_TOOL_V2_MCP),
        {
            let nested = encode_mcp_params_for_call(selected_tool, raw_args, server_name);
            let tag = (field2::CV2C_MCP_PARAMS << 3) | WIRE_TYPE_LEN as u32;
            let mut out = encode_varint(tag as u64);
            out.extend(encode_varint(nested.len() as u64));
            out.extend_from_slice(&nested);
            out
        },
        encode_len_field(field2::CV2C_CALL_ID, tool_call_id),
        encode_len_field(field2::CV2C_NAME, tool_name),
        encode_len_field(field2::CV2C_RAW_ARGS, raw_args),
    ];
    parts.push(encode_varint_field(field2::CV2C_TOOL_INDEX, if tool_index > 0 { tool_index } else { 1 }));
    if let Some(mcid) = model_call_id {
        parts.push(encode_len_field(field2::CV2C_MODEL_CALL_ID, mcid));
    }
    concat_many(parts)
}

fn tool_result_field(tool_result: &Value) -> Vec<u8> {
    let original_name = tool_result
        .get("tool_name")
        .and_then(Json::as_str)
        .or_else(|| tool_result.get("name").and_then(Json::as_str))
        .unwrap_or("");
    let tool_name = format_tool_name(original_name);
    let raw_args = tool_result
        .get("raw_args")
        .and_then(Json::as_str)
        .unwrap_or("{}")
        .to_string();
    let result_content = tool_result
        .get("result_content")
        .and_then(Json::as_str)
        .or_else(|| tool_result.get("result").and_then(Json::as_str))
        .unwrap_or("")
        .to_string();
    let (tool_call_id, model_call_id) = parse_tool_id(
        tool_result
            .get("tool_call_id")
            .and_then(Json::as_str)
            .unwrap_or(""),
    );
    let tool_index = tool_result
        .get("tool_index")
        .and_then(Json::as_u64)
        .or_else(|| tool_result.get("index").and_then(Json::as_u64))
        .unwrap_or(1);
    let (server_name, selected_tool) = parse_tool_name(&tool_name);

    let mut parts = vec![
        encode_len_field(field2::TOOL_RESULT_CALL_ID, &tool_call_id),
        encode_len_field(field2::TOOL_RESULT_NAME, &tool_name),
        encode_varint_field(field2::TOOL_RESULT_INDEX, if tool_index > 0 { tool_index } else { 1 }),
    ];
    if let Some(mcid) = &model_call_id {
        parts.push(encode_len_field(field2::TOOL_RESULT_MODEL_CALL_ID, mcid));
    }
    parts.push(encode_len_field(field2::TOOL_RESULT_RAW_ARGS, &raw_args));
    parts.push({
        let nested = encode_client_side_tool_v2_result(
            &tool_call_id,
            model_call_id.as_deref(),
            &selected_tool,
            &result_content,
            tool_index,
        );
        let tag = (field2::TOOL_RESULT_RESULT << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        out
    });
    parts.push({
        let nested = encode_client_side_tool_v2_call(
            &tool_call_id,
            &tool_name,
            &selected_tool,
            &server_name,
            &raw_args,
            model_call_id.as_deref(),
            tool_index,
        );
        let tag = (field2::TOOL_RESULT_TOOL_CALL << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        out
    });
    concat_many(parts)
}

fn role_number(role: &str) -> u64 {
    if role == "user" {
        ROLE_USER
    } else {
        ROLE_ASSISTANT
    }
}

fn encode_message(
    content: &str,
    role: u64,
    message_id: &str,
    is_last: bool,
    has_tools: bool,
    tool_results: &[Value],
) -> Vec<u8> {
    let has_tool_results = !tool_results.is_empty();
    let mut parts = vec![
        encode_len_field(field::MSG_CONTENT, content),
        encode_varint_field(field::MSG_ROLE, role),
        encode_len_field(field::MSG_ID, message_id),
    ];
    if has_tool_results {
        for tr in tool_results {
            let nested = tool_result_field(tr);
            let tag = (field::MSG_TOOL_RESULTS << 3) | WIRE_TYPE_LEN as u32;
            let mut out = encode_varint(tag as u64);
            out.extend(encode_varint(nested.len() as u64));
            out.extend_from_slice(&nested);
            parts.push(out);
        }
    }
    parts.push(encode_varint_field(field::MSG_IS_AGENTIC, if has_tools { 1 } else { 0 }));
    parts.push(encode_varint_field(
        field::MSG_UNIFIED_MODE,
        if has_tools { UNIFIED_MODE_AGENT } else { UNIFIED_MODE_CHAT },
    ));
    if is_last && has_tools {
        let varint = encode_varint(1);
        let tag = (field::MSG_SUPPORTED_TOOLS << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(varint.len() as u64));
        out.extend_from_slice(&varint);
        parts.push(out);
    }
    concat_many(parts)
}

/// Guardrail from JS `encodeRequest`: split an assistant message that carries
/// both `tool_calls` and `tool_results` into two messages.
fn normalize_mixed_assistant(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(Json::as_str).unwrap_or("");
        let has_calls = msg
            .get("tool_calls")
            .and_then(Json::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let has_results = msg
            .get("tool_results")
            .and_then(Json::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if role != "assistant" || !has_calls || !has_results {
            out.push(msg.clone());
            continue;
        }

        let mut without_results = msg.clone();
        if let Some(obj) = without_results.as_object_mut() {
            obj.insert("tool_results".into(), Json::Array(Vec::new()));
        }
        out.push(without_results);

        let next = messages.get(i + 1);
        let next_same = next
            .and_then(|n| n.get("role").and_then(Json::as_str))
            .map(|r| r == "assistant")
            .unwrap_or(false)
            && next
                .and_then(|n| n.get("tool_results").and_then(Json::as_array))
                .map(|a| !a.is_empty())
                .unwrap_or(false);
        let current_ids: Vec<String> = msg
            .get("tool_results")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|tr| tr.get("tool_call_id").and_then(Json::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let next_ids: Vec<String> = next
            .and_then(|n| n.get("tool_results").and_then(Json::as_array))
            .map(|a| {
                a.iter()
                    .filter_map(|tr| tr.get("tool_call_id").and_then(Json::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let same_ids = !current_ids.is_empty()
            && current_ids.len() == next_ids.len()
            && current_ids.iter().all(|id| next_ids.contains(id));

        if !(next_same && same_ids) {
            out.push(serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_results": msg.get("tool_results").cloned().unwrap_or(Json::Array(Vec::new())),
            }));
        }
    }
    out
}

fn build_chat_request(
    messages: &[Value],
    model_name: &str,
    tools: &[Value],
    reasoning_effort: Option<&str>,
    force_agent_mode: bool,
) -> Vec<u8> {
    let has_tools = !tools.is_empty();
    let is_agentic = has_tools || force_agent_mode;
    let normalized = normalize_mixed_assistant(messages);

    let mut message_parts = Vec::new();
    let mut message_ids = Vec::new();
    let count = normalized.len();
    for (i, msg) in normalized.iter().enumerate() {
        let role = role_number(msg.get("role").and_then(Json::as_str).unwrap_or("assistant"));
        let msg_id = uuid::Uuid::new_v4().to_string();
        let is_last = i + 1 == count;
        let content = match msg.get("content") {
            Some(Json::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let tool_results = msg
            .get("tool_results")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();

        message_parts.push({
            let nested = encode_message(&content, role, &msg_id, is_last, has_tools, &tool_results);
            let tag = (field::MESSAGES << 3) | WIRE_TYPE_LEN as u32;
            let mut out = encode_varint(tag as u64);
            out.extend(encode_varint(nested.len() as u64));
            out.extend_from_slice(&nested);
            out
        });
        message_ids.push(encode_message_id(&msg_id, role));
    }

    let thinking_level = match reasoning_effort {
        Some("medium") => THINKING_LEVEL_MEDIUM,
        Some("high") => THINKING_LEVEL_HIGH,
        _ => THINKING_LEVEL_UNSPECIFIED,
    };

    let mut parts = message_parts;
    parts.extend(message_ids.iter().map(|mid| {
        let tag = (field::MESSAGE_IDS << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(mid.len() as u64));
        out.extend_from_slice(mid);
        out
    }));
    parts.push(encode_varint_field(field::UNKNOWN_2, 1));
    parts.push({
        let nested = encode_instruction("");
        let tag = (field::INSTRUCTION << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        out
    });
    parts.push(encode_varint_field(field::UNKNOWN_4, 1));
    parts.push({
        let nested = encode_model(model_name);
        let tag = (field::MODEL << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        out
    });
    parts.push(encode_len_field(field::WEB_TOOL, ""));
    parts.push(encode_varint_field(field::UNKNOWN_13, 1));
    parts.push({
        let nested = encode_cursor_setting();
        let tag = (field::CURSOR_SETTING << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        out
    });
    parts.push(encode_varint_field(field::UNKNOWN_19, 1));
    parts.push(encode_len_field(field::CONVERSATION_ID, &uuid::Uuid::new_v4().to_string()));
    parts.push({
        let nested = encode_metadata();
        let tag = (field::METADATA << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        out
    });
    parts.push(encode_varint_field(field::IS_AGENTIC, if is_agentic { 1 } else { 0 }));
    if is_agentic {
        let varint = encode_varint(1);
        let tag = (field::SUPPORTED_TOOLS << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(varint.len() as u64));
        out.extend_from_slice(&varint);
        parts.push(out);
    }
    for tool in tools {
        let nested = encode_mcp_tool(tool);
        let tag = (field::MCP_TOOLS << 3) | WIRE_TYPE_LEN as u32;
        let mut out = encode_varint(tag as u64);
        out.extend(encode_varint(nested.len() as u64));
        out.extend_from_slice(&nested);
        parts.push(out);
    }
    parts.push(encode_varint_field(field::LARGE_CONTEXT, 0));
    parts.push(encode_varint_field(field::UNKNOWN_38, 0));
    parts.push(encode_varint_field(
        field::UNIFIED_MODE,
        if is_agentic { UNIFIED_MODE_AGENT } else { UNIFIED_MODE_CHAT },
    ));
    parts.push(encode_len_field(field::UNKNOWN_47, ""));
    parts.push(encode_varint_field(field::SHOULD_DISABLE_TOOLS, if is_agentic { 0 } else { 1 }));
    parts.push(encode_varint_field(field::THINKING_LEVEL, thinking_level));
    parts.push(encode_varint_field(field::UNKNOWN_51, 0));
    parts.push(encode_varint_field(field::UNKNOWN_53, 1));
    parts.push(encode_len_field(
        field::UNIFIED_MODE_NAME,
        if is_agentic { "Agent" } else { "Ask" },
    ));

    let request = concat_many(parts);
    let tag = (field::REQUEST << 3) | WIRE_TYPE_LEN as u32;
    let mut out = encode_varint(tag as u64);
    out.extend(encode_varint(request.len() as u64));
    out.extend_from_slice(&request);
    out
}

/// Port of JS `generateCursorBody`: framed Connect-RPC payload (never compressed).
pub fn generate_cursor_body(
    messages: &[Value],
    model: &str,
    tools: &[Value],
    reasoning_effort: Option<&str>,
    force_agent_mode: bool,
) -> Vec<u8> {
    let protobuf = build_chat_request(messages, model, tools, reasoning_effort, force_agent_mode);
    wrap_connect_rpc_frame(&protobuf, false)
}

// ==================== AGENT SERVICE FRAMES (port of buildAgentRunFrame) ====================

/// `textFromContent`: string content, or the joined `text` parts of an array.
fn text_from_content(content: Option<&Value>) -> String {
    match content {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Json::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Json::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// JS `isTextPart`: absent, a string, or `{ type: "text" }`.
fn is_text_part(part: &Value) -> bool {
    match part {
        Json::String(_) | Json::Null => true,
        Json::Object(_) => part.get("type").and_then(Json::as_str) == Some("text"),
        _ => false,
    }
}

/// JS `isAgentCapableRequest`: every message is text-only (no image parts).
pub fn is_agent_capable_request(body: &Value) -> bool {
    let messages = match body.get("messages").and_then(Json::as_array) {
        Some(m) if !m.is_empty() => m,
        _ => return false,
    };
    messages.iter().all(|message| {
        match message.get("content") {
            Some(Json::Array(parts)) => parts.iter().all(is_text_part),
            Some(Json::Null) | None => true,
            Some(Json::String(_)) => true,
            Some(_) => false,
        }
    })
}

/// JS `encodeHistoryMessage`: one agent.v1 conversation-history entry.
fn encode_history_message(message: &Value) -> Option<Vec<u8>> {
    let content = text_from_content(message.get("content"));
    let role = message.get("role").and_then(Json::as_str).unwrap_or("");
    let mut extras: Vec<String> = Vec::new();

    if role == "assistant" {
        if let Some(calls) = message.get("tool_calls").and_then(Json::as_array) {
            for tc in calls {
                let id = tc.get("id").and_then(Json::as_str).unwrap_or("");
                let function = tc.get("function").cloned().unwrap_or(Json::Null);
                let name = function.get("name").and_then(Json::as_str).unwrap_or("tool");
                let args = function.get("arguments").and_then(Json::as_str).unwrap_or("{}");
                extras.push(format!("[tool_call id={id} name={name} args={args}]"));
            }
        }
    }
    if role == "tool" {
        let id = message.get("tool_call_id").and_then(Json::as_str).unwrap_or("");
        extras.push(format!("[tool_result id={id}]"));
    }

    let text_body = std::iter::once(content)
        .chain(extras)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text_body.is_empty() {
        return None;
    }

    // ConversationHistoryMessage.user / .assistant -> repeated content -> text.
    let text = encode_len_field(1, &text_body);
    let inner = wrap_len(1, &wrap_len(1, &text));
    Some(if role == "assistant" {
        wrap_len(2, &inner)
    } else {
        wrap_len(1, &inner)
    })
}

/// `encodeField(field, WIRE_TYPE.LEN, payload_bytes)` for an already-encoded message.
fn wrap_len(field_num: u32, payload: &[u8]) -> Vec<u8> {
    let tag = (field_num << 3) | WIRE_TYPE_LEN as u32;
    let mut out = encode_varint(tag as u64);
    out.extend(encode_varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

/// Port of JS `buildAgentRunFrame`: the AgentService Run request frame.
///
/// `custom_system_prompt` (RunRequest field 8) makes AgentService return an
/// empty turn, so system text is folded into the current user message instead.
pub fn build_agent_run_frame(
    messages: &[Value],
    model: &str,
    tools: &[Value],
    reasoning_effort: Option<&str>,
) -> Vec<u8> {
    let system = messages
        .iter()
        .filter(|m| m.get("role").and_then(Json::as_str) == Some("system"))
        .map(|m| text_from_content(m.get("content")))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let chat_messages: Vec<&Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Json::as_str) != Some("system"))
        .collect();

    let current_index = chat_messages
        .iter()
        .rposition(|m| m.get("role").and_then(Json::as_str) == Some("user"));

    let current = match current_index {
        Some(i) => Some(chat_messages[i]),
        None => chat_messages.last().copied(),
    };

    let history: Vec<Vec<u8>> = match current_index {
        Some(i) => chat_messages[..i].iter().filter_map(|m| encode_history_message(m)).collect(),
        None => chat_messages
            .iter()
            .take(chat_messages.len().saturating_sub(1))
            .filter_map(|m| encode_history_message(m))
            .collect(),
    };

    let raw_user = {
        let text = current.map(|m| text_from_content(m.get("content"))).unwrap_or_default();
        if text.is_empty() { "Continue.".to_string() } else { text }
    };
    let user_text = if system.is_empty() { raw_user } else { format!("{system}\n\n{raw_user}") };

    // agent.v1.UserMessageAction.user_message: text, id, selected_context, mode=1.
    // Without selected_context (3) + mode (4) the server may accept the RPC and
    // stream an empty turn.
    let user_message = concat_many(vec![
        encode_len_field(1, &user_text),
        encode_len_field(2, &uuid::Uuid::new_v4().to_string()),
        wrap_len(3, &[]),
        encode_varint_field(4, 1),
    ]);

    let conversation_history = if history.is_empty() {
        None
    } else {
        Some(concat_many(history.iter().map(|entry| wrap_len(1, entry)).collect()))
    };

    let mut user_action_parts = vec![wrap_len(1, &user_message)];
    if let Some(history) = &conversation_history {
        user_action_parts.push(wrap_len(7, history));
    }
    let user_action = concat_many(user_action_parts);
    let conversation_action = wrap_len(1, &user_action);

    let effort = reasoning_effort.unwrap_or("").to_lowercase();
    let effort_value = match effort.as_str() {
        "ultra" => "max",
        "minimal" => "low",
        other => other,
    };
    let parameter = if !effort_value.is_empty() && effort_value != "none" {
        let is_gpt = model.to_lowercase().contains("gpt-");
        Some(wrap_len(
            3,
            &concat_many(vec![
                encode_len_field(1, if is_gpt { "reasoning" } else { "effort" }),
                encode_len_field(2, effort_value),
            ]),
        ))
    } else {
        None
    };

    let mut requested_model_parts = vec![encode_len_field(1, model), encode_varint_field(7, 1)];
    if let Some(parameter) = &parameter {
        requested_model_parts.push(parameter.clone());
    }
    let requested_model = concat_many(requested_model_parts);

    // ModelDetails (field 3): thinking variants return an empty turn when only
    // RequestedModel (field 9) is set.
    let model_details = concat_many(vec![
        encode_len_field(1, model),
        encode_len_field(3, model),
        encode_len_field(4, model),
    ]);

    let mcp_tools = encode_mcp_tools(tools);

    let mut run_request_parts = vec![
        // An empty ConversationStateStructure starts a fresh local agent session.
        wrap_len(1, &[]),
        wrap_len(2, &conversation_action),
        wrap_len(3, &model_details),
    ];
    if !mcp_tools.is_empty() {
        run_request_parts.push(wrap_len(4, &mcp_tools));
    }
    run_request_parts.push(wrap_len(9, &requested_model));

    // agent.v1.AgentClientMessage.run_request.
    wrap_connect_rpc_frame(&wrap_len(1, &concat_many(run_request_parts)), false)
}

/// `encodeMcpTools`: agent.v1.McpTools wrapper over each tool definition.
pub fn encode_mcp_tools(tools: &[Value]) -> Vec<u8> {
    if tools.is_empty() {
        return Vec::new();
    }
    let definitions = tools
        .iter()
        .map(encode_mcp_tool_definition)
        .filter(|d| !d.is_empty())
        .collect::<Vec<_>>();
    if definitions.is_empty() {
        return Vec::new();
    }
    wrap_len(1, &concat_many(definitions))
}

/// `encodeMcpToolDefinition`: MTD_NAME/…/MTD_TOOL_NAME over the OpenAI tool shape.
fn encode_mcp_tool_definition(tool: &Value) -> Vec<u8> {
    let function = tool.get("function").filter(|v| v.is_object()).cloned().unwrap_or_else(|| tool.clone());
    let name = function.get("name").and_then(Json::as_str).unwrap_or("");
    let description = function.get("description").and_then(Json::as_str).unwrap_or("");
    let schema = function.get("parameters").cloned().unwrap_or(Json::Object(Default::default()));

    let mut parts = Vec::new();
    if !name.is_empty() {
        parts.push(encode_len_field(1, name));
    }
    if !description.is_empty() {
        parts.push(encode_len_field(2, description));
    }
    parts.push(wrap_len(3, &encode_agent_value(&schema)));
    parts.push(encode_len_field(4, "custom"));
    parts.push(encode_len_field(5, name));
    concat_many(parts)
}

/// `encodeAgentValue`: google.protobuf.Value body (no outer tag).
pub fn encode_agent_value(value: &Value) -> Vec<u8> {
    match value {
        Json::Null => encode_varint_field(1, 0),
        Json::Bool(b) => encode_varint_field(4, if *b { 1 } else { 0 }),
        Json::Number(n) => {
            let tag = (2u32 << 3) | WIRE_TYPE_FIXED64 as u32;
            let mut out = encode_varint(tag as u64);
            out.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_le_bytes());
            out
        }
        Json::String(s) => encode_len_field(3, s),
        Json::Array(items) => {
            let list = concat_many(items.iter().map(|item| wrap_len(1, &encode_agent_value(item))).collect());
            wrap_len(6, &list)
        }
        Json::Object(map) => {
            let entries = map
                .iter()
                .map(|(key, val)| {
                    wrap_len(
                        1,
                        &concat_many(vec![encode_len_field(1, key), wrap_len(2, &encode_agent_value(val))]),
                    )
                })
                .collect();
            wrap_len(5, &concat_many(entries))
        }
    }
}

/// Port of JS `decodeAgentFrames`: parse Connect frames, gunzip when flagged.
/// Returns (payloads, leftover bytes).
pub fn decode_agent_frames(buffer: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
    const COMPRESS_GZIP: u8 = 0x01;
    const COMPRESS_TRAILER: u8 = 0x02;
    let mut frames = Vec::new();
    let mut pending = buffer.to_vec();
    while pending.len() >= 5 {
        let flags = pending[0];
        let length = u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
        if pending.len() < 5 + length {
            break;
        }
        let payload = pending[5..5 + length].to_vec();
        pending = pending[5 + length..].to_vec();
        let payload = if flags & COMPRESS_GZIP != 0 {
            match gunzip(&payload) {
                Some(d) => d,
                None => payload,
            }
        } else {
            payload
        };
        if flags & COMPRESS_TRAILER == 0 {
            frames.push(payload);
        }
    }
    (frames, pending)
}

/// Inflate a gzip payload (JS `zlib.gunzipSync`).
fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok().map(|_| out)
}

/// Port of JS `parseConnectRPCFrame`: one frame from the front of `buffer`.
/// Returns (payload, consumed) or None when the frame is incomplete.
pub fn parse_connect_rpc_frame(buffer: &[u8]) -> Option<(Vec<u8>, usize)> {
    if buffer.len() < 5 {
        return None;
    }
    let flags = buffer[0];
    let length = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
    if buffer.len() < 5 + length {
        return None;
    }
    let mut payload = buffer[5..5 + length].to_vec();
    if flags == 0x01 {
        if let Some(d) = gunzip(&payload) {
            payload = d;
        }
    }
    Some((payload, 5 + length))
}

/// Extract the AgentService `exec` request id and exec id.
pub fn exec_ids(exec_request: &[u8]) -> (u64, String) {
    let fields = decode_message(exec_request);
    let id = fields
        .get(&1)
        .and_then(|v| v.first())
        .map(|f| {
            let mut arr = [0u8; 8];
            arr[..f.value.len().min(8)].copy_from_slice(&f.value[..f.value.len().min(8)]);
            u64::from_le_bytes(arr)
        })
        .unwrap_or(0);
    let exec_id = fields
        .get(&15)
        .and_then(|v| v.first())
        .map(|f| String::from_utf8_lossy(&f.value).to_string())
        .unwrap_or_default();
    (id, exec_id)
}

/// Port of JS `wrapExecClientMessage`: AgentClientMessage.exec_client_message.
pub fn wrap_exec_client_message(
    exec_msg_id: u64,
    exec_id: &str,
    result_field: u32,
    result_payload: &[u8],
) -> Vec<u8> {
    let mut parts = Vec::new();
    if exec_msg_id != 0 {
        parts.push(encode_varint_field(1, exec_msg_id));
    }
    parts.push(encode_len_field(15, exec_id));
    parts.push(wrap_len(result_field, result_payload));
    wrap_connect_rpc_frame(&wrap_len(2, &concat_many(parts)), false)
}

/// Port of JS `createRequestContextResponse`: acknowledge a request-context exec.
pub fn create_request_context_response(exec_request: &[u8]) -> Vec<u8> {
    let (id, exec_id) = exec_ids(exec_request);
    let request_context_success = wrap_len(1, &[]);
    let request_context_result = wrap_len(1, &request_context_success);
    wrap_exec_client_message(id, &exec_id, 10, &request_context_result)
}

/// ExecServerMessage variant → ExecClientMessage result field (same numbers).
const EXEC_RESULT_FIELD: &[(u32, u32)] = &[
    (2, 2), (3, 3), (4, 4), (5, 5), (7, 7), (8, 8), (9, 9), (16, 16), (20, 20), (23, 23),
];

/// Port of JS `rejectExecRequest`: refuse a tool the host cannot run.
pub fn reject_exec_request(exec_request: &[u8]) -> Option<Vec<u8>> {
    let (id, exec_id) = exec_ids(exec_request);
    let fields = decode_message(exec_request);
    let variant = fields
        .keys()
        .copied()
        .find(|f| *f != 1 && *f != 15)?;
    let result_field = EXEC_RESULT_FIELD
        .iter()
        .find(|(v, _)| *v == variant)
        .map(|(_, r)| *r)?;
    // Diagnostics has no rejected variant — empty success unblocks the stream.
    if variant == 9 {
        return Some(wrap_exec_client_message(id, &exec_id, 9, &[]));
    }
    let rejected = wrap_len(
        2,
        &encode_len_field(2, "Tool not available in this environment. Use the MCP tools provided instead."),
    );
    Some(wrap_exec_client_message(id, &exec_id, result_field, &rejected))
}

// ==================== STREAM DECODING (port of cursor.js readCursorFrame) ====================

const COMPRESS_NONE: u8 = 0x00;
const COMPRESS_GZIP: u8 = 0x01;
const COMPRESS_TRAILER: u8 = 0x02;
const COMPRESS_GZIP_TRAILER: u8 = 0x03;

/// Result of reading one frame from the Cursor buffer.
pub enum CursorFrame {
    /// Not enough bytes yet (or trailing partial frame): reading is done.
    Done,
    /// Decompression failed; skip this frame.
    Skip(usize),
    /// A decoded payload and the next offset.
    Ok(Vec<u8>, usize),
}

/// `decompressPayload`: gunzip, then zlib deflate, then raw deflate.
///
/// A JSON error body is returned untouched: Cursor sometimes answers a failed
/// request with a plain JSON document behind a compressed flag.
fn decompress_payload(payload: Vec<u8>, flags: u8) -> Vec<u8> {
    if payload.len() > 10 && payload[0] == 0x7b && payload[1] == 0x22 {
        if let Ok(text) = std::str::from_utf8(&payload) {
            if text.starts_with("{\"error\"") {
                return payload;
            }
        }
    }

    if flags == COMPRESS_GZIP || flags == COMPRESS_TRAILER || flags == COMPRESS_GZIP_TRAILER {
        if let Some(out) = gunzip(&payload) {
            return out;
        }
        if let Some(out) = inflate_zlib(&payload) {
            return out;
        }
        if let Some(out) = inflate_raw(&payload) {
            return out;
        }
    }
    payload
}

fn inflate_zlib(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    match decoder.read_to_end(&mut out) {
        Ok(_) => Some(out),
        Err(_) => None,
    }
}

fn inflate_raw(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let mut out = Vec::new();
    match decoder.read_to_end(&mut out) {
        Ok(_) => Some(out),
        Err(_) => None,
    }
}

/// `readCursorFrame`: header + bounds + decompress at `offset`.
pub fn read_cursor_frame(buffer: &[u8], offset: usize) -> CursorFrame {
    if offset + 5 > buffer.len() {
        return CursorFrame::Done;
    }
    let flags = buffer[offset];
    let length =
        u32::from_be_bytes([buffer[offset + 1], buffer[offset + 2], buffer[offset + 3], buffer[offset + 4]])
            as usize;
    if offset + 5 + length > buffer.len() {
        return CursorFrame::Done;
    }
    let payload = buffer[offset + 5..offset + 5 + length].to_vec();
    let new_offset = offset + 5 + length;
    let payload = decompress_payload(payload, flags);
    if payload.is_empty() && length > 0 {
        return CursorFrame::Skip(new_offset);
    }
    CursorFrame::Ok(payload, new_offset)
}

/// `isComposerModel`: `composer`, `composer-*`, optionally behind a vendor prefix.
pub fn is_composer_model(model: &str) -> bool {
    let model_id = model.rsplit('/').next().unwrap_or(model);
    let lower = model_id.to_lowercase();
    lower == "composer" || lower.starts_with("composer-")
}

/// `visibleComposerContentFromThinking`: text after the last `</think>`.
pub fn visible_composer_content_from_thinking(thinking: &str) -> String {
    const END_TAG: &str = "</think>";
    match thinking.rfind(END_TAG) {
        Some(idx) => thinking[idx + END_TAG.len()..].trim_start().to_string(),
        None => String::new(),
    }
}

/// `tailOnlyCursorMessages`: keep the latest turn for a wedged long session.
pub fn tail_only_cursor_messages(messages: &[Value]) -> Vec<Value> {
    if messages.is_empty() {
        return Vec::new();
    }
    let system_messages: Vec<Value> = messages
        .iter()
        .filter(|m| {
            m.get("role").and_then(Json::as_str) == Some("system")
                && !m.get("content").and_then(Json::as_str).unwrap_or("").contains("[9router]")
        })
        .cloned()
        .collect();
    let chat_messages: Vec<Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Json::as_str) != Some("system"))
        .cloned()
        .collect();

    let mut anchor = chat_messages.len().saturating_sub(1);
    for i in (0..chat_messages.len()).rev() {
        let role = chat_messages[i].get("role").and_then(Json::as_str).unwrap_or("");
        if role == "user" || role == "tool" {
            anchor = i;
            break;
        }
    }
    let tail_start = if anchor > 0
        && chat_messages[anchor - 1].get("role").and_then(Json::as_str) == Some("assistant")
    {
        anchor - 1
    } else {
        anchor
    };
    let tail: Vec<Value> = chat_messages[tail_start.min(chat_messages.len())..].to_vec();

    let note = serde_json::json!({
        "role": "system",
        "content": format!(
            "[9router] Long session: {} messages trimmed to the latest turn. Answer the current request; re-read files if needed.",
            messages.len()
        ),
    });

    let mut out = system_messages;
    out.push(note);
    out.extend(tail);
    out
}
