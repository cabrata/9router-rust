//! Port of `src/app/api/translator/{console-logs,console-logs/stream,load,save,translate,send}/route.js`.
//!
//! `translate` drives the real `nr-translator` pipeline (step 2 = source→OpenAI,
//! step 3 = OpenAI→target plus executor URL/headers/body); `send` streams the
//! step-4 body to the provider through `nr-executors`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::console_log_buffer::{self as buffer, Broadcast};

/// `allowedFiles` in both `load` and `save` — the only names we will touch, which
/// keeps `?file=` from becoming a path traversal.
const ALLOWED_FILES: &[&str] = &[
    "1_req_client.json",
    "2_req_source.json",
    "3_req_openai.json",
    "4_req_target.json",
    "5_res_provider.txt",
    "6_res_openai.txt",
    "7_res_client.txt",
    "7_res_client.json",
];

/// `path.join(process.cwd(), "logs", "translator")`.
fn logs_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("logs")
        .join("translator")
}

fn is_allowed(file: &str) -> bool {
    ALLOWED_FILES.contains(&file)
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "success": false, "error": message.into() }))).into_response()
}

/// GET /api/translator/console-logs
pub async fn console_logs_get() -> Response {
    Json(buffer::logs_payload()).into_response()
}

/// DELETE /api/translator/console-logs
pub async fn console_logs_delete() -> Response {
    buffer::clear_logs();
    Json(json!({ "success": true })).into_response()
}

/// GET /api/translator/console-logs/stream
///
/// Sends the buffered backlog as `{ type: "init", logs }`, then `lines` and
/// `clear` frames. Keepalive pings every 25 s match the JS keepalive interval.
pub async fn console_logs_stream() -> Response {
    let backlog = buffer::get_logs();
    let mut rx = buffer::subscribe();

    let initial = if backlog.is_empty() {
        Vec::new()
    } else {
        vec![Ok::<Event, Infallible>(Event::default()
            .data(json!({ "type": "init", "logs": backlog }).to_string()))]
    };

    let stream = async_stream::stream! {
        for ev in initial {
            yield ev;
        }
        loop {
            match rx.recv().await {
                Ok(Broadcast::Lines(lines)) if !lines.is_empty() => {
                    yield Ok(Event::default()
                        .data(json!({ "type": "lines", "lines": lines }).to_string()));
                }
                Ok(Broadcast::Lines(_)) => continue,
                Ok(Broadcast::Clear) => {
                    yield Ok(Event::default().data(json!({ "type": "clear" }).to_string()));
                }
                // Slow subscriber: skip the gap rather than dropping the connection.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
        .into_response()
}

/// GET /api/translator/load?file=...
pub async fn load_get(Query(params): Query<HashMap<String, String>>) -> Response {
    load_from(&logs_dir(), &params)
}

/// POST /api/translator/save — `{ file, content }`.
pub async fn save_post(Json(body): Json<Value>) -> Response {
    save_to(&logs_dir(), &body)
}

fn load_from(dir: &Path, params: &HashMap<String, String>) -> Response {
    let Some(file) = params.get("file").map(String::as_str).filter(|f| !f.is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "File parameter required");
    };
    if !is_allowed(file) {
        return json_error(StatusCode::BAD_REQUEST, "Invalid file name");
    }

    match std::fs::read_to_string(dir.join(file)) {
        Ok(content) => Json(json!({ "success": true, "content": content })).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            json_error(StatusCode::NOT_FOUND, "File not found")
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn save_to(dir: &Path, body: &Value) -> Response {
    let file = body.get("file").and_then(|v| v.as_str()).filter(|f| !f.is_empty());
    let content = body.get("content").and_then(|v| v.as_str());
    let (Some(file), Some(content)) = (file, content) else {
        return json_error(StatusCode::BAD_REQUEST, "File and content required");
    };
    if !is_allowed(file) {
        return json_error(StatusCode::BAD_REQUEST, "Invalid file name");
    }

    if let Err(e) = std::fs::create_dir_all(dir) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    if let Err(e) = std::fs::write(dir.join(file), content) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(json!({ "success": true })).into_response()
}

pub fn routes() -> Router {
    Router::new()
        .route(
            "/api/translator/console-logs",
            get(console_logs_get).delete(console_logs_delete),
        )
        .route("/api/translator/console-logs/stream", get(console_logs_stream))
        .route("/api/translator/load", get(load_get))
        .route("/api/translator/save", post(save_post))
        .route("/api/translator/translate", post(translate_post))
        .route("/api/translator/send", post(send_post))
}

// ─── translate / send ───────────────────────────────────────────────────────

/// `getExecutor(provider)` credentials object built from a connection row.
fn connection_credentials(connection: &Value) -> nr_core::Credentials {
    let psd = connection.get("providerSpecificData").cloned();
    let mut creds = nr_core::Credentials::new();
    for key in [
        "apiKey",
        "accessToken",
        "refreshToken",
        "idToken",
        "lastRefreshAt",
        "projectId",
    ] {
        if let Some(v) = connection.get(key) {
            if !v.is_null() {
                creds.set(key, v.clone());
            }
        }
    }
    creds.set("connectionId", json!(connection.get("id").and_then(Value::as_str).unwrap_or("")));
    if let Some(psd) = psd {
        if let Some(copilot) = psd.get("copilotToken") {
            creds.set("copilotToken", copilot.clone());
        }
        if let Some(expires) = psd.get("copilotTokenExpiresAt") {
            creds.set("copilotTokenExpiresAt", expires.clone());
        }
        creds.set("providerSpecificData", psd);
    }
    creds
}

/// First active connection for a provider (`connections.find(c => c.isActive !== false)`).
fn active_connection(db: &nr_db::Db, provider: &str) -> Option<Value> {
    let filter = json!({ "provider": provider });
    let connections = nr_db::get_provider_connections(db, Some(&filter)).ok()?;
    connections
        .into_iter()
        .find(|c| c.get("isActive").and_then(Value::as_bool) != Some(false))
}

fn translate_error(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "success": false, "error": message.into() })),
    )
        .into_response()
}

