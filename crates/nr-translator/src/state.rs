//! Streaming translation state. Port of `initState` in open-sse/translator/index.js.
//!
//! Both translator agents may APPEND fields (never rename/remove). Anything
//! translator-private that doesn't deserve a typed field can go in `ext`.

use std::collections::HashMap;

use indexmap::IndexMap;
use nr_core::{Json, JsonMap};

#[derive(Clone, Debug, Default)]
pub struct StreamState {
    pub message_id: Option<String>,
    pub model: Option<String>,
    pub text_block_started: bool,
    pub thinking_block_started: bool,
    pub in_thinking_block: bool,
    pub current_block_index: Option<i64>,
    /// Tool calls being assembled, keyed by tool-call index.
    pub tool_calls: IndexMap<i64, Json>,
    /// Tool-call ids already emitted to the client, keyed by index (#4992).
    pub stream_tool_call_ids: HashMap<i64, String>,
    pub finish_reason: Option<String>,
    pub finish_reason_sent: bool,
    pub usage: Option<Json>,
    pub content_block_index: i64,
    /// Cloaked/renamed tool name → client tool name (JS `state.toolNameMap`).
    pub tool_name_map: Option<HashMap<String, String>>,
    /// Translator-private extras (the JS state is an open object).
    pub ext: JsonMap,
}
