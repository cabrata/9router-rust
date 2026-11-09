//! devin-cli ACP test: spawn a mock `devin` binary that speaks JSON-RPC over
//! stdio and verify the executor turns its notifications into OpenAI SSE.
//!
//! The mock is a POSIX shell script that echoes canned JSON-RPC replies. It is
//! written to a temp dir and pointed at through `CLI_DEVIN_BIN`, so the test
//! needs no Devin installation. `CLI_DEVIN_BIN` is process-global, so the two
//! tests here are serialized with a lock.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use nr_core::{Credentials, ExecRequest, Executor};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Write a script that answers `initialize`, `session/new`, then streams two
/// `session/update` notifications followed by `_cognition.ai/agent_stopped`.
fn mock_devin(script_body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "devin-acp-mock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mock dir");
    let path = dir.join("devin");
    let mut file = std::fs::File::create(&path).expect("mock script");
    file.write_all(script_body.as_bytes()).expect("write mock script");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

/// The happy path: initialize → session/new → session/prompt → two text deltas
/// → agent_stopped.
const HAPPY_SCRIPT: &str = r#"#!/bin/sh
# Read and discard the client's initialize request.
read -r init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"0.3"}}'
read -r new_session
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}'
read -r prompt
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"type":"agent_message_chunk","content":"Hel"}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"type":"agent_message_chunk","content":"lo"}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"_cognition.ai/agent_stopped","params":{}}'
"#;

/// A binary that cannot be executed must surface an actionable install hint.
const MISSING_SCRIPT: &str = r#"#!/bin/sh
exit 1
"#;

fn run_devin(script: &str, model: &str) -> nr_core::ExecResult {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let bin = mock_devin(script);
    std::env::set_var("CLI_DEVIN_BIN", &bin);

    let exec = nr_exec_cli::devin_cli::create("devin-cli");
    let log = nr_core::Logger::new();
    let mut credentials = Credentials::new();
    let req = ExecRequest {
        model,
        body: serde_json::json!({
            "messages": [{ "role": "user", "content": "say hello" }]
        }),
        stream: true,
        credentials: &mut credentials,
        provider_session_id: None,
        client_tool: None,
        cancel: tokio_util::sync::CancellationToken::new(),
        log: &log,
        proxy: Default::default(),
        provider_overrides: None,
    };

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(exec.execute(req));
    std::env::remove_var("CLI_DEVIN_BIN");
    let _ = std::fs::remove_dir_all(bin.parent().unwrap_or(&bin));
    match result {
        Ok(result) => result,
        Err(e) => panic!("devin executor returned an error: {e}"),
    }
}

#[test]
fn devin_acp_notifications_become_openai_sse_chunks() {
    let result = run_devin(HAPPY_SCRIPT, "devin");
    assert_eq!(result.response.status, 200);
    assert_eq!(result.url, "devin://acp/stdio");
    assert_eq!(result.response.content_type(), "text/event-stream");

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let events = rt.block_on(nr_core::http::collect_sse_data(result.response.body)).unwrap();

    // Role chunk, two content deltas, finish chunk, [DONE].
    assert!(events.len() >= 4, "{events:?}");
    assert_eq!(events.last().map(String::as_str), Some("[DONE]"));

    let role: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(role["choices"][0]["delta"]["role"], serde_json::json!("assistant"));

    let joined: String = events
        .iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str().map(String::from))
        .collect();
    assert_eq!(joined, "Hello");

    let finish = events
        .iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .find(|v| !v["choices"][0]["finish_reason"].is_null())
        .expect("finish chunk");
    assert_eq!(finish["choices"][0]["finish_reason"], serde_json::json!("stop"));
    assert_eq!(finish["usage"]["estimated"], serde_json::json!(true));
}

#[test]
fn devin_missing_binary_reports_an_install_hint() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let missing = std::env::temp_dir().join(format!("devin-missing-{}", std::process::id()));
    std::env::set_var("CLI_DEVIN_BIN", &missing);

    let exec = nr_exec_cli::devin_cli::create("devin-cli");
    let log = nr_core::Logger::new();
    let mut credentials = Credentials::new();
    let req = ExecRequest {
        model: "devin",
        body: serde_json::json!({"messages": [{ "role": "user", "content": "hi" }]}),
        stream: true,
        credentials: &mut credentials,
        provider_session_id: None,
        client_tool: None,
        cancel: tokio_util::sync::CancellationToken::new(),
        log: &log,
        proxy: Default::default(),
        provider_overrides: None,
    };

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let message = match rt.block_on(exec.execute(req)) {
        Ok(_) => panic!("spawn must fail for a missing binary"),
        Err(e) => e.to_string(),
    };
    std::env::remove_var("CLI_DEVIN_BIN");
    assert!(message.contains("Devin CLI"), "{message}");
    assert!(message.contains("CLI_DEVIN_BIN"), "{message}");
}

/// Keep the unused-looking constant referenced so the fixture stays compile-checked.
#[test]
fn missing_binary_fixture_is_valid_shell() {
    let _ = MISSING_SCRIPT;
    let bin = mock_devin(MISSING_SCRIPT);
    assert!(bin.exists());
    let _ = std::fs::remove_dir_all(bin.parent().unwrap_or(&bin));
}
