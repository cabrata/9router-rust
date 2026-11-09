//! Request-shaping tests for the binary executors (bedrock, kiro, cursor).
//!
//! Every case drives the public `BaseHooks` surface directly
//! (`build_url` / `build_headers` / `transform_request`) with representative
//! `Credentials` fixtures, so nothing here touches the network.
//!
//! Ported intent: open-sse/executors/{bedrock,kiro,cursor}.js and
//! ../9router/tests/unit/*{bedrock,kiro,cursor}*.

use std::collections::BTreeMap;

use nr_core::Credentials;
use nr_exec_base::BaseHooks;
use nr_exec_binary::aws_event_stream::{
    encode_event_frame, parse_event_frame, parse_event_frame_with_limits, EventHeaderValue,
};
use nr_exec_binary::aws_sigv4::{escape_uri, sign_aws_request, AwsCredentials, SigningOptions};
use nr_exec_binary::bedrock::BedrockExecutor;
use nr_exec_binary::cursor::CursorExecutor;
use nr_exec_binary::cursor_checksum::{
    build_cursor_headers, generate_cursor_checksum, generate_hashed64_hex, generate_session_id,
};
use nr_exec_binary::kiro::KiroExecutor;
use serde_json::json;

fn creds(v: serde_json::Value) -> Credentials {
    Credentials::from_value(v)
}

// ─── Bedrock ────────────────────────────────────────────────────────────────

#[test]
fn bedrock_url_uses_region_and_stream_action() {
    let exec = BedrockExecutor::new("bedrock");
    let c = creds(json!({"providerSpecificData": {"region": "eu-central-1"}}));

    assert_eq!(
        exec.build_url("anthropic.claude-sonnet-4", false, 0, &c).unwrap(),
        "https://bedrock-runtime.eu-central-1.amazonaws.com/model/anthropic.claude-sonnet-4/invoke"
    );
    assert_eq!(
        exec.build_url("anthropic.claude-sonnet-4", true, 0, &c).unwrap(),
        "https://bedrock-runtime.eu-central-1.amazonaws.com/model/anthropic.claude-sonnet-4/invoke-with-response-stream"
    );
}

#[test]
fn bedrock_url_escapes_model_id_and_defaults_region() {
    let exec = BedrockExecutor::new("bedrock");
    let c = Credentials::new();
    let url = exec.build_url("vendor/model:v1", false, 0, &c).unwrap();
    // The slash and colon must be percent-encoded or the path segments break.
    assert!(url.contains("/model/vendor%2Fmodel%3Av1/invoke"), "{url}");
    assert!(url.starts_with("https://bedrock-runtime.us-east-1.amazonaws.com"), "{url}");
}

#[test]
fn bedrock_rejects_invalid_region() {
    let exec = BedrockExecutor::new("bedrock");
    // Region is interpolated into the hostname, so anything with a dot or slash
    // must not reach build_url.
    for bad in ["evil.com/", "us east 1", "US-EAST-1", ""] {
        let c = creds(json!({"providerSpecificData": {"region": bad}}));
        let url = exec.build_url("m", false, 0, &c).unwrap();
        assert!(
            url.starts_with("https://bedrock-runtime.us-east-1.amazonaws.com"),
            "region {bad:?} should have been rejected, got {url}"
        );
    }
}

#[test]
fn bedrock_transform_keys_by_wire_format() {
    let body = json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]});

    // Claude-wire providers get anthropic_version and drop model/stream.
    let claude = BedrockExecutor::new("bedrock");
    let out = claude.transform_request("m", &body, true, &Credentials::new()).unwrap();
    assert_eq!(out["anthropic_version"], json!("bedrock-2023-05-31"));
    assert!(out.get("model").is_none());
    assert!(out.get("stream").is_none());
    assert_eq!(out["messages"], body["messages"]);

    // Non-claude wire (e.g. grok on bedrock) keeps the body minus model/stream.
    let other = BedrockExecutor::new("bedrock-grok");
    let out = other.transform_request("m", &body, true, &Credentials::new()).unwrap();
    assert!(out.get("anthropic_version").is_none());
    assert!(out.get("model").is_none());
    assert!(out.get("stream").is_none(), "stream is carried by the URL action");
}

