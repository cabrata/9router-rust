//! Request-shaping tests for the specialized executors: `build_url`,
//! `build_headers` and `transform_request` per provider, driven from
//! representative `Credentials` fixtures. No network access.
//!
//! Ported intent: open-sse/executors/*.js for codex, github, commandcode,
//! codebuddy, gemini-cli, iflow, ollama-local and kimchi.

use nr_core::Credentials;
use nr_exec_base::BaseHooks;
use nr_exec_simple::codebuddy_cn::CodeBuddyCnExecutor;
use nr_exec_simple::codebuddy_intl::CodeBuddyIntlExecutor;
use nr_exec_simple::codex::CodexExecutor;
use nr_exec_simple::commandcode::CommandCodeExecutor;
use nr_exec_simple::github::GithubExecutor;
use nr_exec_simple::kimchi::strip_reasoning_content;
use serde_json::json;

fn creds(v: serde_json::Value) -> Credentials {
    Credentials::from_value(v)
}

// ─── GitHub Copilot ─────────────────────────────────────────────────────────

#[test]
fn github_routes_claude_to_messages_and_others_to_chat() {
    let exec = GithubExecutor::new();
    let c = Credentials::new();

    let claude = exec.build_url("claude-sonnet-4", false, 0, &c).unwrap();
    assert!(claude.contains("messages"), "claude must use /v1/messages: {claude}");

    let gpt = exec.build_url("gpt-4o", false, 0, &c).unwrap();
    assert!(!gpt.contains("/messages"), "non-claude must not use the messages endpoint: {gpt}");
    assert!(gpt.contains("chat/completions") || gpt.contains("responses"), "{gpt}");
}

#[test]
fn github_headers_prefer_copilot_token_then_access_token() {
    let exec = GithubExecutor::new();
    let body = json!({});

    let with_copilot = creds(json!({
        "accessToken": "gho_user",
        "providerSpecificData": { "copilotToken": "copilot-session-tok" }
    }));
    let h = exec.build_headers(&with_copilot, false, "https://x", "gpt-4o", &body).unwrap();
    assert_eq!(h["Authorization"], "Bearer copilot-session-tok");
    assert_eq!(h["copilot-integration-id"], "vscode-chat");
    assert_eq!(h["anthropic-version"], "2023-06-01");
    // A per-request id keeps concurrent streams distinguishable.
    assert_eq!(h["x-request-id"].len(), 36);
    assert_eq!(h["X-Initiator"], "user");

    let oauth_only = creds(json!({"accessToken": "gho_fallback"}));
    let h = exec.build_headers(&oauth_only, true, "https://x", "gpt-4o", &body).unwrap();
    assert_eq!(h["Authorization"], "Bearer gho_fallback");
}

#[test]
fn github_moves_max_tokens_only_for_reasoning_models() {
    let exec = GithubExecutor::new();
    let c = Credentials::new();
    let body = json!({"max_tokens": 512, "messages": []});

    let new_model = exec.transform_request("gpt-5-codex", &body, false, &c).unwrap();
    assert_eq!(new_model["max_completion_tokens"], 512);
    assert!(new_model.get("max_tokens").is_none(), "old key removed");

    // A classic model keeps max_tokens, since it rejects the new name.
    let old_model = exec.transform_request("gpt-4o", &body, false, &c).unwrap();
    assert_eq!(old_model["max_tokens"], 512);
    assert!(old_model.get("max_completion_tokens").is_none());
}

#[test]
fn github_strips_reasoning_effort_none() {
    let exec = GithubExecutor::new();
    let c = Credentials::new();
    let body = json!({"reasoning_effort": "none", "messages": []});
    let out = exec.transform_request("gpt-4o", &body, false, &c).unwrap();
    assert!(out.get("reasoning_effort").is_none(), "models without it reject the value");

    let kept = json!({"reasoning_effort": "high", "messages": []});
    let out = exec.transform_request("gpt-4o", &kept, false, &c).unwrap();
    assert_eq!(out["reasoning_effort"], "high");
}

// ─── Codex (Responses API) ──────────────────────────────────────────────────

