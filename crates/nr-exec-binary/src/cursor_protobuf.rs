//! Port of open-sse/utils/cursorProtobuf.js
//! Protobuf encoding/decoding for Cursor API

use serde_json::{json, Value};
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

/// Generate Cursor body from messages
pub fn generate_cursor_body(
    messages: &[Value],
    model: &str,
    tools: &[Value],
    _reasoning_effort: Option<&str>,
    _force_agent_mode: bool,
) -> Vec<u8> {
    // TODO: Implement full message encoding
    let mut payload = Vec::new();
    wrap_connect_rpc_frame(&payload, false).into_iter().collect()
}
