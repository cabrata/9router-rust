//! Request-shaping tests for the CLI executors (devin-cli, grok-cli, perchai).
//!
//! Every case drives the public surface directly — `BaseHooks::build_url` /
//! `build_headers` / `transform_request`, the request builders, or the stream
//! wrappers — with representative `Credentials` fixtures. Nothing here touches
//! the network or spawns a subprocess.
//!
//! Ported intent: open-sse/executors/{devin-cli,grok-cli,perchai}.js and
//! ../9router/tests/unit/*{devin,grok-cli,perchai}*.

use nr_core::{Credentials, Json};
use nr_exec_base::BaseHooks;
use nr_exec_cli::devin_cli::DevinCliExecutor;
use nr_exec_cli::grok_cli::GrokCliExecutor;
use nr_exec_cli::perchai::{
    build_request_body, build_tier_gate_body, ensure_system_prompt_length, is_tier_gate,
    normalize_tool_arguments, parse_error_payload, PerchAiExecutor, PERCH_SURFACE_GATE_CODE,
    PERCH_TIER_GATE_CODE,
};
use serde_json::json;

fn creds(v: serde_json::Value) -> Credentials {
    Credentials::from_value(v)
}

// ─── Devin CLI ──────────────────────────────────────────────────────────────

#[test]
fn devin_cli_is_a_local_transport_with_no_auth() {
    let exec = DevinCliExecutor::new();
    assert_eq!(BaseHooks::provider(&exec), "devin-cli");
    // The "URL" is a marker, not an HTTP endpoint: devin-cli drives a local
    // ACP subprocess over stdio.
    assert_eq!(BaseHooks::build_url(&exec, "devin", true, 0, &Credentials::new()).unwrap(), "devin://acp/stdio");
    assert!(BaseHooks::build_headers(&exec, &Credentials::new(), true, "devin://acp/stdio", "devin", &json!({}))
        .unwrap()
        .is_empty());
    // No credential → no refresh, and chatCore must not try to refresh it.
    assert!(exec.no_auth());
}

#[test]
fn devin_cli_passes_the_body_through_untouched() {
    let exec = DevinCliExecutor::new();
    let body = json!({
        "model": "devin",
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 16
    });
    assert_eq!(BaseHooks::transform_request(&exec, "devin", &body, true, &Credentials::new()).unwrap(), body);
}

// ─── Grok CLI ───────────────────────────────────────────────────────────────

fn grok_creds() -> Credentials {
    creds(json!({
        "accessToken": "xai-token",
        "connectionId": "conn-42",
        "providerSpecificData": { "email": "dev@example.com" }
    }))
}

#[test]
fn grok_cli_url_is_the_responses_endpoint() {
    let exec = GrokCliExecutor::new();
    let url = BaseHooks::build_url(&exec, "grok-4.5", true, 0, &grok_creds()).unwrap();
    assert!(url.contains("responses"), "{url}");
    assert!(url.starts_with("https://"), "{url}");
}

#[test]
fn grok_cli_headers_carry_the_cli_fingerprint_and_turn_index() {
    let exec = GrokCliExecutor::new();
    let body = json!({
        "input": [
            { "type": "message", "role": "user", "content": "one" },
            { "type": "message", "role": "assistant", "content": "two" },
            { "type": "message", "role": "user", "content": "three" }
        ]
    });
    let headers = BaseHooks::build_headers(&exec, &grok_creds(), true, "https://cli-chat-proxy.grok.com/v1/responses", "grok-4.5", &body)
        .unwrap();

    assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer xai-token"));
    // The turn index lets grok-cli pick its reasoning budget per turn.
    assert_eq!(headers.get("x-grok-turn-idx").map(String::as_str), Some("2"));
    // The connection id doubles as the session id so turns stay grouped.
    assert_eq!(headers.get("x-grok-session-id").map(String::as_str), Some("conn-42"));
    assert_eq!(headers.get("x-grok-conv-id").map(String::as_str), Some("conn-42"));
    assert!(headers.get("x-grok-req-id").is_some());
    assert!(headers.get("x-grok-client-identifier").is_some());
    assert!(headers.get("x-grok-client-version").is_some());
}

#[test]
fn grok_cli_transform_builds_a_responses_input_array() {
    let exec = GrokCliExecutor::new();
    let body = json!({
        "messages": [
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "hello" }
        ]
    });
    let out = BaseHooks::transform_request(&exec, "grok-4.5", &body, true, &grok_creds()).unwrap();
    let input = out.get("input").and_then(Json::as_array).expect("responses input array");
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], json!("message"));
    assert_eq!(input[0]["role"], json!("system"));
    assert_eq!(input[1]["content"], json!("hello"));
    // Chat-completions `messages` must not leak into the Responses body.
    assert!(out.get("messages").is_none(), "{out}");
}

// ─── Perch AI ───────────────────────────────────────────────────────────────