#[test]
fn bedrock_accept_header_matches_streaming_mode() {
    let exec = BedrockExecutor::new("bedrock");
    let c = Credentials::new();
    let body = json!({});

    let plain = exec.build_headers(&c, false, "https://x", "m", &body).unwrap();
    assert_eq!(plain["Content-Type"], "application/json");
    assert_eq!(plain["Accept"], "application/json");

    let sse = exec.build_headers(&c, true, "https://x", "m", &body).unwrap();
    assert_eq!(sse["Accept"], "application/vnd.amazon.eventstream");
}

// ─── Kiro ───────────────────────────────────────────────────────────────────

#[test]
fn kiro_api_key_auth_sets_bearer_and_token_type() {
    let exec = KiroExecutor::new("kiro");
    let c = creds(json!({
        "accessToken": "tok-123",
        "providerSpecificData": {"authMethod": "api_key", "apiKey": "kiro-key"}
    }));
    let h = exec.build_headers(&c, false, "https://q.us-east-1.amazonaws.com", "m", &json!({})).unwrap();

    assert_eq!(h["Authorization"], "Bearer kiro-key", "psd apiKey wins over accessToken");
    assert_eq!(h["TokenType"], "API_KEY");
    assert_eq!(h["x-amz-sso-bearer"], "tok-123");
    assert_eq!(h["Amz-Sdk-Request"], "attempt=1; max=3");
    // A fresh invocation id per request keeps AWS from deduping retries.
    assert_eq!(h["Amz-Sdk-Invocation-Id"].len(), 36);
}

#[test]
fn kiro_oauth_auth_uses_access_token_and_omits_token_type() {
    let exec = KiroExecutor::new("kiro");
    let c = creds(json!({"accessToken": "oauth-tok"}));
    let h = exec.build_headers(&c, true, "https://q.us-east-1.amazonaws.com", "m", &json!({})).unwrap();

    assert_eq!(h["Authorization"], "Bearer oauth-tok");
    assert!(h.get("TokenType").is_none(), "TokenType is api-key/external-idp only");
    assert_eq!(h["x-amzn-kiro-agent-mode"], "spec");

    // The parity header rides on the providerSpecificData branch, so an OAuth
    // connection configured with psd still sends it.
    let with_psd = creds(json!({
        "accessToken": "oauth-tok",
        "providerSpecificData": {}
    }));
    let h = exec.build_headers(&with_psd, false, "https://x", "m", &json!({})).unwrap();
    assert_eq!(h["x-amz-sso-bearer"], "oauth-tok");
}

#[test]
fn kiro_external_idp_sets_external_token_type() {
    let exec = KiroExecutor::new("kiro");
    let c = creds(json!({
        "accessToken": "idp-tok",
        "providerSpecificData": {"authMethod": "external_idp"}
    }));
    let h = exec.build_headers(&c, false, "https://q.us-east-1.amazonaws.com", "m", &json!({})).unwrap();
    assert_eq!(h["Authorization"], "Bearer idp-tok");
    assert_eq!(h["TokenType"], "EXTERNAL_IDP");
}

#[test]
fn kiro_profile_arn_header_is_forwarded_when_present() {
    let exec = KiroExecutor::new("kiro");
    let with = creds(json!({
        "accessToken": "t",
        "providerSpecificData": {"profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/ABC"}
    }));
    let h = exec.build_headers(&with, false, "https://x", "m", &json!({})).unwrap();
    assert_eq!(h["x-amzn-codewhisperer-profile-arn"], "arn:aws:codewhisperer:us-east-1:1:profile/ABC");

    let without = creds(json!({"accessToken": "t"}));
    let h = exec.build_headers(&without, false, "https://x", "m", &json!({})).unwrap();
    assert!(h.get("x-amzn-codewhisperer-profile-arn").is_none());
}

#[test]
fn kiro_url_index_falls_back_to_first_base_url() {
    let exec = KiroExecutor::new("kiro");
    let c = creds(json!({"accessToken": "t"}));
    let first = exec.build_url("m", false, 0, &c).unwrap();
    // Out-of-range index must not produce an empty URL.
    let beyond = exec.build_url("m", false, 99, &c).unwrap();
    assert_eq!(first, beyond);
    assert!(first.starts_with("https://"));
}