/// POST /api/translator/translate — `{ step: 1|2|3, body }`.
pub async fn translate_post(
    ext: Option<axum::extract::Extension<nr_db::Db>>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(step) = payload.get("step").and_then(Value::as_i64) else {
        return translate_error("Step and body required");
    };
    let Some(body) = payload.get("body") else {
        return translate_error("Step and body required");
    };

    let db = match ext {
        Some(axum::extract::Extension(db)) => db,
        None => match nr_db::db() {
            Ok(db) => db,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "success": false, "error": e.to_string() })),
                )
                    .into_response()
            }
        },
    };

    match step {
        // Detect provider + formats from the client body.
        1 => {
            let client_body = body.get("body").unwrap_or(body);
            let model_str = client_body.get("model").and_then(Value::as_str).unwrap_or("");
            let (provider, model) = nr_engine::model::get_model_info(&db, model_str);
            let provider = provider.unwrap_or_default();
            let source_format = nr_common::provider::detect_format(client_body);
            let target_format = nr_common::provider::get_target_format(&provider, None);
            Json(json!({
                "success": true,
                "result": {
                    "provider": provider,
                    "model": model,
                    "sourceFormat": source_format,
                    "targetFormat": target_format,
                },
            }))
            .into_response()
        }
        // source → OpenAI intermediate.
        2 => {
            let client_body = body.get("body").unwrap_or(body).clone();
            let model_str = client_body.get("model").and_then(Value::as_str).unwrap_or("").to_string();
            let (provider, model) = nr_engine::model::get_model_info(&db, &model_str);
            let provider = provider.unwrap_or_default();
            let source_format = nr_common::provider::detect_format(&client_body);
            let stream = client_body.get("stream").and_then(Value::as_bool) != Some(false);

            let mut result = nr_translator::translate_request(
                source_format,
                "openai",
                &model,
                client_body,
                stream,
                None,
                &nr_translator::TranslationOptions::default(),
            );
            if let Some(obj) = result.as_object_mut() {
                obj.shift_remove("_toolNameMap");
            }
            Json(json!({ "success": true, "result": { "body": result, "provider": provider } })).into_response()
        }
        // OpenAI intermediate → target + URL/headers/body.
        3 => {
            let openai_body = body.get("body").unwrap_or(body).clone();
            let provider = payload
                .get("provider")
                .or_else(|| body.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let model = payload
                .get("model")
                .or_else(|| body.get("model"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if provider.is_empty() || model.is_empty() {
                return translate_error("provider and model required");
            }

            let target_format = nr_common::provider::get_target_format(&provider, None);
            let stream = openai_body.get("stream").and_then(Value::as_bool) != Some(false);
            let mut translated = nr_translator::translate_request(
                "openai",
                target_format,
                &model,
                openai_body,
                stream,
                None,
                &nr_translator::TranslationOptions::default(),
            );
            if let Some(obj) = translated.as_object_mut() {
                obj.shift_remove("_toolNameMap");
            }

            let Some(connection) = active_connection(&db, &provider) else {
                return translate_error(format!("No active connection for provider: {provider}"));
            };
            let credentials = connection_credentials(&connection);
            let executor = nr_executors::get_executor(&provider);

            // `buildUrl` / `buildHeaders` / `transformRequest` live on the
            // executor's BaseHooks impl, which the Executor trait does not
            // expose; the default executor is reconstructible for it.
            let default_exec = nr_exec_base::DefaultExecutor::new(&provider);
            use nr_exec_base::BaseHooks;
            let url = match default_exec.build_url(&model, stream, 0, &credentials) {
                Ok(u) => u,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "success": false, "error": e.to_string() })),
                    )
                        .into_response()
                }
            };
            let headers = default_exec
                .build_headers(&credentials, stream, &url, &model, &translated)
                .unwrap_or_default();
            let final_body = default_exec
                .transform_request(&model, &translated, stream, &credentials)
                .unwrap_or(translated);
            let _ = executor;

            Json(json!({
                "success": true,
                "result": { "url": url, "headers": headers, "body": final_body },
            }))
            .into_response()
        }
        _ => translate_error("Invalid step (1-3)"),
    }
}