#[test]
fn perch_url_is_the_model_call_endpoint() {
    let exec = PerchAiExecutor::new("perchai");
    assert_eq!(
        BaseHooks::build_url(&exec, "standard", true, 0, &Credentials::new()).unwrap(),
        "https://app.perchai.app/api/perch-terminal/model-call"
    );
    // Self-hosted / regional app URLs are honoured, trailing slash included.
    let c = creds(json!({ "providerSpecificData": { "appUrl": "https://eu.perch.example/" } }));
    assert_eq!(
        BaseHooks::build_url(&exec, "standard", true, 0, &c).unwrap(),
        "https://eu.perch.example/api/perch-terminal/model-call"
    );
}

#[test]
fn perch_manual_model_body_routes_by_preferred_model_id() {
    let body = json!({
        "messages": [
            { "role": "system", "content": "sys" },
            { "role": "user", "content": "hi" }
        ],
        "max_tokens": 100,
        "temperature": 0.2,
        "tools": [{ "type": "function", "function": { "name": "f" } }],
        "tool_choice": "auto"
    });
    let payload = build_request_body("perchai/Claude-4", &body, Some("run-1"), true);

    assert_eq!(payload["preferredModelId"], json!("claude-4"));
    assert_eq!(payload["runId"], json!("run-1"));
    assert_eq!(payload["lane"], json!("chat"));
    assert_eq!(payload["clientSurface"], json!("cli"));
    assert_eq!(payload["resendAfterOutput"], json!(true));
    assert_eq!(payload["request"]["maxTokens"], json!(100));
    assert_eq!(payload["request"]["temperature"], json!(0.2));
    assert_eq!(payload["request"]["toolChoice"], json!("auto"));
    // Manual picks never carry a roost selector.
    assert!(payload.get("roostModelChoice").is_none());
    // The surface gate pads the system prompt past 200 chars.
    let system = payload["request"]["messages"][0]["content"].as_str().unwrap();
    assert!(system.chars().count() >= 200, "{}", system.chars().count());
}

#[test]
fn perch_tier_model_body_uses_the_roost_harness() {
    let body = json!({
        "messages": [{ "role": "system", "content": "short" }],
        "max_completion_tokens": 64
    });
    let payload = build_request_body("pro", &body, None, false);
    assert_eq!(payload["preferredModelId"], Json::Null);
    assert_eq!(payload["roostModelChoice"], json!("pro"));
    assert_eq!(payload["roostReasoning"], json!(true));
    assert_eq!(payload["effort"]["level"], json!("high"));
    assert_eq!(payload["request"]["maxTokens"], json!(64));
    // Roost prompts are never padded, and no ticket means no clientSurface.
    assert_eq!(payload["request"]["messages"][0]["content"], json!("short"));
    assert!(payload.get("clientSurface").is_none());
}

#[test]
fn perch_only_pads_an_existing_system_prompt() {
    let without = json!([{ "role": "user", "content": "hi" }]);
    assert_eq!(ensure_system_prompt_length(&without), without);

    let with = json!([
        { "role": "system", "content": "be brief" },
        { "role": "user", "content": "hi" }
    ]);
    let padded = ensure_system_prompt_length(&with);
    assert!(padded[0]["content"].as_str().unwrap().starts_with("be brief\n\n"));
    // Message order (and therefore image indices) is untouched.
    assert_eq!(padded[1], with[1]);
}

#[test]
fn perch_tier_gate_only_fires_on_403_with_the_code() {
    assert!(is_tier_gate(403, &format!("{{\"errorCode\":\"{PERCH_TIER_GATE_CODE}\"}}")));
    assert!(!is_tier_gate(402, PERCH_TIER_GATE_CODE));
    assert!(!is_tier_gate(429, PERCH_TIER_GATE_CODE));
    assert!(!is_tier_gate(403, ""));

    let gate = build_tier_gate_body(None, None);
    assert_eq!(gate["errorCode"], json!(PERCH_TIER_GATE_CODE));
    assert_eq!(gate["limit"]["url"], json!("https://chat.perchai.app"));
    assert_eq!(gate["ok"], json!(false));
}

#[test]
fn perch_error_payload_and_surface_gate_notice() {
    let (error, code, plan) = parse_error_payload(
        "{\"error\":\"pick a plan\",\"limit\":{\"errorCode\":\"tier_selection_required\",\"plan\":{\"id\":\"pro\"}}}",
    );
    assert_eq!(error, "pick a plan");
    assert_eq!(code, PERCH_TIER_GATE_CODE);
    assert_eq!(plan.unwrap()["id"], json!("pro"));
    assert!(PERCH_SURFACE_GATE_CODE.contains("surface"));
}

#[test]
fn perch_tool_arguments_must_be_a_json_object() {
    // `arguments: ""` makes clients reject the call as malformed.
    assert_eq!(normalize_tool_arguments(Some(&json!(""))), "{}");
    assert_eq!(normalize_tool_arguments(None), "{}");
    assert_eq!(normalize_tool_arguments(Some(&json!({"a": 1}))), "{\"a\":1}");
    assert_eq!(normalize_tool_arguments(Some(&json!("{\"a\":1}"))), "{\"a\":1}");
}