#[test]
fn kiro_retries_auth_failures_only_with_fallback_urls() {
    let exec = KiroExecutor::new("kiro");
    // 429 and 5xx rotate everywhere (base rule).
    assert!(exec.should_retry(429, 0));
    assert!(exec.should_retry(503, 0));
    // 401/403 only make sense when another base URL exists.
    let last = exec.base_urls().len().saturating_sub(1);
    assert!(!exec.should_retry(401, last), "no next url to try");
    // A plain 400 is never a retry signal.
    assert!(!exec.should_retry(400, 0));
}

// ─── Cursor ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_headers_require_access_token_and_machine_id() {
    let exec = CursorExecutor::new("cursor");
    let h = exec
        .build_headers(
            &creds(json!({
                "accessToken": "cur-key",
                "providerSpecificData": { "machineId": "mid", "ghostMode": false }
            })),
            false,
            "https://api2.cursor.sh",
            "m",
            &json!({}),
        )
        .unwrap();
    assert_eq!(h["authorization"], "Bearer cur-key", "checksum header set uses lowercase names");
    // The checksum embeds the machine id and a fresh timestamp, so assert its
    // shape rather than comparing against a second call made a moment later.
    assert!(h["x-cursor-checksum"].ends_with("mid"), "{}", h["x-cursor-checksum"]);
    assert_eq!(h["x-ghost-mode"], "false");
    assert_eq!(h["content-type"], "application/connect+proto");

    // Missing machine id is a config error, not a silent fallback: the
    // checksum must stay bound to one machine.
    let no_mid = exec
        .build_headers(&creds(json!({"accessToken": "k"})), false, "https://x", "m", &json!({}))
        .unwrap_err();
    assert!(no_mid.to_string().contains("Machine ID"), "{no_mid}");

    // Missing token likewise.
    let no_tok = exec
        .build_headers(&Credentials::new(), false, "https://x", "m", &json!({}))
        .unwrap_err();
    assert!(no_tok.to_string().contains("access token"), "{no_tok}");
}

#[test]
fn cursor_checksum_ends_with_machine_id_and_stays_url_safe() {
    let machine = "deadbeefcafe";
    let sum = generate_cursor_checksum(machine);
    assert!(sum.ends_with(machine), "{sum}");
    let payload = &sum[..sum.len() - machine.len()];
    assert_eq!(payload.len(), 8, "6 bytes base64 is 8 chars");
    assert!(
        payload.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "url-safe base64 only: {payload}"
    );
}

#[test]
fn cursor_session_id_is_stable_uuid_v5_of_token() {
    let a = generate_session_id("tok-abc");
    let b = generate_session_id("tok-abc");
    let c = generate_session_id("tok-xyz");
    assert_eq!(a, b, "same token → same session id");
    assert_ne!(a, c);
    assert_eq!(a.len(), 36);
    assert_eq!(a.matches('-').count(), 4);
    // v5 UUIDs carry the version nibble in the third group.
    assert_eq!(&a[14..15], "5");
}

#[test]
fn cursor_full_header_set_strips_token_prefix() {
    let h = build_cursor_headers("workos::the-real-token", Some("mid"), false);
    // The derived client key must be a hash of the *clean* token.
    assert_eq!(h["x-client-key"], generate_hashed64_hex("the-real-token", ""));
    assert!(h["x-cursor-checksum"].ends_with("mid"));
    assert_eq!(h["x-session-id"], generate_session_id("the-real-token"));
    assert_eq!(h["authorization"], "Bearer the-real-token", "prefix stripped");
}

// ─── EventStream framing ────────────────────────────────────────────────────

#[test]
fn event_frame_roundtrip_preserves_headers_and_payload() {
    let headers = BTreeMap::from([
        (":event-type".to_string(), "assistantResponseEvent".to_string()),
        (":message-type".to_string(), "event".to_string()),
    ]);
    let payload = json!({"content": "hello"});
    let frame = encode_event_frame(&headers, Some(&payload));

    let parsed = parse_event_frame(&frame).expect("valid frame");
    assert_eq!(parsed.payload, Some(payload));
    match parsed.headers.get(":event-type") {
        Some(EventHeaderValue::String(s)) => assert_eq!(s, "assistantResponseEvent"),
        other => panic!("unexpected header type: {other:?}"),
    }
}

