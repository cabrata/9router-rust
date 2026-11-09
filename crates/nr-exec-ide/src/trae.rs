//! Port of open-sse/executors/trae.js.
//!
//! Trae executor — SOLO remote agent API.
//!
//! Flow:
//!   1. POST {base}/chat_sessions          → { code:0, data:{ chat_session_id, message_id } }
//!   2. GET  {base}/chat_sessions/{id}/events?reply_to_message_id={message_id}
//!        → text/event-stream. Assistant text streams in `plan_item` events under
//!          the `thought` field (cumulative per plan-item id). `token_usage` carries
//!          usage; `done` ends the turn; `error` carries upstream errors.
//!
//! Auth: header `Authorization: Cloud-IDE-JWT <jwt>` (RS256, ~14-day lifetime).
//! Identity fields for common_params live in credentials.providerSpecificData.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{stream, StreamExt};
use nr_core::http::{BodyStream, HeaderList, UpstreamResponse};
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result};
use serde_json::json;

const DEFAULT_BASE_URL: &str = "https://core-normal.trae.ai/api/remote/v1";
const STREAM_TIMEOUT: Duration = Duration::from_millis(300000);
const TRAE_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

pub fn flatten_query(messages: &[Json]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        let mut content = String::new();
        if let Some(s) = m.get("content").and_then(Json::as_str) {
            content.push_str(s);
        } else if let Some(arr) = m.get("content").and_then(Json::as_array) {
            for p in arr {
                if let Some(s) = p.as_str() {
                    content.push_str(s);
                } else if let Some(t) = p.get("text").and_then(Json::as_str) {
                    content.push_str(t);
                }
            }
        }
        match m.get("role").and_then(Json::as_str) {
            Some("system") => parts.push(format!("[System]\n{content}")),
            Some("assistant") => parts.push(format!("[Assistant]\n{content}")),
            _ => parts.push(content),
        }
    }
    json!([{ "type": "text", "data": { "content": parts.join("\n\n") } }]).to_string()
}

pub struct ModeInfo {
    pub mode: &'static str,
    pub strategy: &'static str,
    pub model_name: String,
}

pub fn resolve_mode(model: &str) -> ModeInfo {
    let m = model.trim().to_lowercase();
    if m == "work" || m == "auto-work" || m == "solo-work" {
        return ModeInfo {
            mode: "work",
            strategy: "auto",
            model_name: String::new(),
        };
    }
    let auto = m.is_empty() || m == "auto";
    ModeInfo {
        mode: "code",
        strategy: if auto { "auto" } else { "manual" },
        model_name: if auto { String::new() } else { model.to_string() },
    }
}

pub fn common_params(creds: &nr_core::Credentials, mode: &str, session_id: Option<&str>) -> String {
    let psd = creds.psd();
    let get_str = |k: &str, def: &str| -> String {
        psd.and_then(|p| p.get(k))
            .and_then(Json::as_str)
            .unwrap_or(def)
            .to_string()
    };

    let mut cp = json!({
        "language": "en-us",
        "app_language": get_str("appLanguage", "en"),
        "quality": "stable",
        "app_version": get_str("appVersion", "1.0.0.1229"),
        "web_id": get_str("webId", ""),
        "user_identity": get_str("userIdentity", "Free"),
        "is_freshman": "0",
        "biz_user_id": get_str("bizUserId", ""),
        "user_unique_id": get_str("userUniqueId", ""),
        "scope": get_str("scope", "marscode-us"),
        "tenant": get_str("tenant", "marscode"),
        "region": get_str("region", "US-East"),
        "aiRegion": psd.and_then(|p| p.get("aiRegion")).and_then(Json::as_str).unwrap_or(&get_str("region", "US-East")),
        "is_privacy_mode": 0,
        "privacy_mode": "off",
        "solo_chat_mode": mode,
    });

    if let Some(sid) = session_id {
        if let Some(obj) = cp.as_object_mut() {
            obj.insert("biz_session_id".into(), Json::String(sid.to_string()));
        }
    }

    cp.to_string()
}

pub struct TraeExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl TraeExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("trae"))
            .unwrap_or(&EMPTY_CONFIG);
        Self {
            provider: provider.to_string(),
            config,
        }
    }

    fn base(&self) -> String {
        self.config
            .get("baseUrl")
            .and_then(Json::as_str)
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string()
    }

    fn build_headers(&self, creds: &nr_core::Credentials, stream: bool) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        let token = creds.access_token().unwrap_or("");
        headers.insert(
            "Authorization",
            reqwest::header::HeaderValue::from_str(&format!("Cloud-IDE-JWT {token}")).unwrap_or(reqwest::header::HeaderValue::from_static("")),
        );
        headers.insert("Content-Type", reqwest::header::HeaderValue::from_static("application/json"));
        headers.insert("X-Trae-Client-Type", reqwest::header::HeaderValue::from_static("web"));

        let psd = creds.psd();
        let lang = psd.and_then(|p| p.get("appLanguage")).and_then(Json::as_str).unwrap_or("en");
        let region = psd.and_then(|p| p.get("userRegion")).and_then(Json::as_str).unwrap_or("US");
        headers.insert("X-Preferenced-Language", reqwest::header::HeaderValue::from_str(lang).unwrap_or(reqwest::header::HeaderValue::from_static("en")));
        headers.insert("x-user-region", reqwest::header::HeaderValue::from_str(region).unwrap_or(reqwest::header::HeaderValue::from_static("US")));
        headers.insert("Referer", reqwest::header::HeaderValue::from_static("https://solo.trae.ai/"));
        headers.insert("User-Agent", reqwest::header::HeaderValue::from_static(TRAE_UA));
        if stream {
            headers.insert("Accept", reqwest::header::HeaderValue::from_static("text/event-stream"));
        } else {
            headers.insert("Accept", reqwest::header::HeaderValue::from_static("application/json"));
        }
        headers
    }
}

