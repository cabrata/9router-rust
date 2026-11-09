//! Integration tests for the pxpipe / headroom / translator / tunnel / mcp
//! route groups. Each group is exercised through the real router so the
//! registration itself is covered, not just the handlers.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nr_db::Db;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

static TEST_DB: OnceLock<Db> = OnceLock::new();
static TEST_DATA_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

fn test_db() -> Db {
    TEST_DB
        .get_or_init(|| {
            let dir = TempDir::new().expect("create temp dir");
            let leaked = dir.into_path();
            std::env::set_var("DATA_DIR", &leaked);
            let db = Db::open_in(&leaked).expect("open test db");
            let _ = TEST_DATA_DIR.set(leaked.clone());
            // Keep the PXPIPE card deterministic: "not installed, do not auto-install".
            let _ = nr_db::update_settings(&db, json!({ "pxpipeAutoInstall": false }));
            db
        })
        .clone()
}

fn router() -> axum::Router {
    nr_server::routes::routes().layer(axum::Extension(test_db()))
}

async fn call(method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let req = match body {
        Some(b) => req.body(Body::from(serde_json::to_vec(&b).unwrap())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = router().oneshot(req).await.expect("call handler");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 5_000_000)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

// ─── pxpipe ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pxpipe_status_shape() {
    let (status, body) = call("GET", "/api/pxpipe/status", None).await;
    assert_eq!(status, StatusCode::OK);
    // getPxpipeStatus() + the settings echo.
    for key in [
        "installed",
        "installing",
        "version",
        "path",
        "running",
        "loadedAt",
        "uptimeMs",
        "npmAvailable",
        "mode",
        "enabled",
        "autoInstall",
        "minChars",
        "timeoutMs",
    ] {
        assert!(body.get(key).is_some(), "missing {key} in {body}");
    }
    assert_eq!(body["mode"], "library");
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn pxpipe_health_is_unhealthy_when_missing() {
    for method in ["GET", "POST"] {
        let (status, body) = call(method, "/api/pxpipe/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["healthy"], false);
        let checks = body["checks"].as_array().expect("checks array");
        assert_eq!(checks.len(), 1, "only the install check runs");
        assert_eq!(checks[0]["id"], "installed");
        assert_eq!(checks[0]["ok"], false);
        assert_eq!(body["error"], "pxpipe not installed");
    }
}

#[tokio::test]
async fn pxpipe_start_refuses_without_auto_install() {
    let (status, body) = call("POST", "/api/pxpipe/start", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "NOT_INSTALLED");
}

#[tokio::test]
async fn pxpipe_stats_and_logs_shape() {
    let (status, body) = call("GET", "/api/pxpipe/stats?limit=5", None).await;
    assert_eq!(status, StatusCode::OK);
    let windows = body["windows"].as_object().expect("windows");
    for key in ["all", "today", "yesterday", "last7d", "last30d"] {
        assert!(windows.contains_key(key), "missing window {key}");
    }
    assert_eq!(body["timeline"].as_array().unwrap().len(), 30);
    assert!(body["recent"].is_array());

    let (status, body) = call("GET", "/api/pxpipe/logs?limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["installLog"].is_string());
    assert!(body["events"].is_array());
}

#[tokio::test]
async fn pxpipe_stop_merges_status() {
    let (status, body) = call("POST", "/api/pxpipe/stop", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stopped"], false, "nothing was loaded");
    assert_eq!(body["running"], false);
    assert_eq!(body["mode"], "library");
}

// ─── headroom ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn headroom_status_shape() {
    let (status, body) = call("GET", "/api/headroom/status", None).await;
    assert_eq!(status, StatusCode::OK);
    for key in [
        "installed",
        "path",
        "running",
        "python",
        "localUrl",
        "canStart",
        "version",
        "extras",
        "url",
        "managedPid",
    ] {
        assert!(body.get(key).is_some(), "missing {key} in {body}");
    }
    // `settings.headroomUrl || DEFAULT_HEADROOM_URL`; other tests in this file
    // share the settings row, so assert the shape rather than one literal.
    let url = body["url"].as_str().expect("url string");
    assert!(url.starts_with("http://") || url.starts_with("https://"), "{url}");
    assert!(body["extras"].get("code").is_some());
    assert!(body["extras"].get("ml").is_some());
}

#[tokio::test]
async fn headroom_stop_reports_conflict_when_idle() {
    let (status, body) = call("POST", "/api/headroom/stop", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["stopped"], false);
    assert_eq!(body["reason"], "not_running");
}

#[tokio::test]
async fn headroom_extras_shape() {
    let (status, body) = call("GET", "/api/headroom/extras", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], json!(["code", "ml"]));
    assert!(body.get("extras").is_some());
}

// External-URL rejection (EXTERNAL_PROXY) is asserted by the unit test
// `loopback_detection_matches_js`: a settings-mutating integration case would
// race every other test that shares this file's settings row.

// ─── translator ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn translator_translate_step_validation() {
    let (status, body) = call("POST", "/api/translator/translate", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["success"], false);
    assert_eq!(body["error"], "Step and body required");

    let (status, body) = call(
        "POST",
        "/api/translator/translate",
        Some(json!({ "step": 9, "body": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid step (1-3)");
}

#[tokio::test]
async fn translator_translate_step_1_detects_formats() {
    let (status, body) = call(
        "POST",
        "/api/translator/translate",
        Some(json!({
            "step": 1,
            "body": { "body": { "model": "claude/claude-3-5-sonnet", "messages": [] } },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true);
    let result = &body["result"];
    assert!(result.get("provider").is_some());
    assert!(result.get("model").is_some());
    assert!(result.get("sourceFormat").is_some());
    assert!(result.get("targetFormat").is_some());
}

#[tokio::test]
async fn translator_translate_step_3_needs_a_connection() {
    let (status, body) = call(
        "POST",
        "/api/translator/translate",
        Some(json!({
            "step": 3,
            "provider": "openai",
            "model": "gpt-4o",
            "body": { "model": "gpt-4o", "messages": [] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["success"], false);
    assert_eq!(body["error"], "No active connection for provider: openai");
}

#[tokio::test]
async fn translator_translate_step_2_runs_the_real_translator() {
    // claude source → openai intermediate, the first half of the real pipeline.
    let (status, body) = call(
        "POST",
        "/api/translator/translate",
        Some(json!({
            "step": 2,
            "body": {
                "body": {
                    "model": "anthropic/claude-3-5-sonnet",
                    "max_tokens": 32,
                    "system": "be brief",
                    "messages": [{ "role": "user", "content": "hi" }],
                }
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true);
    let translated = &body["result"]["body"];
    // The translator emits OpenAI shape: messages plus a hoisted system turn.
    let messages = translated["messages"].as_array().expect("messages array");
    assert!(messages.iter().any(|m| m["role"] == "system"), "{translated}");
    assert!(messages.iter().any(|m| m["role"] == "user"), "{translated}");
    // `_toolNameMap` is stripped from the response (JS `delete result._toolNameMap`).
    assert!(translated.get("_toolNameMap").is_none(), "{translated}");
    assert_eq!(translated["max_tokens"], 32);
}

#[tokio::test]
async fn translator_send_requires_provider_model_body() {
    let (status, body) = call("POST", "/api/translator/send", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "provider, model, and body required");

    let (status, body) = call(
        "POST",
        "/api/translator/send",
        Some(json!({ "provider": "openai", "model": "gpt-4o", "body": { "messages": [] } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "No active connection for provider: openai");
}

#[tokio::test]
async fn translator_load_and_save_validate_params() {
    let (status, body) = call("GET", "/api/translator/load", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "File parameter required");

    let (status, body) = call("GET", "/api/translator/load?file=../../etc/passwd", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid file name");

    let (status, body) = call(
        "POST",
        "/api/translator/save",
        Some(json!({ "file": "../x.json", "content": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid file name");
}

// ─── tunnel ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tunnel_status_shape() {
    let (status, body) = call("GET", "/api/tunnel/status", None).await;
    assert_eq!(status, StatusCode::OK);
    let tunnel = &body["tunnel"];
    for key in [
        "enabled",
        "settingsEnabled",
        "tunnelUrl",
        "shortId",
        "publicUrl",
        "running",
    ] {
        assert!(tunnel.get(key).is_some(), "missing tunnel.{key} in {body}");
    }
    let tailscale = &body["tailscale"];
    for key in ["enabled", "settingsEnabled", "tunnelUrl", "running", "loggedIn"] {
        assert!(tailscale.get(key).is_some(), "missing tailscale.{key}");
    }
    // Download progress stays live (not part of the 3s probe cache).
    assert!(body["download"].get("downloading").is_some());
    assert!(body["download"].get("progress").is_some());
}

#[tokio::test]
async fn tunnel_tailscale_check_shape() {
    let (status, body) = call("GET", "/api/tunnel/tailscale-check", None).await;
    assert_eq!(status, StatusCode::OK);
    for key in [
        "installed",
        "loggedIn",
        "platform",
        "brewAvailable",
        "daemonRunning",
        "customDaemonRunning",
        "systemDaemonRunning",
        "hasCachedPassword",
    ] {
        assert!(body.get(key).is_some(), "missing {key} in {body}");
    }
}

#[tokio::test]
async fn tunnel_disable_and_tailscale_disable_succeed() {
    let (status, body) = call("POST", "/api/tunnel/disable", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);

    let (status, body) = call("POST", "/api/tunnel/tailscale-disable", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
}

/// The test host has no tailscale binary. `tailscale-enable` must report that
/// instead of faking a tunnel (the JS flow would fall through to the installer).
#[tokio::test]
async fn tunnel_tailscale_enable_reports_missing_binary() {
    if which("tailscale").is_some() {
        return; // developer machine with tailscale installed: not our case
    }
    let (status, body) = call("POST", "/api/tunnel/tailscale-enable", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["error"].as_str().unwrap_or("");
    assert!(message.contains("not installed"), "unexpected error: {body}");
}

/// A disable when nothing runs is still a success (the JS clears state and
/// settings unconditionally).
#[tokio::test]
async fn tunnel_disable_clears_settings() {
    let db = test_db();
    let _ = nr_db::update_settings(&db, json!({ "tunnelEnabled": true, "tunnelUrl": "https://x" }));
    let (status, body) = call("POST", "/api/tunnel/disable", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    let settings = nr_db::get_settings(&db).unwrap();
    assert_eq!(settings["tunnelEnabled"], false);
    assert_eq!(settings["tunnelUrl"], "");
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

// ─── mcp ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_unknown_plugin_is_404() {
    let (status, _) = call("GET", "/api/mcp/definitely-not-a-plugin/sse", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = call(
        "POST",
        "/api/mcp/definitely-not-a-plugin/message",
        Some(json!({ "jsonrpc": "2.0", "id": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Unknown plugin: definitely-not-a-plugin");
}

#[tokio::test]
async fn mcp_message_without_sse_session_reports_bridge_down() {
    // A preset plugin, but no SSE session has spawned the child yet.
    let (status, body) = call(
        "POST",
        "/api/mcp/browsermcp/message",
        Some(json!({ "jsonrpc": "2.0", "id": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "Bridge not running: browsermcp");
}

// ─── version ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn version_routes_still_registered() {
    // `/api/version` ships in api_routes(), which app() merges beside routes().
    let app = nr_server::api_routes().layer(axum::Extension(test_db()));
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("call handler");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.expect("read body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert!(body.get("currentVersion").is_some());
    assert!(body.get("hasUpdate").is_some());
}