#[test]
fn codex_converts_system_role_to_developer() {
    let exec = CodexExecutor::new();
    let c = Credentials::new();
    let body = json!({
        "input": [
            { "role": "system", "content": "be terse" },
            { "role": "user", "content": "hi" }
        ]
    });
    let out = exec.transform_request("gpt-5", &body, false, &c).unwrap();
    assert_eq!(out["input"][0]["role"], "developer", "system → developer for cache prefix");
    assert_eq!(out["input"][1]["role"], "user", "user role untouched");
}

#[test]
fn codex_drops_server_generated_item_references() {
    let exec = CodexExecutor::new();
    let c = Credentials::new();
    let body = json!({
        "input": [
            "msg_abc123",
            "keep-this-plain-string",
            { "type": "item_reference", "id": "rs_1" },
            { "role": "user", "content": "hi" }
        ]
    });
    let out = exec.transform_request("gpt-5", &body, false, &c).unwrap();
    let items = out["input"].as_array().unwrap();
    assert_eq!(items.len(), 2, "server ids and item_reference dropped: {out}");
    assert_eq!(items[0], "keep-this-plain-string");
    assert_eq!(items[1]["role"], "user");
}

#[test]
fn codex_strips_unicode_property_escapes_from_tool_schemas() {
    let exec = CodexExecutor::new();
    let c = Credentials::new();
    let body = json!({
        "input": [],
        "tools": [{
            "type": "function",
            "name": "f",
            "parameters": {
                "type": "object",
                "properties": {
                    "a": { "type": "string", "pattern": "\\p{Cc}+" },
                    "b": { "type": "string", "pattern": "^[a-z]+$" }
                }
            }
        }]
    });
    let out = exec.transform_request("gpt-5", &body, false, &c).unwrap();
    let props = &out["tools"][0]["parameters"]["properties"];
    assert!(
        props["a"].get("pattern").is_none(),
        "\\p{{...}} is rejected by Codex"
    );
    assert_eq!(props["b"]["pattern"], "^[a-z]+$", "plain patterns survive");
}

#[test]
fn codex_forces_stream_and_drops_unsupported_params() {
    let exec = CodexExecutor::new();
    let c = Credentials::new();
    let body = json!({
        "input": [],
        "temperature": 0.5,
        "top_p": 0.9,
        "max_tokens": 100,
        "max_output_tokens": 200,
        "user": "u",
        "stream": false,
        "store": false
    });
    let out = exec.transform_request("gpt-5", &body, false, &c).unwrap();
    assert_eq!(out["stream"], true, "Codex API requires streaming");
    assert_eq!(out["store"], false, "store is preserved");
    for key in ["temperature", "top_p", "max_tokens", "max_output_tokens", "user"] {
        assert!(out.get(key).is_none(), "{key} must be stripped");
    }
}

#[test]
fn codex_session_header_prefers_body_key_then_connection() {
    let exec = CodexExecutor::new();

    let with_body_key = creds(json!({"connectionId": "conn-1"}));
    let body = json!({"prompt_cache_key": "body-session"});
    let h = exec.build_headers(&with_body_key, false, "https://x", "gpt-5", &body).unwrap();
    assert_eq!(h["session_id"], "body-session");
    assert_eq!(h["originator"], "codex_cli_rs");

    // No body hint: fall back to the connection id so caching stays stable.
    let h = exec.build_headers(&with_body_key, false, "https://x", "gpt-5", &json!({})).unwrap();
    assert_eq!(h["session_id"], "conn-1");
}

#[test]
fn codex_account_header_is_optional() {
    let exec = CodexExecutor::new();
    let with = creds(json!({"providerSpecificData": {"workspaceId": "ws-42"}}));
    let h = exec.build_headers(&with, false, "https://x", "gpt-5", &json!({})).unwrap();
    assert_eq!(h["ChatGPT-Account-ID"], "ws-42");

    let without = Credentials::new();
    let h = exec.build_headers(&without, false, "https://x", "gpt-5", &json!({})).unwrap();
    assert!(h.get("ChatGPT-Account-ID").is_none(), "no workspace → no header");
}

// ─── CommandCode ────────────────────────────────────────────────────────────