/// POST /api/translator/send — `{ provider, model, body }` streams the upstream.
pub async fn send_post(
    ext: Option<axum::extract::Extension<nr_db::Db>>,
    Json(payload): Json<Value>,
) -> Response {
    let provider = payload.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
    let model = payload.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(body) = payload.get("body").filter(|b| !b.is_null()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "error": "provider, model, and body required" })),
        )
            .into_response();
    };
    if provider.is_empty() || model.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "error": "provider, model, and body required" })),
        )
            .into_response();
    }
    let body = body.clone();

    let db = match ext {
        Some(axum::extract::Extension(db)) => db,
        None => match nr_db::db() {
            Ok(db) => db,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "success": false, "error": e.to_string() })),
                )
                    .into_response()
            }
        },
    };

    let Some(connection) = active_connection(&db, &provider) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "error": format!("No active connection for provider: {provider}") })),
        )
            .into_response();
    };

    let mut credentials = connection_credentials(&connection);
    let stream = body.get("stream").and_then(Value::as_bool) != Some(false);
    // The debug endpoint sends the body as-is: the caller already translated it
    // in step 3 and the JS route never touches it again.
    let translated = body;

    let executor = nr_executors::get_executor(&provider);
    let log = nr_core::Logger::new();
    let response = execute_once(&executor, &model, translated.clone(), stream, &mut credentials, &log).await;

    let response = match response {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "success": false, "error": e.to_string() })),
            )
                .into_response()
        }
    };

    // Auto-refresh token on 401/403 and retry (same as chatCore).
    let response = if response.status == 401 || response.status == 403 {
        match executor.refresh_credentials(&credentials, &log, None).await {
            Ok(Some(patch)) => {
                credentials.merge(&patch);
                if let Some(id) = connection.get("id").and_then(Value::as_str) {
                    let _ = nr_db::update_provider_connection(&db, id, &Value::Object(patch));
                }
                match execute_once(&executor, &model, translated, stream, &mut credentials, &log).await {
                    Ok(r) => r,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "success": false, "error": e.to_string() })),
                        )
                            .into_response()
                    }
                }
            }
            _ => response,
        }
    } else {
        response
    };

    if !response.ok() {
        let status = response.status;
        let details = response.text().await.unwrap_or_default();
        let trimmed: String = details.chars().take(500).collect();
        tracing::error!("[Translator] Provider error {status}: {trimmed}");
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return (
            code,
            Json(json!({
                "success": false,
                "error": format!("Provider error: {status}"),
                "details": details,
            })),
        )
            .into_response();
    }

    let mut resp = Response::new(axum::body::Body::from_stream(response.body));
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-cache"));
    headers.insert(axum::http::header::CONNECTION, axum::http::HeaderValue::from_static("keep-alive"));
    resp
}

async fn execute_once(
    executor: &std::sync::Arc<dyn nr_core::Executor>,
    model: &str,
    body: Value,
    stream: bool,
    credentials: &mut nr_core::Credentials,
    log: &nr_core::Logger,
) -> nr_core::Result<nr_core::http::UpstreamResponse> {
    let proxy = nr_core::http::ProxyOptions::from_credentials(credentials);
    let req = nr_core::ExecRequest {
        model,
        body,
        stream,
        credentials,
        provider_session_id: None,
        client_tool: None,
        cancel: tokio_util::sync::CancellationToken::new(),
        log,
        proxy,
        provider_overrides: None,
    };
    Ok(executor.execute(req).await?.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_and_traversal_names() {
        assert!(!is_allowed("../settings.json"));
        assert!(!is_allowed("1_req_client.json.bak"));
        assert!(!is_allowed(""));
        assert!(is_allowed("1_req_client.json"));
        assert!(is_allowed("7_res_client.txt"));
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let resp = save_to(
            dir.path(),
            &json!({ "file": "1_req_client.json", "content": "{\"a\":1}" }),
        );
        assert_eq!(resp.status(), StatusCode::OK);

        let mut params = HashMap::new();
        params.insert("file".to_string(), "1_req_client.json".to_string());
        assert_eq!(load_from(dir.path(), &params).status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn save_requires_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let resp = save_to(dir.path(), &json!({ "file": "1_req_client.json" }));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn save_rejects_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let resp = save_to(dir.path(), &json!({ "file": "../x.json", "content": "x" }));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn load_reports_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut params = HashMap::new();
        params.insert("file".to_string(), "2_req_source.json".to_string());
        assert_eq!(load_from(dir.path(), &params).status(), StatusCode::NOT_FOUND);
    }
}
