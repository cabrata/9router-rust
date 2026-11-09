//! Port of open-sse/utils/kiroSessionReplay.js
//! Session replay for Kiro to improve cache hit rate by freezing the first user message.

use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration, Instant};

const MAX_SESSION_STARTS: usize = 5000;
const DEFAULT_SESSION_TTL_MS: u64 = 5 * 60 * 1000; // 5 minutes
const SESSION_CLEANUP_INTERVAL_MS: u64 = 60 * 1000; // 1 minute

type SessionKey = String;

#[derive(Clone, Debug)]
struct SessionEntry {
    session_start: JsonValue,
    model_id: String,
    system_prompt: String,
    last_used: u64,
}

pub struct KiroSessionReplayStore {
    sessions: Arc<Mutex<HashMap<SessionKey, SessionEntry>>>,
}

impl KiroSessionReplayStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn session_key(connection_id: Option<&str>, conversation_id: Option<&str>) -> String {
        format!(
            "{}:{}",
            connection_id.unwrap_or(""),
            conversation_id.unwrap_or("")
        )
    }

    fn ensure_user_message_model_id(message: &mut JsonValue, model_id: &str) {
        if let Some(msg) = message.get_mut("userInputMessage") {
            if msg.get("modelId").is_none() {
                msg["modelId"] = JsonValue::String(model_id.to_string());
            }
        }
    }

    fn ensure_history_model_ids(history: &mut Vec<JsonValue>, model_id: &str) {
        for item in history.iter_mut() {
            Self::ensure_user_message_model_id(item, model_id);
        }
    }

    fn prefix_user_message(
        message: &JsonValue,
        content_prefix: &str,
        model_id: &str,
    ) -> JsonValue {
        let mut out = message.clone();
        if !out.is_object() {
            out = json!({"userInputMessage": {"content": ""}});
        }
        if out.get("userInputMessage").is_none() {
            out["userInputMessage"] = json!({"content": ""});
        }

        Self::ensure_user_message_model_id(&mut out, model_id);

        if !content_prefix.is_empty() {
            if let Some(msg) = out.get_mut("userInputMessage") {
                let current_content = msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new_content = if current_content.is_empty() {
                    content_prefix.to_string()
                } else {
                    format!("{}\n\n{}", content_prefix, current_content)
                };
                msg["content"] = JsonValue::String(new_content);
            }
        }

        out
    }

    fn find_first_user_index(history: &[JsonValue]) -> Option<usize> {
        history
            .iter()
            .position(|item| item.get("userInputMessage").is_some())
    }

    fn has_tool_results(message: &JsonValue) -> bool {
        message
            .get("userInputMessage")
            .and_then(|msg| msg.get("userInputMessageContext"))
            .and_then(|ctx| ctx.get("toolResults"))
            .and_then(|tr| tr.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    }

    fn can_replace_session_start(
        history: &[JsonValue],
        first_user_index: Option<usize>,
    ) -> bool {
        first_user_index == Some(0)
            && !Self::has_tool_results(&history.get(0).cloned().unwrap_or(JsonValue::Null))
    }

    fn remember_session_start(&self, key: SessionKey, entry: SessionEntry) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= MAX_SESSION_STARTS {
            if let Some(first_key) = sessions.keys().next().cloned() {
                sessions.remove(&first_key);
            }
        }
        sessions.insert(key, entry);
    }

    pub fn apply_session_replay(
        &self,
        conversation_id: Option<&str>,
        connection_id: Option<&str>,
        model_id: &str,
        system_prompt: &str,
        content_prefix: &str,
        current_content_prefix: &str,
        history: &[JsonValue],
        current_message: &JsonValue,
    ) -> (Vec<JsonValue>, JsonValue, bool) {
        let key = Self::session_key(connection_id, conversation_id);
        let sessions = self.sessions.lock().unwrap();
        let existing = sessions.get(&key);

        let mut base_history = history.to_vec();
        let base_current = current_message.clone();

        if let Some(entry) = existing {
            if entry.model_id == model_id && entry.system_prompt == system_prompt {
                drop(sessions);
                // Update last used time
                self.remember_session_start(
                    key,
                    SessionEntry {
                        last_used: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                        ..entry.clone()
                    },
                );

                let first_user_index = Self::find_first_user_index(&base_history);
                let mut session_start = entry.session_start.clone();
                Self::ensure_user_message_model_id(&mut session_start, model_id);

                if Self::can_replace_session_start(&base_history, first_user_index) {
                    if let Some(idx) = first_user_index {
                        base_history[idx] = session_start;
                    }
                } else {
                    base_history.insert(0, session_start);
                    if base_history.len() == 1 {
                        base_history.push(json!({"assistantResponseMessage": {"content": "..."}}));
                    }
                }

                Self::ensure_history_model_ids(&mut base_history, model_id);
                let next_current =
                    Self::prefix_user_message(&base_current, current_content_prefix, model_id);
                return (base_history, next_current, true);
            }
        }

        drop(sessions);

        let first_user_index = Self::find_first_user_index(&base_history);
        let session_start: JsonValue;
        let next_current: JsonValue;

        if Self::can_replace_session_start(&base_history, first_user_index) {
            if let Some(idx) = first_user_index {
                session_start =
                    Self::prefix_user_message(&base_history[idx], content_prefix, model_id);
                base_history[idx] = session_start.clone();
                next_current =
                    Self::prefix_user_message(&base_current, current_content_prefix, model_id);
            } else {
                session_start = Self::prefix_user_message(&JsonValue::Null, content_prefix, model_id);
                next_current = session_start.clone();
            }
        } else if first_user_index.is_some() {
            session_start = Self::prefix_user_message(
                &JsonValue::Null,
                content_prefix,
                model_id,
            );
            base_history.insert(0, session_start.clone());
            next_current = Self::prefix_user_message(&base_current, current_content_prefix, model_id);
        } else {
            session_start = Self::prefix_user_message(&base_current, content_prefix, model_id);
            next_current = session_start.clone();
        }

        if conversation_id.is_some() {
            self.remember_session_start(
                key,
                SessionEntry {
                    session_start: session_start.clone(),
                    model_id: model_id.to_string(),
                    system_prompt: system_prompt.to_string(),
                    last_used: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                },
            );
        }

        Self::ensure_history_model_ids(&mut base_history, model_id);
        (base_history, next_current, false)
    }

    pub fn clear(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.clear();
    }
}

impl Default for KiroSessionReplayStore {
    fn default() -> Self {
        Self::new()
    }
}