#[test]
fn commandcode_forces_stream_in_body_and_params() {
    let exec = CommandCodeExecutor::new();
    let c = creds(json!({"apiKey": "cc-key"}));
    let body = json!({"stream": false, "params": {"stream": false}, "messages": []});
    let out = exec.transform_request("m", &body, false, &c).unwrap();
    assert_eq!(out["stream"], true);
    assert_eq!(out["params"]["stream"], true, "nested params also forced");
}

#[test]
fn commandcode_headers_carry_bearer_and_session() {
    let exec = CommandCodeExecutor::new();
    let c = creds(json!({"apiKey": "cc-key"}));
    let body = json!({"threadId": "thread-9"});
    let h = exec.build_headers(&c, true, "https://x", "m", &body).unwrap();
    assert_eq!(h["Authorization"], "Bearer cc-key");
    assert_eq!(h["Content-Type"], "application/json");
    assert_eq!(h["x-session-id"], "thread-9", "thread id is the session key");
}

// ─── CodeBuddy CN / Intl ────────────────────────────────────────────────────

#[test]
fn codebuddy_cn_forces_stream_and_maps_reasoning() {
    let exec = CodeBuddyCnExecutor::new();
    let c = creds(json!({"apiKey": "cb-key"}));

    let body = json!({"stream": false, "reasoning_effort": "high", "messages": []});
    let out = exec.transform_request("hunyuan", &body, false, &c).unwrap();
    assert_eq!(out["stream"], true, "Tencent rejects non-stream requests");
    assert_eq!(out["reasoning_summary"], "auto", "reasoning needs the summary flag");

    let off = json!({"reasoning_effort": "none", "messages": []});
    let out = exec.transform_request("hunyuan", &off, false, &c).unwrap();
    assert!(out.get("reasoning_effort").is_none(), "off means removed");
    assert!(out.get("reasoning_summary").is_none(), "no summary when reasoning is off");
}

#[test]
fn codebuddy_cn_sets_conversation_header_family() {
    let exec = CodeBuddyCnExecutor::new();
    let c = creds(json!({"apiKey": "cb-key"}));
    let h = exec.build_headers(&c, true, "https://x", "hunyuan", &json!({})).unwrap();

    let req_id = h["X-Request-ID"].clone();
    assert_eq!(req_id.len(), 36);
    assert_eq!(h["X-Conversation-Request-ID"], req_id, "request id is shared");
    assert_eq!(h["X-Conversation-ID"].len(), 36);
    assert_eq!(h["X-Conversation-Message-ID"], req_id);
    // The base headers still apply.
    assert_eq!(h["Authorization"], "Bearer cb-key");
}

#[test]
fn codebuddy_intl_forces_stream_and_reasoning_summary() {
    let exec = CodeBuddyIntlExecutor::new();
    let c = creds(json!({"apiKey": "cb-key"}));
    let body = json!({"stream": false, "reasoning_effort": "medium", "messages": []});
    let out = exec.transform_request("claude-sonnet-4", &body, false, &c).unwrap();
    assert_eq!(out["stream"], true);
    assert_eq!(out["reasoning_summary"], "auto");

    let off = json!({"reasoning_effort": "off", "messages": []});
    let out = exec.transform_request("claude-sonnet-4", &off, false, &c).unwrap();
    assert!(out.get("reasoning_effort").is_none());
    assert!(out.get("reasoning_summary").is_none());
}

// ─── Kimchi ─────────────────────────────────────────────────────────────────

#[test]
fn kimchi_strip_reasoning_content_removes_the_field() {
    let mut body = json!({
        "messages": [
            { "role": "assistant", "content": "hi", "reasoning_content": "thinking..." },
            { "role": "user", "content": "next" }
        ]
    });
    strip_reasoning_content(&mut body);
    assert!(body["messages"][0].get("reasoning_content").is_none());
    assert_eq!(body["messages"][0]["content"], "hi", "the visible content stays");
    assert_eq!(body["messages"][1]["content"], "next");
}

#[test]
fn kimchi_strip_reasoning_content_is_a_noop_without_the_field() {
    let mut body = json!({"messages": [{"role": "user", "content": "x"}]});
    let before = body.clone();
    strip_reasoning_content(&mut body);
    assert_eq!(body, before);
    // A non-object body must not panic.
    let mut scalar = json!("not an object");
    strip_reasoning_content(&mut scalar);
    assert_eq!(scalar, json!("not an object"));
}
