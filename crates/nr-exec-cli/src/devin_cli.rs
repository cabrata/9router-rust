//! Port of open-sse/executors/devin-cli.js.
//!
//! Devin CLI Executor — routes completions through the official Devin CLI binary
//! via ACP (Agent Client Protocol) JSON-RPC 2.0 over stdio.
//!
//! Auth: noAuth — the subprocess inherits the parent env and uses credentials
//! stored by `devin auth login` (~/.local/share/devin/credentials.toml).

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use nr_core::http::{BodyStream, HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Json, Result, Executor};
use nr_exec_base::base::BaseHooks;
use serde_json::json;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Resolve path to devin binary from env, PATH, or known installation locations.
fn resolve_devin_bin() -> String {
    // 1. Explicit override
    if let Ok(bin) = env::var("CLI_DEVIN_BIN") {
        let bin = bin.trim();
        if !bin.is_empty() {
            return bin.to_string();
        }
    }

    let home = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"));
    let is_win = cfg!(windows);

    // 2. Known installer / package-manager locations
    let candidates: Vec<PathBuf> = if is_win {
        vec![
            home.join("AppData/Local/devin/cli/bin/devin.exe"),
            home.join(".local/bin/devin.exe"),
            home.join("scoop/shims/devin.exe"),
            home.join("AppData/Local/Programs/devin/devin.exe"),
        ]
    } else {
        vec![
            home.join(".local/share/devin/bin/devin"),
            home.join(".devin/bin/devin"),
            home.join(".local/bin/devin"),
            PathBuf::from("/opt/homebrew/bin/devin"),
            PathBuf::from("/usr/local/bin/devin"),
            PathBuf::from("/usr/bin/devin"),
        ]
    };

    for path in candidates {
        if path.exists() {
            return path.to_string_lossy().into_owned();
        }
    }

    // 3. Fallback to PATH lookup
    if is_win {
        "devin.exe".to_string()
    } else {
        "devin".to_string()
    }
}

/// Build JSON-RPC 2.0 message.
fn rpc_msg(method: &str, params: Option<&Json>, id: Option<u32>) -> String {
    let mut msg = serde_json::Map::new();
    msg.insert("jsonrpc".into(), json!("2.0"));
    msg.insert("method".into(), json!(method));
    if let Some(params) = params {
        msg.insert("params".into(), params.clone());
    }
    if let Some(id) = id {
        msg.insert("id".into(), json!(id));
    }
    serde_json::to_string(&msg).unwrap_or_default() + "\n"
}

/// Extract text content from messages (flattened for devin's text-only prompt).
fn build_prompt_text(messages: &[Json]) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        if let Some(obj) = msg.as_object() {
            let role = obj.get("role").and_then(Json::as_str).unwrap_or("user");
            let mut text = String::new();

            if let Some(Json::String(s)) = obj.get("content") {
                text = s.clone();
            } else if let Some(arr) = obj.get("content").and_then(Json::as_array) {
                for part in arr {
                    if let Some(obj) = part.as_object() {
                        if obj.get("type").and_then(Json::as_str) == Some("text") {
                            if let Some(Json::String(s)) = obj.get("text") {
                                text.push_str(s);
                            }
                        }
                    }
                }
            }

            if !text.trim().is_empty() {
                let prefix = match role {
                    "system" => "[System]",
                    "assistant" => "[Assistant]",
                    "tool" => "[Tool]",
                    _ => "[User]",
                };
                lines.push(format!("{}\n{}", prefix, text));
            }
        }
    }
    if lines.is_empty() {
        lines.push("(empty)".to_string());
    }
    lines.join("\n\n")
}

/// Devin CLI Executor — routes completions through the official Devin CLI binary.
pub struct DevinCliExecutor {
    provider: String,
    config: Json,
}

impl DevinCliExecutor {
    pub fn new() -> Self {
        let provider = "devin-cli".to_string();
        let config = Json::Object(Default::default());
        Self { provider, config }
    }
}

impl Default for DevinCliExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseHooks for DevinCliExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn no_auth(&self) -> bool {
        true
    }

    fn build_url(&self, _model: &str, _stream: bool, _url_index: usize, _creds: &Credentials) -> Result<String> {
        Ok("devin://acp/stdio".to_string())
    }

    fn build_headers(
        &self,
        _creds: &Credentials,
        _stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        Ok(HeaderList::new())
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        Ok(body.clone())
    }
}