impl nr_exec_base::base::BaseHooks for TraeExecutor {
    fn provider(&self) -> &str {
        "trae"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }
}

#[async_trait]
impl Executor for TraeExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let base_url = self.base();
        let headers = self.build_headers(req.credentials, req.stream);
        let messages = req.body.get("messages").and_then(Json::as_array).cloned().unwrap_or_default();
        let query = flatten_query(&messages);
        let mode_info = resolve_mode(req.model);
        let common = common_params(req.credentials, mode_info.mode, None);

        let body = json!({
            "mode": mode_info.mode,
            "environment_id": "default",
            "initial_message": {
                "chat_session_id": "",
                "content": [],
                "query": query,
                "model_name": mode_info.model_name,
                "agent_type": "solo_agent_remote",
                "model_selection_strategy": mode_info.strategy,
                "common_params": common,
            },
            "env": "remote",
            "auto_create_project": false,
            "origin": "web",
        });

        let client = reqwest::Client::builder()
            .timeout(STREAM_TIMEOUT)
            .build()
            .unwrap_or_default();

        let session_url = format!("{base_url}/chat_sessions");
        let res = match client.post(&session_url).headers(headers.clone()).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                let err = json!({
                    "error": { "message": format!("trae create_session failed: {e}"), "type": "api_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(502, &err),
                    url: session_url,
                    headers: HeaderList::new(),
                    transformed_body: body,
                    response_format: None,
                });
            }
        };

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            let err = json!({
                "error": { "message": format!("[{status}] {text}"), "type": "api_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(502, &err),
                url: session_url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        let create_json: Json = res.json().await.unwrap_or(Json::Null);
        if create_json.get("code").and_then(Json::as_i64) != Some(0) {
            let err = json!({
                "error": { "message": format!("Trae create_session: {create_json}"), "type": "api_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(502, &err),
                url: session_url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        let data = create_json.get("data");
        let session_id = data.and_then(|d| d.get("chat_session_id")).and_then(Json::as_str).unwrap_or("");
        let message_id = data.and_then(|d| d.get("message_id")).and_then(Json::as_str).unwrap_or("");

        let events_url = format!(
            "{base_url}/chat_sessions/{session_id}/events?reply_to_message_id={}",
            urlencoding::encode(message_id)
        );

        let events_res = match client.get(&events_url).headers(headers).send().await {
            Ok(r) => r,
            Err(e) => {
                let err = json!({
                    "error": { "message": format!("Trae events stream failed: {e}"), "type": "api_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(502, &err),
                    url: events_url,
                    headers: HeaderList::new(),
                    transformed_body: body,
                    response_format: None,
                });
            }
        };

        let response_id = format!("chatcmpl-trae-{}", chrono::Utc::now().timestamp_millis());
        let created = chrono::Utc::now().timestamp();
        let model = req.model.to_string();

        let text_body = events_res.text().await.unwrap_or_default();

        let mut order: Vec<String> = Vec::new();
        let mut thoughts: HashMap<String, String> = HashMap::new();
        let mut usage: Option<Json> = None;
        let mut error_event: Option<Json> = None;

        let mut current_event: Option<String> = None;
        let mut sse_chunks = String::new();

        if req.stream {
            let init_chunk = json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]
            });
            sse_chunks.push_str(&format!("data: {init_chunk}\n\n"));
        }

        let mut sent_len = 0;

        for line in text_body.lines() {
            let line = line.trim();
            if let Some(ev) = line.strip_prefix("event:") {
                current_event = Some(ev.trim().to_string());
            } else if let Some(data_str) = line.strip_prefix("data:") {
                let payload = data_str.trim();
                let parsed: Json = serde_json::from_str(payload).unwrap_or(Json::Null);

                match current_event.as_deref() {
                    Some("error") => {
                        error_event = Some(parsed);
                        break;
                    }
                    Some("token_usage") => {
                        usage = Some(parsed);
                    }
                    Some("plan_item") => {
                        if let Some(pid) = parsed.get("id").and_then(Json::as_str) {
                            if !thoughts.contains_key(pid) {
                                order.push(pid.to_string());
                            }
                            let t = parsed.get("thought").and_then(Json::as_str).unwrap_or("");
                            let existing = thoughts.get(pid).map(|s| s.as_str()).unwrap_or("");
                            if t.len() >= existing.len() {
                                thoughts.insert(pid.to_string(), t.to_string());
                            }

                            let full: String = order.iter().filter_map(|id| thoughts.get(id)).cloned().collect();
                            if full.len() > sent_len {
                                let piece = &full[sent_len..];
                                sent_len = full.len();

                                if req.stream {
                                    let chunk = json!({
                                        "id": response_id,
                                        "object": "chat.completion.chunk",
                                        "created": created,
                                        "model": model,
                                        "choices": [{ "index": 0, "delta": { "content": piece }, "finish_reason": null }]
                                    });
                                    sse_chunks.push_str(&format!("data: {chunk}\n\n"));
                                }
                            }
                        }
                    }
                    Some("done") => {
                        break;
                    }
                    _ => {}
                }
            } else if line.is_empty() {
                current_event = None;
            }
        }

        if req.stream {
            if let Some(err_ev) = error_event {
                let err_msg = format!("trae {}: {}", err_ev.get("code").and_then(Json::as_str).unwrap_or(""), err_ev.get("message").and_then(Json::as_str).unwrap_or(""));
                let err_chunk = json!({
                    "id": response_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [],
                    "error": { "message": err_msg, "type": "api_error" }
                });
                sse_chunks.push_str(&format!("data: {err_chunk}\n\n"));
            } else {
                let stop_chunk = json!({
                    "id": response_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                });
                sse_chunks.push_str(&format!("data: {stop_chunk}\n\n"));

                if let Some(u) = usage {
                    let usage_chunk = json!({
                        "id": response_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [],
                        "usage": {
                            "prompt_tokens": u.get("prompt_tokens").and_then(Json::as_i64).unwrap_or(0),
                            "completion_tokens": u.get("completion_tokens").and_then(Json::as_i64).unwrap_or(0),
                            "total_tokens": u.get("total_tokens").and_then(Json::as_i64).unwrap_or(0)
                        }
                    });
                    sse_chunks.push_str(&format!("data: {usage_chunk}\n\n"));
                }
            }

            sse_chunks.push_str("data: [DONE]\n\n");
            let bytes = Bytes::from(sse_chunks);
            let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(bytes) }));

            return Ok(ExecResult {
                response: UpstreamResponse::sse(body_stream),
                url: events_url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        // Non-streaming response
        if let Some(err_ev) = error_event {
            let err_msg = format!("trae {}: {}", err_ev.get("code").and_then(Json::as_str).unwrap_or(""), err_ev.get("message").and_then(Json::as_str).unwrap_or(""));
            let err = json!({ "error": { "message": err_msg, "type": "api_error" } });
            return Ok(ExecResult {
                response: UpstreamResponse::json(502, &err),
                url: events_url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        let full_content: String = order.iter().filter_map(|id| thoughts.get(id)).cloned().collect();
        let prompt_tokens = usage.as_ref().and_then(|u| u.get("prompt_tokens")).and_then(Json::as_i64).unwrap_or(0);
        let completion_tokens = usage.as_ref().and_then(|u| u.get("completion_tokens")).and_then(Json::as_i64).unwrap_or(0);

        let completion = json!({
            "id": response_id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": full_content },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        });

        Ok(ExecResult {
            response: UpstreamResponse::json(200, &completion),
            url: events_url,
            headers: HeaderList::new(),
            transformed_body: body,
            response_format: None,
        })
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(TraeExecutor::new(provider))
}

static EMPTY_CONFIG: std::sync::LazyLock<nr_core::Json> =
    std::sync::LazyLock::new(|| nr_core::Json::Object(Default::default()));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trae_executor_creates() {
        let executor = TraeExecutor::new("trae");
        assert_eq!(executor.provider(), "trae");
    }

    #[test]
    fn test_resolve_mode() {
        let work = resolve_mode("work");
        assert_eq!(work.mode, "work");
        assert_eq!(work.strategy, "auto");

        let auto = resolve_mode("auto");
        assert_eq!(auto.mode, "code");
        assert_eq!(auto.strategy, "auto");

        let manual = resolve_mode("claude-3-5-sonnet");
        assert_eq!(manual.mode, "code");
        assert_eq!(manual.strategy, "manual");
        assert_eq!(manual.model_name, "claude-3-5-sonnet");
    }

    #[test]
    fn test_flatten_query() {
        let msgs = json!([
            {"role": "system", "content": "Be concise"},
            {"role": "user", "content": "Hi"}
        ]);
        let query = flatten_query(msgs.as_array().unwrap());
        // The query is a JSON-encoded typed content-block array, so the newline
        // inside the payload is escaped.
        let parsed: Json = serde_json::from_str(&query).unwrap();
        let content = parsed[0]["data"]["content"].as_str().unwrap();
        assert_eq!(content, "[System]\nBe concise\n\nHi");
        assert_eq!(parsed[0]["type"], "text");
    }
}
