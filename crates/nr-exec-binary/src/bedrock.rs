//! Port of open-sse/executors/bedrock.js
//! Bedrock executor for AWS Anthropic models and Grok on Bedrock.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose, Engine};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::StreamExt;
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Json, Result};
use nr_exec_base::base::BaseHooks;

use crate::aws_credentials::{resolve_aws_credentials, resolve_region};
use crate::aws_event_stream::{crc32, parse_event_frame, EventHeaderValue};
use crate::aws_sigv4::{escape_uri, sign_aws_request, AwsCredentials, SigningOptions};

const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
const BEDROCK_SERVICE: &str = "bedrock";

/// Bedrock executor for AWS Anthropic models and Grok on Bedrock.
pub struct BedrockExecutor {
    provider: String,
    config: Json,
    wire_format: String,
    is_claude_wire: bool,
}

impl BedrockExecutor {
    pub fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).cloned().unwrap_or_default();
        let wire_format = config
            .get("format")
            .and_then(Json::as_str)
            .unwrap_or("openai")
            .to_string();
        let is_claude_wire = wire_format == "claude";

        Self {
            provider: provider_id.to_string(),
            config,
            wire_format,
            is_claude_wire,
        }
    }
}

impl BaseHooks for BedrockExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(
        &self,
        model: &str,
        stream: bool,
        _url_index: usize,
        creds: &Credentials,
    ) -> Result<String> {
        let region = resolve_region(creds, None)?;
        let action = if stream { "invoke-with-response-stream" } else { "invoke" };
        let escaped_model = escape_uri(model);
        Ok(format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/{}",
            region, escaped_model, action
        ))
    }

    fn transform_request(
        &self,
        _model: &str,
        body: &Json,
        _stream: bool,
        _creds: &Credentials,
    ) -> Result<Json> {
        let mut transformed = body.clone();
        if let Json::Object(ref mut obj) = transformed {
            obj.remove("model");
            obj.remove("stream");
            if self.is_claude_wire {
                obj.insert("anthropic_version".to_string(), Json::String(ANTHROPIC_VERSION.to_string()));
            }
        }
        Ok(transformed)
    }

    fn build_headers(
        &self,
        _creds: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        if stream {
            headers.insert("Accept".into(), "application/vnd.amazon.eventstream".into());
        } else {
            headers.insert("Accept".into(), "application/json".into());
        }
        Ok(headers)
    }
}

/// Convert AWS EventStream binary chunks to SSE lines (port of JS `eventStreamToSse`).
async fn event_stream_to_sse(
    mut response: UpstreamResponse,
    is_claude_wire: bool,
) -> UpstreamResponse {
    let (mut tx, rx) = mpsc::channel::<std::io::Result<Bytes>>(32);

    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut saw_terminal_event = false;
        let mut failed = false;

        let emit = |controller_tx: &mut mpsc::Sender<std::io::Result<Bytes>>,
                    event_type: Option<&str>,
                    data: &Json| {
            let frame = if is_claude_wire {
                let t = event_type.unwrap_or("message");
                format!("event: {t}\ndata: {data}\n\n")
            } else {
                format!("data: {data}\n\n")
            };
            let _ = controller_tx.try_send(Ok(Bytes::from(frame)));
        };

        let mut fail = |controller_tx: &mut mpsc::Sender<std::io::Result<Bytes>>,
                        err_type: &str,
                        message: &str| {
            if failed {
                return false;
            }
            failed = true;
            let payload = if is_claude_wire {
                serde_json::json!({"type": "error", "error": {"type": err_type, "message": message}})
            } else {
                serde_json::json!({"error": {"type": err_type, "message": message, "code": err_type}})
            };
            emit(controller_tx, Some("error"), &payload);
            false
        };

        while let Some(chunk_res) = response.body.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    fail(&mut tx, "api_error", &format!("Bedrock stream read failed: {e}"));
                    break;
                }
            };
            if chunk.is_empty() {
                continue;
            }
            buffer.extend_from_slice(&chunk);

            // Drain complete EventStream frames
            let mut stop = false;
            while buffer.len() >= 12 {
                let prelude_crc = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);
                if prelude_crc != crc32(&buffer[..8]) {
                    fail(&mut tx, "api_error", "Bedrock EventStream prelude CRC mismatch");
                    stop = true;
                    break;
                }
                let total_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
                if total_length < 16 || total_length > crate::aws_event_stream::MAX_MESSAGE_BYTES {
                    fail(&mut tx, "api_error", "Bedrock EventStream frame bounds are invalid");
                    stop = true;
                    break;
                }
                if buffer.len() < total_length {
                    break; // Wait for rest of frame
                }

                let frame_bytes = buffer[..total_length].to_vec();
                buffer.drain(..total_length);

                let event = match parse_event_frame(&frame_bytes) {
                    Ok(e) => e,
                    Err(e) => {
                        fail(&mut tx, "api_error", &e);
                        stop = true;
                        break;
                    }
                };

                let header_str = |k: &str| -> Option<String> {
                    match event.headers.get(k) {
                        Some(EventHeaderValue::String(s)) => Some(s.clone()),
                        _ => None,
                    }
                };

                let message_type = header_str(":message-type").unwrap_or_default();
                if message_type == "exception" || message_type == "error" {
                    let ex_type = header_str(":exception-type")
                        .or_else(|| header_str(":error-code"))
                        .unwrap_or_else(|| "api_error".to_string());
                    let msg = header_str(":error-message")
                        .unwrap_or_else(|| "Bedrock returned an EventStream error".to_string());
                    fail(&mut tx, &ex_type, &msg);
                    stop = true;
                    break;
                }

                if header_str(":event-type").as_deref() != Some("chunk") {
                    continue;
                }

                // Chunk frame carries {"bytes": "<base64>"}
                let encoded_opt = event.payload.as_ref()
                    .and_then(|p| p.get("bytes"))
                    .and_then(Json::as_str);

                let encoded = match encoded_opt {
                    Some(s) => s,
                    None => {
                        fail(&mut tx, "api_error", "Bedrock chunk frame carried no payload bytes");
                        stop = true;
                        break;
                    }
                };

                let decoded_bytes = match general_purpose::STANDARD.decode(encoded) {
                    Ok(b) => b,
                    Err(e) => {
                        fail(&mut tx, "api_error", &format!("Bedrock chunk was not valid base64 ({e})"));
                        stop = true;
                        break;
                    }
                };

                let inner: Json = match serde_json::from_slice(&decoded_bytes) {
                    Ok(j) => j,
                    Err(e) => {
                        fail(&mut tx, "api_error", &format!("Bedrock chunk was not valid JSON ({e})"));
                        stop = true;
                        break;
                    }
                };

                if is_claude_wire {
                    let ev_type = inner.get("type").and_then(Json::as_str);
                    emit(&mut tx, ev_type, &inner);
                    if ev_type == Some("message_stop") {
                        saw_terminal_event = true;
                    }
                } else {
                    emit(&mut tx, None, &inner);
                    if let Some(choices) = inner.get("choices").and_then(Json::as_array) {
                        if choices.iter().any(|c| c.get("finish_reason").is_some() && !c.get("finish_reason").unwrap().is_null()) {
                            saw_terminal_event = true;
                        }
                    }
                }
            }
            if stop {
                break;
            }
        }

        if !failed && !saw_terminal_event && !is_claude_wire {
            let _ = tx.try_send(Ok(Bytes::from("data: [DONE]\n\n")));
        }
    });

    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
    headers.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    UpstreamResponse::from_stream(200, headers, Box::pin(rx))
}