#[async_trait]
impl Executor for DevinCliExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        true
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let model = req.model.to_string();
        let body = req.body.clone();

        let messages = if let Some(arr) = body.get("messages").and_then(Json::as_array) {
            arr.clone()
        } else if let Some(arr) = body.get("input").and_then(Json::as_array) {
            arr.clone()
        } else {
            vec![]
        };

        let prompt_text = build_prompt_text(&messages);
        let devin_bin = resolve_devin_bin();

        req.log.debug("DEVIN", format!("spawning devin acp, model={model}"));

        // Spawn devin subprocess
        let mut child = Command::new(&devin_bin)
            .arg("acp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env_clear()
            .envs(env::vars()) // Inherit parent environment for stored credentials
            .env("DEVIN_PERMISSION_MODE", env::var("DEVIN_PERMISSION_MODE").unwrap_or_else(|_| "bypass".to_string()))
            .spawn()
            .map_err(|e| {
                let msg = if e.to_string().contains("not found") {
                    format!("Devin CLI not found: {devin_bin}. Install via https://cli.devin.ai or set CLI_DEVIN_BIN env var.")
                } else {
                    format!("Devin CLI spawn error: {e}")
                };
                Error::Other(msg)
            })?;

        let mut stdin = child.stdin.take().ok_or_else(|| Error::Other("Failed to open stdin".to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Other("Failed to open stdout".to_string()))?;
        let _stderr = child.stderr.take();

        // Send initialize
        stdin.write_all(rpc_msg("initialize", Some(&json!({
            "protocolVersion": "0.3",
            "clientInfo": { "name": "9router", "version": "1.0" },
            "capabilities": {}
        })), Some(1)).as_bytes()).await.ok();
        stdin.flush().await.ok();

        // Read lines from stdout and build SSE stream
        let buf_reader = BufReader::new(stdout);
        let mut lines = buf_reader.lines();

        let mut init_done = false;
        let mut session_created = false;
        let response_id = format!("chatcmpl-devin-{}", chrono::Utc::now().timestamp_millis());
        let created = chrono::Utc::now().timestamp();
        let mut role_emitted = false;
        let mut total_text = String::new();
        let mut finished = false;
        let mut id_counter = 2u32;
        let mut output_events = String::new();

        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let msg: Json = match serde_json::from_str(line) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Initialize response → send session/new
            if !init_done && msg.get("result").is_some() && msg.get("method").is_none() {
                init_done = true;
                let tmp_dir = std::env::temp_dir();
                let cwd = tmp_dir.to_string_lossy().into_owned();
                stdin.write_all(rpc_msg("session/new", Some(&json!({
                    "cwd": cwd,
                    "mcpServers": [],
                    "model": model
                })), Some(id_counter)).as_bytes()).await.ok();
                stdin.flush().await.ok();
                id_counter += 1;
                continue;
            }

            // session/new response → send session/prompt
            if init_done && !session_created && msg.get("result").is_some() && msg.get("method").is_none() {
                if let Some(obj) = msg.get("result").and_then(Json::as_object) {
                    if let Some(Json::String(sid)) = obj.get("sessionId") {
                        session_created = true;
                        stdin.write_all(rpc_msg("session/prompt", Some(&json!({
                            "sessionId": sid,
                            "prompt": [{ "type": "text", "text": prompt_text }]
                        })), Some(id_counter)).as_bytes()).await.ok();
                        stdin.flush().await.ok();
                        id_counter += 1;
                        continue;
                    }
                }
            }

            // Handle streaming notifications (session/update)
            if msg.get("method").and_then(Json::as_str) == Some("session/update") {
                if let Some(params) = msg.get("params").and_then(Json::as_object) {
                    if let Some(Json::String(text)) = params.get("content") {
                        if !role_emitted {
                            let role_chunk = json!({
                                "id": response_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
                            });
                            output_events.push_str(&format!("data: {}\n\n", role_chunk));
                            role_emitted = true;
                        }
                        total_text.push_str(text);
                        let delta_chunk = json!({
                            "id": response_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                        });
                        output_events.push_str(&format!("data: {}\n\n", delta_chunk));
                    }

                    // Check for stop signals
                    if let Some(Json::String(type_val)) = params.get("type") {
                        if matches!(type_val.as_str(), "message_stop" | "stop" | "done") {
                            finished = true;
                            break;
                        }
                    }
                }
            }

            // Handle agent stopped
            if msg.get("method").and_then(Json::as_str) == Some("_cognition.ai/agent_stopped") {
                finished = true;
                break;
            }
        }

        if !finished && role_emitted {
            let finish_chunk = json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": (prompt_text.len() / 4).max(1),
                    "completion_tokens": (total_text.len() / 4).max(1),
                    "total_tokens": ((prompt_text.len() + total_text.len()) / 4).max(1),
                    "estimated": true
                }
            });
            output_events.push_str(&format!("data: {}\n\n", finish_chunk));
        }

        output_events.push_str("data: [DONE]\n\n");

        // Gracefully close
        let _ = stdin.shutdown().await;
        let _ = child.kill().await;

        // Create SSE stream from events
        let events_bytes = Bytes::from(output_events);
        let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(events_bytes) }));
        let response = UpstreamResponse::sse(body_stream);

        Ok(ExecResult {
            response,
            url: "devin://acp/stdio".to_string(),
            headers: HeaderList::new(),
            transformed_body: body,
            response_format: None,
        })
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(DevinCliExecutor::new())
}
