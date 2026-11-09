//! Port of open-sse/utils/kiroSessionReplay.js
//! Session replay for Kiro to improve cache hit rate by freezing the first user message.

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use nr_core::Json;
use serde_json::json;

const MAX_SESSION_STARTS: usize = 5000;

fn default_session_ttl_ms() -> u128 {
    nr_data::runtime("SESSION_TTL_MS")
        .and_then(|v| v.as_u64())
        .unwrap_or(5 * 60 * 1000) as u128
}

fn session_key(connection_id: Option<&str>, conversation_id: Option<&str>) -> String {
    format!("{}:{}", connection_id.unwrap_or(""), conversation_id.unwrap_or(""))
}

fn clone_value(value: &Json) -> Json {
    serde_json::from_str(&value.to_string()).unwrap_or(Json::Null)
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn ensure_user_message_model_id(message: &mut Json, model_id: &str) {
    if let Some(msg) = message.get_mut("userInputMessage").filter(|m| m.is_object()) {
        if msg.get("modelId").map(|v| v.is_null()).unwrap_or(true) && !model_id.is_empty() {
            msg["modelId"] = json!(model_id);
        }
    }
}

fn ensure_history_model_ids(history: &mut [Json], model_id: &str) {
    for item in history.iter_mut() {
        ensure_user_message_model_id(item, model_id);
    }
}

fn prefix_user_message(message: Option<&Json>, content_prefix: &str, model_id: &str) -> Json {
    let mut out = match message {
        Some(m) => clone_value(m),
        None => json!({"userInputMessage": {"content": ""}}),
    };
    if out.get("userInputMessage").map(|v| v.is_null()).unwrap_or(true) {
        out["userInputMessage"] = json!({"content": ""});
    }
    ensure_user_message_model_id(&mut out, model_id);
    if !content_prefix.is_empty() {
        let content = out["userInputMessage"].get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        out["userInputMessage"]["content"] = if content.is_empty() {
            json!(content_prefix)
        } else {
            json!(format!("{content_prefix}\n\n{content}"))
        };
    }
    out
}

fn find_first_user_index(history: &[Json]) -> Option<usize> {
    history.iter().position(|item| item.get("userInputMessage").map(|v| !v.is_null()).unwrap_or(false))
}

fn has_tool_results(message: &Json) -> bool {
    message
        .get("userInputMessage")
        .and_then(|u| u.get("userInputMessageContext"))
        .and_then(|c| c.get("toolResults"))
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

fn can_replace_session_start(history: &[Json], first_user_index: Option<usize>) -> bool {
    match first_user_index {
        Some(0) => !has_tool_results(&history[0]),
        _ => false,
    }
}

#[derive(Clone)]
struct SessionEntry {
    session_start: Json,
    model_id: String,
    system_prompt: String,
    last_used: u128,
}

#[derive(Default)]
pub struct KiroSessionReplayStore {
    entries: HashMap<String, SessionEntry>,
    order: VecDeque<String>,
}

impl KiroSessionReplayStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn remember(&mut self, key: String, entry: SessionEntry) {
        if self.entries.insert(key.clone(), entry).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > MAX_SESSION_STARTS {
            let oldest = self.order.pop_front();
            if let Some(k) = oldest {
                self.entries.remove(&k);
            }
        }
    }

    /// Preserve Kiro cacheability by freezing the first user message (`msg0`) for
    /// a session, replaying that exact message as the first history user on later
    /// turns, and injecting volatile current-time context only into the current turn.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &mut self,
        conversation_id: Option<&str>,
        connection_id: Option<&str>,
        model_id: &str,
        system_prompt: &str,
        content_prefix: &str,
        current_content_prefix: &str,
        history: &[Json],
        current_message: Option<&Json>,
    ) -> (Vec<Json>, Json, bool) {
        let key = session_key(connection_id, conversation_id);
        let mut base_history: Vec<Json> = history.iter().map(clone_value).collect();
        let base_current = match current_message {
            Some(m) => clone_value(m),
            None => json!({"userInputMessage": {"content": ""}}),
        };

        // Replay the frozen first user message when this session/model/prompt matches.
        let existing = conversation_id
            .filter(|id| !id.is_empty())
            .and_then(|_| self.entries.get(&key))
            .filter(|e| e.model_id == model_id && e.system_prompt == system_prompt)
            .map(|e| e.session_start.clone());

        if let Some(session_start) = existing {
            self.entries.get_mut(&key).unwrap().last_used = now_ms();
            let first_user_index = find_first_user_index(&base_history);
            let mut session_start = session_start;
            ensure_user_message_model_id(&mut session_start, model_id);
            if can_replace_session_start(&base_history, first_user_index) {
                base_history[0] = session_start;
            } else {
                base_history.insert(0, session_start);
                if base_history.len() == 1 {
                    base_history.push(json!({"assistantResponseMessage": {"content": "..."}}));
                }
            }
            let current = prefix_user_message(Some(&base_current), current_content_prefix, model_id);
            ensure_history_model_ids(&mut base_history, model_id);
            return (base_history, current, true);
        }

        let first_user_index = find_first_user_index(&base_history);
        let session_start;
        let next_current;
        if can_replace_session_start(&base_history, first_user_index) {
            session_start = prefix_user_message(Some(&base_history[0]), content_prefix, model_id);
            base_history[0] = clone_value(&session_start);
            next_current = prefix_user_message(Some(&base_current), current_content_prefix, model_id);
        } else if first_user_index.is_some() {
            let seed = json!({"userInputMessage": {"content": "", "modelId": model_id}});
            session_start = prefix_user_message(Some(&seed), content_prefix, model_id);
            base_history.insert(0, clone_value(&session_start));
            next_current = prefix_user_message(Some(&base_current), current_content_prefix, model_id);
        } else {
            session_start = prefix_user_message(Some(&base_current), content_prefix, model_id);
            next_current = session_start.clone();
        }

        if let Some(id) = conversation_id.filter(|s| !s.is_empty()) {
            self.remember(
                key,
                SessionEntry {
                    session_start: session_start.clone(),
                    model_id: model_id.to_string(),
                    system_prompt: system_prompt.to_string(),
                    last_used: now_ms(),
                },
            );
        }

        ensure_history_model_ids(&mut base_history, model_id);
        (base_history, next_current, false)
    }

    /// Drop entries past the session TTL.
    pub fn cleanup(&mut self) {
        let now = now_ms();
        let ttl = default_session_ttl_ms();
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.last_used) > ttl)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.entries.remove(&key);
            self.order.retain(|k| k != &key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replay_first_turn_and_replay() {
        let mut store = KiroSessionReplayStore::new();
        let history = vec![json!({"userInputMessage": {"content": "hello", "modelId": "m"}})];
        let current = json!({"userInputMessage": {"content": "go", "modelId": "m"}});
        let (h1, _c1, replayed1) = store.apply(Some("conv1"), Some("conn1"), "m", "sys", "PREFIX", "TIME", &history, Some(&current));
        assert!(!replayed1);
        assert_eq!(h1[0]["userInputMessage"]["content"], "PREFIX\n\nhello");

        let (h2, c2, replayed2) = store.apply(Some("conv1"), Some("conn1"), "m", "sys", "PREFIX", "TIME", &history, Some(&current));
        assert!(replayed2);
        assert_eq!(h2[0]["userInputMessage"]["content"], "PREFIX\n\nhello");
        assert_eq!(c2["userInputMessage"]["content"], "TIME\n\ngo");
    }

    #[test]
    fn test_replay_evicts_oldest() {
        let mut store = KiroSessionReplayStore::new();
        let history = vec![json!({"userInputMessage": {"content": "x", "modelId": "m"}})];
        for i in 0..=MAX_SESSION_STARTS {
            let conv = format!("conv{i}");
            store.apply(Some(&conv), Some("conn"), "m", "sys", "", "", &history, Some(&json!({"userInputMessage": {"content": "y", "modelId": "m"}})));
        }
        assert!(!store.entries.contains_key(&session_key(Some("conn"), Some("conv0"))));
        assert!(store.entries.contains_key(&session_key(Some("conn"), Some(&format!("conv{MAX_SESSION_STARTS}")))));
    }
}