#[test]
fn event_frame_rejects_corrupted_bytes() {
    let mut frame = encode_event_frame(&BTreeMap::new(), Some(&json!({"a": 1})));

    // Flip a payload byte: the message CRC must catch it.
    let mid = frame.len() / 2;
    frame[mid] ^= 0xff;
    assert!(parse_event_frame(&frame).is_err());

    // Truncation must not be silently accepted.
    assert!(parse_event_frame(&frame[..frame.len() - 1]).is_err());

    // Anything shorter than a prelude is rejected outright.
    assert!(parse_event_frame(&[0u8; 4]).is_err());
}

#[test]
fn event_frame_limits_are_enforced() {
    let headers = BTreeMap::from([(":event-type".to_string(), "assistantResponseEvent".to_string())]);
    let frame = encode_event_frame(&headers, Some(&json!({"a": "aaaaaaaaaa"})));

    // A headers budget smaller than the declared header block is rejected.
    assert!(parse_event_frame_with_limits(&frame, frame.len(), 8 * 1024).is_ok());
    assert!(parse_event_frame_with_limits(&frame, frame.len(), 4).is_err());
    // A message budget below the frame size is rejected, not truncated.
    assert!(parse_event_frame_with_limits(&frame, frame.len() - 1, 8 * 1024).is_err());
}

// ─── SigV4 ──────────────────────────────────────────────────────────────────

fn signing_options(region: &str, service: &str, token: Option<&str>) -> SigningOptions {
    SigningOptions {
        method: "POST".into(),
        url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/invoke".into(),
        headers: Some(BTreeMap::from([("host".into(), "bedrock-runtime.us-east-1.amazonaws.com".into())])),
        body: Some(br#"{"a":1}"#.to_vec()),
        region: region.into(),
        service: service.into(),
        credentials: AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: token.map(str::to_string),
        },
        double_encode_path: true,
        date: chrono::DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z")
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc)),
    }
}

#[test]
fn sigv4_authorization_shape() {
    let headers = sign_aws_request(signing_options("us-east-1", "bedrock", None)).unwrap();
    let auth = &headers["Authorization"];
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"), "{auth}");
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
    assert!(auth.contains("SignedHeaders="), "{auth}");
    assert!(auth.contains("Signature="), "{auth}");
    // The date header must match the signed date, not "now".
    assert_eq!(headers["X-Amz-Date"], "20150830T123600Z");
    assert_eq!(headers["host"], "bedrock-runtime.us-east-1.amazonaws.com");
    assert!(headers.get("X-Amz-Security-Token").is_none());
}

#[test]
fn sigv4_region_is_part_of_the_credential_scope() {
    let eu = sign_aws_request(signing_options("eu-central-1", "bedrock", None)).unwrap();
    assert!(eu["Authorization"].contains("/eu-central-1/bedrock/aws4_request"));
    let us = sign_aws_request(signing_options("us-east-1", "bedrock", None)).unwrap();
    assert_ne!(eu["Authorization"], us["Authorization"], "region must change the signature");
}

#[test]
fn sigv4_session_token_becomes_a_signed_header() {
    let headers = sign_aws_request(signing_options("us-east-1", "bedrock", Some("sess-token"))).unwrap();
    assert_eq!(headers["X-Amz-Security-Token"], "sess-token");
    let auth = &headers["Authorization"];
    assert!(
        auth.to_lowercase().contains("x-amz-security-token"),
        "session token must be in SignedHeaders: {auth}"
    );
}

#[test]
fn escape_uri_matches_rfc3986_unreserved_set() {
    assert_eq!(escape_uri("aZ09-_.~"), "aZ09-_.~");
    assert_eq!(escape_uri("a b"), "a%20b");
    assert_eq!(escape_uri("/"), "%2F");
    assert_eq!(escape_uri(":"), "%3A");
    assert_eq!(escape_uri("-"), "-", "dash must not be encoded");
}