#[async_trait]
impl nr_core::Executor for BedrockExecutor {
    fn provider(&self) -> &str {
        BaseHooks::provider(self)
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let resolved = resolve_aws_credentials(req.credentials).await?;

        let url = self.build_url(req.model, req.stream, 0, req.credentials)?;
        let transformed_body = self.transform_request(req.model, &req.body, req.stream, req.credentials)?;
        let payload = transformed_body.to_string();

        let mut base_headers = BTreeMap::new();
        base_headers.insert("Content-Type".to_string(), "application/json".to_string());
        if req.stream {
            base_headers.insert("Accept".to_string(), "application/vnd.amazon.eventstream".to_string());
        } else {
            base_headers.insert("Accept".to_string(), "application/json".to_string());
        }

        let signed_headers = sign_aws_request(SigningOptions {
            method: "POST".to_string(),
            url: url.clone(),
            headers: Some(base_headers),
            body: Some(payload.as_bytes().to_vec()),
            region: resolved.region.clone(),
            service: BEDROCK_SERVICE.to_string(),
            credentials: AwsCredentials {
                access_key_id: resolved.access_key_id,
                secret_access_key: resolved.secret_access_key,
                session_token: resolved.session_token,
            },
            double_encode_path: true,
            date: None,
        }).map_err(|e| Error::Provider(format!("Bedrock SigV4 signing failed: {e}")))?;

        let mut headers = HeaderList::new();
        for (k, v) in signed_headers {
            headers.insert(k, v);
        }

        let fetch = FetchRequest {
            header_timeout: Some(self.header_timeout()),
            cancel: Some(req.cancel.clone()),
            ..FetchRequest::post_json(url.clone(), headers.clone(), &transformed_body)
        };

        let response = proxy_fetch(fetch, Some(&req.proxy)).await?;

        // Streaming responses arrive as EventStream framing — decode to SSE
        if req.stream && response.status == 200 {
            let sse_response = event_stream_to_sse(response, self.is_claude_wire).await;
            return Ok(ExecResult {
                response: sse_response,
                url,
                headers,
                transformed_body,
                response_format: None,
            });
        }

        Ok(ExecResult {
            response,
            url,
            headers,
            transformed_body,
            response_format: None,
        })
    }
}

/// Create a Bedrock executor for the given provider ID.
pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(BedrockExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_url_construction() {
        let exec = BedrockExecutor::new("bedrock");
        let creds = Credentials::from_value(serde_json::json!({
            "providerSpecificData": { "region": "us-west-2" }
        }));
        let url = exec.build_url("anthropic.claude-v2", true, 0, &creds).unwrap();
        assert_eq!(
            url,
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-v2/invoke-with-response-stream"
        );
    }

    #[test]
    fn bedrock_transform_request_claude() {
        let exec = BedrockExecutor::new("bedrock");
        let creds = Credentials::new();
        let body = serde_json::json!({ "model": "m", "stream": true, "messages": [] });
        let transformed = exec.transform_request("m", &body, true, &creds).unwrap();
        assert_eq!(transformed.get("anthropic_version").and_then(Json::as_str), Some(ANTHROPIC_VERSION));
        assert!(transformed.get("model").is_none());
        assert!(transformed.get("stream").is_none());
    }
}
