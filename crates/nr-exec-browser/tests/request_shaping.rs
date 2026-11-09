//! Request-shaping tests for the browser/web executors (gemini-web, grok-web,
//! muse-web, veoaifree-web).
//!
//! These executors drive a web session cookie rather than an API token, so the
//! shaping surface is: cookie resolution, prompt flattening, and provider
//! quirks (markup tool calls, aspect-ratio mapping, SSRF guards). Nothing here
//! opens a browser or touches the network.
//!
//! Ported intent: open-sse/executors/{gemini-web,grok-web,muse-web,veoaifree-web}.js.

use nr_core::{Credentials, Json};
use nr_exec_browser::gemini_web::{build_gemini_prompt, parse_cookies, resolve_gemini_web_cookie};
use nr_exec_browser::grok_web::{build_grok_cookie_header, extract_tool_calls_from_markup, parse_openai_messages};
use nr_exec_browser::muse_web::{resolve_muse_cookie, flatten_messages};
use nr_exec_browser::veoaifree_web::{is_private_address, map_aspect_ratio};
use serde_json::json;

fn creds(v: serde_json::Value) -> Credentials {
    Credentials::from_value(v)
}

// ─── Gemini web ─────────────────────────────────────────────────────────────

#[test]
fn gemini_cookie_parsing_drops_browser_attributes() {
    let pairs = parse_cookies("__Secure-1PSID=abc; Path=/; Domain=.google.com; Secure; HttpOnly; NID=xyz");
    // Only the credential cookies survive; the `Path`/`Domain`/`Secure` noise
    // must never be forwarded as cookies.
    assert_eq!(
        pairs,
        vec![("__Secure-1PSID".to_string(), "abc".to_string()), ("NID".to_string(), "xyz".to_string())]
    );
    assert!(parse_cookies("").is_empty());
    assert!(parse_cookies("=novalue; Path=/").is_empty());
}

#[test]
fn gemini_cookie_resolution_prefers_api_key_then_fields() {
    // A pasted raw cookie string wins.
    let c = creds(json!({ "apiKey": "__Secure-1PSID=abc; __Secure-1PSIDTS=ts" }));
    let cookie = resolve_gemini_web_cookie(&c);
    assert!(cookie.contains("__Secure-1PSID=abc"), "{cookie}");
    assert!(cookie.contains("__Secure-1PSIDTS=ts"), "{cookie}");

    // Split fields are joined when no raw cookie is present.
    let c = creds(json!({
        "providerSpecificData": { "__Secure-1PSID": "p1", "__Secure-1PSIDTS": "t1" }
    }));
    let cookie = resolve_gemini_web_cookie(&c);
    assert!(cookie.contains("__Secure-1PSID=p1"), "{cookie}");
    assert!(cookie.contains("__Secure-1PSIDTS=t1"), "{cookie}");

    // No credential at all → empty, so the executor can fail fast.
    assert!(resolve_gemini_web_cookie(&Credentials::new()).is_empty());
}

#[test]
fn gemini_prompt_flattens_multiturn_and_keeps_the_last_user_turn_raw() {
    let messages = vec![
        json!({ "role": "system", "content": "be terse" }),
        json!({ "role": "user", "content": "first question" }),
        json!({ "role": "assistant", "content": "first answer" }),
        json!({ "role": "user", "content": "second question" }),
    ];
    let prompt = build_gemini_prompt(&messages);
    assert!(prompt.contains("first question"), "{prompt}");
    assert!(prompt.contains("first answer"), "{prompt}");
    assert!(prompt.ends_with("second question"), "{prompt}");
    assert!(prompt.contains("be terse"), "{prompt}");
}

#[test]
fn gemini_single_turn_prompt_is_the_message_itself() {
    let messages = vec![json!({ "role": "user", "content": "just this" })];
    assert_eq!(build_gemini_prompt(&messages), "just this");
}

// ─── Grok web ───────────────────────────────────────────────────────────────

#[test]
fn grok_cookie_header_accepts_raw_sso_or_full_cookie() {
    // A bare sso token is wrapped.
    assert_eq!(build_grok_cookie_header("rawtoken"), "sso=rawtoken");
    // A full Cookie header is forwarded untouched.
    assert_eq!(build_grok_cookie_header("sso=a; ct0=b"), "sso=a; ct0=b");
    assert_eq!(build_grok_cookie_header("   "), "");
}

#[test]
fn grok_message_parsing_labels_prior_turns_only() {
    let messages = vec![
        json!({ "role": "system", "content": "sys" }),
        json!({ "role": "user", "content": "older" }),
        json!({ "role": "assistant", "content": "reply" }),
        json!({ "role": "user", "content": "newest" }),
    ];
    let out = parse_openai_messages(&messages);
    // The final user turn is the live prompt; prior turns carry role labels so
    // the web UI reads them as history.
    assert!(out.ends_with("newest"), "{out}");
    assert!(out.contains("system: sys"), "{out}");
    assert!(out.contains("user: older"), "{out}");
    assert!(out.contains("assistant: reply"), "{out}");
    assert!(!out.contains("user: newest"), "{out}");
}

#[test]
fn grok_markup_tool_calls_are_split_out_of_the_text() {
    let text = "before\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_call>\nafter";
    let (cleaned, calls) = extract_tool_calls_from_markup(text);
    assert!(!cleaned.contains("<tool_call>"), "{cleaned}");
    assert!(cleaned.contains("before"), "{cleaned}");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], json!("read"));
    assert!(calls[0]["id"].as_str().unwrap().starts_with("call_"));

    // No markup → text passes through untouched.
    let (same, none) = extract_tool_calls_from_markup("plain answer");
    assert_eq!(same, "plain answer");
    assert!(none.is_empty());
}

// ─── Muse web ───────────────────────────────────────────────────────────────

#[test]
fn muse_cookie_resolution_reads_every_credential_slot() {
    let c = creds(json!({ "apiKey": "sess=1; other=2" }));
    let cookie = resolve_muse_cookie(&c);
    assert!(cookie.contains("sess=1"), "{cookie}");

    let c = creds(json!({ "providerSpecificData": { "cookie": "sess=9" } }));
    assert!(resolve_muse_cookie(&c).contains("sess=9"));

    assert!(resolve_muse_cookie(&Credentials::new()).is_empty());
}

#[test]
fn muse_prompt_flattening_separates_text_and_images() {
    let messages = vec![json!({
        "role": "user",
        "content": [
            { "type": "text", "text": "describe this" },
            { "type": "image_url", "image_url": { "url": "https://cdn/a.png" } }
        ]
    })];
    let (prompt, images) = flatten_messages(&messages);
    assert!(prompt.contains("describe this"), "{prompt}");
    assert_eq!(images, vec!["https://cdn/a.png".to_string()]);
}

// ─── veoaifree web ──────────────────────────────────────────────────────────

#[test]
fn veoaifree_aspect_ratio_mapping() {
    assert_eq!(map_aspect_ratio(Some("9:16"), "VIDEO_ASPECT_RATIO"), "VIDEO_ASPECT_RATIO_PORTRAIT");
    assert_eq!(map_aspect_ratio(Some("portrait"), "VIDEO_ASPECT_RATIO"), "VIDEO_ASPECT_RATIO_PORTRAIT");
    assert_eq!(map_aspect_ratio(Some("1:1"), "VIDEO_ASPECT_RATIO"), "VIDEO_ASPECT_RATIO_SQUARE");
    assert_eq!(map_aspect_ratio(Some("square"), "IMAGE_ASPECT_RATIO"), "IMAGE_ASPECT_RATIO_SQUARE");
    // Default is landscape 16:9.
    assert_eq!(map_aspect_ratio(None, "IMAGE_ASPECT_RATIO"), "IMAGE_ASPECT_RATIO_LANDSCAPE");
}

#[test]
fn veoaifree_blocks_private_artifact_hosts() {
    // The artifact download follows redirects to user-visible URLs, so every
    // hop must be a public host (SSRF guard).
    for private in ["127.0.0.1", "10.1.2.3", "192.168.0.5", "172.16.4.4", "169.254.169.254", "[::1]", "0.0.0.0"] {
        assert!(is_private_address(private), "{private} must be blocked");
    }
    for public in ["8.8.8.8", "1.1.1.1", "203.0.113.9"] {
        assert!(!is_private_address(public), "{public} must be allowed");
    }
}

// ─── provider identity ─────────────────────────────────────────────────────

#[test]
fn browser_executors_report_their_provider() {
    assert_eq!(nr_exec_browser::gemini_web::create("gemini-web").provider(), "gemini-web");
    assert_eq!(nr_exec_browser::grok_web::create("grok-web").provider(), "grok-web");
    assert_eq!(nr_exec_browser::muse_web::create("muse-web").provider(), "muse-web");
    assert_eq!(nr_exec_browser::veoaifree_web::create("veoaifree-web").provider(), "veoaifree-web");
    // veoaifree needs no credential at all.
    assert!(nr_exec_browser::veoaifree_web::create("veoaifree-web").no_auth());
}

#[test]
fn veoaifree_rejects_chat_requests_with_an_actionable_error() {
    use nr_core::{ExecRequest, Json as J};
    use tokio_util::sync::CancellationToken;

    let exec = nr_exec_browser::veoaifree_web::create("veoaifree-web");
    let log = nr_core::Logger::new();
    let mut credentials = Credentials::new();
    let cancel = CancellationToken::new();
    let req = ExecRequest {
        model: "veo-3",
        body: json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        stream: false,
        credentials: &mut credentials,
        provider_session_id: None,
        client_tool: None,
        cancel,
        log: &log,
        proxy: Default::default(),
        provider_overrides: None,
    };

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(exec.execute(req)).expect("media provider returns an error body, not a transport error");
    assert_eq!(result.response.status, 400);
    let body: J = rt.block_on(result.response.json_body()).unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    // Points the caller at the media endpoints instead of failing opaquely.
    assert!(message.contains("/v1/images/generations"), "{message}");
    assert!(message.contains("/v1/videos/generations"), "{message}");
}
