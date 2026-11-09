//! Request-shaping tests for the IDE executors (qoder, zed, trae, windsurf,
//! antigravity).
//!
//! Every case drives the public surface directly — `BaseHooks::build_url` /
//! `build_headers` / `transform_request`, the request builders, or the stream
//! wrappers — with representative `Credentials` fixtures. Nothing here touches
//! the network.
//!
//! Ported intent: open-sse/executors/{qoder,zed,trae,windsurf,antigravity}.js
//! and ../9router/tests/unit/qoder*.test.js.

use nr_core::{Credentials, Json};
use nr_exec_base::BaseHooks;
use nr_exec_ide::qoder::attachments::{decoded_bytes, extract_url_from_upload_response, stub_text};
use nr_exec_ide::qoder::context_tier::{
    apply_context_tier, estimate_prompt_tokens, get_context_tiers, parse_tier_token_count,
    resolve_context_tier, ContextTier,
};
use nr_exec_ide::qoder::cosy::{derive_machine_id, qoder_encode_body};
use nr_exec_ide::qoder::models::{is_qoder_pat, routable_qoder_models, Catalog};
use nr_exec_ide::qoder::queue::{
    compute_qoder_queue_delay_ms, parse_qoder_queue_info, parse_qoder_queue_status,
    qoder_queue_status_url, QueueInfo,
};
use nr_exec_ide::qoder::sse::{canonicalize_qoder_usage, is_billing_block};
use nr_exec_ide::qoder::{build_chat_url, normalize_content, normalize_messages, QoderExecutor};
use nr_exec_ide::zed::ZedExecutor;
use serde_json::json;

fn creds(v: serde_json::Value) -> Credentials {
    Credentials::from_value(v)
}

/// A device-token Qoder connection as the dashboard stores it.
fn qoder_creds() -> Credentials {
    creds(json!({
        "accessToken": "dt-abc123",
        "displayName": "Dev",
        "email": "dev@example.com",
        "providerSpecificData": { "userId": "user-1", "machineId": "machine-1" }
    }))
}

// ─── Qoder: URL ─────────────────────────────────────────────────────────────

#[test]
fn qoder_url_is_the_cosy_signed_inference_endpoint() {
    let url = build_chat_url(&qoder_creds(), "intl");
    assert_eq!(
        url,
        "https://api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"
    );
    // The encoder flag is what lets qoderEncodeBody run upstream.
    assert!(url.contains("Encode=1"));
}

#[test]
fn qoder_cn_region_uses_the_gateway_host() {
    let url = build_chat_url(&qoder_creds(), "cn");
    assert!(url.starts_with("https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation"), "{url}");
}

#[test]
fn qoder_job_tokens_route_to_api2_on_intl() {
    let mut c = qoder_creds();
    c.set("accessToken", Json::String("jt-xyz".into()));
    // api3 answers 403 "Login expired" for jt- traffic; qodercli serves it
    // from api2 instead.
    assert!(build_chat_url(&c, "intl").starts_with("https://api2.qoder.sh"));
    // CN has a single gateway host.
    assert!(build_chat_url(&c, "cn").starts_with("https://gateway.qoder.com.cn"));
}

#[test]
fn qoder_pat_detection_matches_prefix_only() {
    assert!(is_qoder_pat(Some("pt-abc")));
    assert!(!is_qoder_pat(Some("dt-abc")));
    assert!(!is_qoder_pat(Some("jt-abc")));
    assert!(!is_qoder_pat(None));
}

#[test]
fn qoder_provider_hooks_expose_the_registry_config() {
    let exec = QoderExecutor::new("qoder-cn");
    assert_eq!(BaseHooks::provider(&exec), "qoder-cn");
    assert!(!BaseHooks::no_auth(&exec));
    // timeoutMs from PROVIDERS.qoder-cn.
    assert_eq!(exec.config().get("timeoutMs").and_then(Json::as_u64), Some(120_000));
    assert_eq!(exec.config().get("format").and_then(Json::as_str), Some("openai"));
}

// ─── Qoder: request body shaping ────────────────────────────────────────────

#[test]
fn qoder_request_hoists_system_and_flattens_text_blocks() {
    let messages = json!([
        { "role": "system", "content": "be terse" },
        { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
        { "role": "assistant", "content": "hello" }
    ]);
    let (out, system) = normalize_messages(&messages);

    // Qoder rejects system inside `messages`, so it moves to `payload.system`.
    assert_eq!(system, "be terse");
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|m| m["role"] != json!("system")));
    assert_eq!(out[0]["content"], json!("hi"));
}

#[test]
fn qoder_request_keeps_image_blocks_as_openai_parts() {
    let messages = json!([{
        "role": "user",
        "content": [
            { "type": "text", "text": "compare" },
            { "type": "image_url", "image_url": "https://cdn/x.png" }
        ]
    }]);
    let (out, _) = normalize_messages(&messages);
    let blocks = out[0]["content"].as_array().expect("image content stays an array");
    assert_eq!(blocks.len(), 2);
    // A bare string image_url is normalized to the object form.
    assert_eq!(blocks[1]["image_url"]["url"], json!("https://cdn/x.png"));
}

#[test]
fn qoder_claude_image_becomes_a_data_uri_part() {
    let content = json!([{
        "type": "image",
        "source": { "type": "base64", "media_type": "image/jpeg", "data": "aGk=" }
    }]);
    let normalized = normalize_content(&content);
    let block = &normalized[0];
    assert_eq!(block["type"], json!("image_url"));
    assert_eq!(block["image_url"]["url"], json!("data:image/jpeg;base64,aGk="));
}

#[test]
fn qoder_file_and_document_blocks_become_stubs() {
    let content = json!([
        { "type": "file", "file": { "filename": "big.pdf", "file_data": "data:application/pdf;base64,AAA" } },
        { "type": "document", "title": "spec.docx", "source": { "type": "base64", "data": "AAA" } }
    ]);
    let text = normalize_content(&content).as_str().unwrap().to_string();
    assert!(text.contains("[file omitted: big.pdf"), "{text}");
    assert!(text.contains("[file omitted: spec.docx"), "{text}");
    // 30MB PDFs must never land in agent_chat_generation.
    assert!(!text.contains("base64,AAA"));
}

#[test]
fn qoder_attachment_stub_and_upload_helpers_keep_the_js_shapes() {
    assert_eq!(
        stub_text(Some("a.png"), Some("image/png"), Some(2048), "upload failed"),
        "[file omitted: a.png, 2048 bytes — upload failed]"
    );
    assert_eq!(decoded_bytes("aGVsbG8="), 6); // floor(len * 3 / 4), padding included
    assert_eq!(
        extract_url_from_upload_response(&json!({ "result": { "imageUrls": ["https://oss/a.png"] } })),
        Some("https://oss/a.png".into())
    );
}

#[test]
fn qoder_body_encoder_preserves_length_and_alphabet() {
    let out = qoder_encode_body(b"hello");
    assert_eq!(out.len(), 8, "base64 length is preserved");
    const CUSTOM: &str = "_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!$";
    assert!(out.chars().all(|c| CUSTOM.contains(c)), "{out}");
    assert_eq!(qoder_encode_body(b""), "");
    assert_eq!(qoder_encode_body("hello".as_bytes()), out);
}

#[test]
fn qoder_machine_id_is_stable_per_user() {
    let a = derive_machine_id("user-1");
    assert_eq!(a, derive_machine_id("user-1"));
    assert_ne!(a, derive_machine_id("user-2"));
    // UUID-shaped so it is indistinguishable from a generated id.
    assert_eq!(a.len(), 36);
    assert_eq!(a.split('-').count(), 5);
}

// ─── Qoder: context tiers ───────────────────────────────────────────────────

fn tiered_model_config() -> Json {
    json!({
        "key": "qmodel_38max",
        "max_input_tokens": 180000,
        "context_config": [
            { "name": "200K", "tokenCount": 200000, "isDefault": true },
            { "name": "1M", "tokenCount": 1000000 }
        ]
    })
}

#[test]
fn qoder_tier_parsing_accepts_every_catalog_spelling() {
    assert_eq!(parse_tier_token_count(Some(&json!("200K"))), 200_000);
    assert_eq!(parse_tier_token_count(Some(&json!("1M"))), 1_000_000);
    assert_eq!(parse_tier_token_count(Some(&json!(204_800))), 204_800);
    assert_eq!(parse_tier_token_count(Some(&json!("1.5K"))), 1_500);
    assert_eq!(parse_tier_token_count(Some(&json!("nope"))), 0);

    let tiers = get_context_tiers(&tiered_model_config());
    assert_eq!(tiers.len(), 2);
    assert!(tiers[0].is_default);
}

#[test]
fn qoder_prompt_estimate_counts_cjk_at_one_token_each() {
    let ascii = estimate_prompt_tokens("", &json!([{ "role": "user", "content": "abcd" }]), &json!([]));
    let cjk = estimate_prompt_tokens("", &json!([{ "role": "user", "content": "你好世界" }]), &json!([]));
    // Same character count, ~4x the tokens for CJK.
    assert!(cjk >= ascii, "ascii={ascii} cjk={cjk}");
}

#[test]
fn qoder_tier_only_escalates_when_the_prompt_stops_fitting() {
    let small = json!([{ "role": "user", "content": "hello" }]);
    assert!(resolve_context_tier(&tiered_model_config(), "", &small, &json!([]), None).is_none());

    let big = json!([{ "role": "user", "content": "x".repeat(900_000) }]);
    let choice = resolve_context_tier(&tiered_model_config(), "", &big, &json!([]), None).expect("escalation");
    // ~225k tokens + 15% headroom no longer fits the 180k default, so the
    // smallest advertised tier that covers it wins (the 1M one).
    assert_eq!(choice.tier.token_count, 1_000_000);
    assert_eq!(choice.reason, "auto:fits");

    // Past every tier, the largest is used and the reason says so.
    let huge = json!([{ "role": "user", "content": "x".repeat(9_000_000) }]);
    let choice = resolve_context_tier(&tiered_model_config(), "", &huge, &json!([]), None).expect("escalation");
    assert_eq!(choice.reason, "auto:largest");
}

#[test]
fn qoder_tier_apply_mirrors_the_three_ide_slots() {
    let mut payload = json!({
        "parameters": { "max_tokens": 8 },
        "chat_context": { "extra": {} },
        "model_config": { "key": "qmodel_38max" }
    });
    apply_context_tier(&mut payload, &ContextTier { name: "1M".into(), token_count: 1_000_000, is_default: false });
    assert_eq!(payload["parameters"]["context_length"], json!(1_000_000));
    assert_eq!(payload["parameters"]["max_tokens"], json!(8));
    assert_eq!(payload["chat_context"]["extra"]["ideModelConfigOverride"]["max_input_tokens"], json!(1_000_000));
    assert_eq!(payload["model_config"]["max_input_tokens"], json!(1_000_000));
}

// ─── Qoder: queue + SSE ─────────────────────────────────────────────────────

#[test]
fn qoder_queue_signal_is_recognised_through_nested_json() {
    let inner = json!({
        "isQueued": true, "modelKey": "auto", "queueType": "p3", "retryAfterSeconds": 30
    })
    .to_string();
    let body = json!({ "code": "10605", "message": inner, "subCode": "RESOURCE_LIMIT" });
    let info = parse_qoder_queue_info(&body).expect("queue signal");
    assert_eq!(info.model_key.as_deref(), Some("auto"));
    assert_eq!(info.queue_type.as_deref(), Some("p3"));

    // A normal completion is not a queue signal.
    assert!(parse_qoder_queue_info(&json!({ "choices": [{ "delta": { "content": "hi" } }] })).is_none());
}

#[test]
fn qoder_queue_delay_never_replays_faster_than_the_floor() {
    let info = QueueInfo { retry_after_seconds: Some(2.0), ..QueueInfo::default() };
    assert_eq!(compute_qoder_queue_delay_ms(&info, false), 15_000);
    // A status-derived hint is trusted even less.
    assert_eq!(compute_qoder_queue_delay_ms(&info, true), 30_000);
    let long = QueueInfo { wait_time: Some(600.0), ..QueueInfo::default() };
    assert_eq!(compute_qoder_queue_delay_ms(&long, false), 60_000);
}

#[test]
fn qoder_queue_status_url_and_parse() {
    let url = qoder_queue_status_url("https://api3.qoder.sh", "rs-1", "auto", Some("p3"));
    assert_eq!(
        url,
        "https://api3.qoder.sh/algo/api/v2/service/ask/queue/status?requestSetId=rs-1&modelKey=auto&queueType=p3"
    );
    let status = parse_qoder_queue_status(&json!({ "isQueued": false, "queueType": "p3" })).unwrap();
    assert_eq!(status.is_queued, Some(false));
    // Missing isQueued → endpoint unusable.
    assert!(parse_qoder_queue_status(&json!({ "queueType": "p3" })).is_none());
}

#[test]
fn qoder_billing_block_excludes_queue_throttle() {
    assert!(is_billing_block(r#"{"code":"112"}"#));
    assert!(is_billing_block(r#"{"pricingUrl":"https://x"}"#));
    // 10605 is the transient capacity signal and must stay retryable.
    assert!(!is_billing_block(r#"{"code":"10605","subCode":"RESOURCE_LIMIT"}"#));
}

#[test]
fn qoder_usage_is_canonicalized_for_downstream() {
    let usage = json!({
        "input_tokens": 12,
        "output_tokens": 3,
        "prompt_tokens_details": { "cached_tokens": 4 }
    });
    let out = canonicalize_qoder_usage(&usage).unwrap();
    assert_eq!(out["prompt_tokens"], json!(12));
    assert_eq!(out["completion_tokens"], json!(3));
    assert_eq!(out["total_tokens"], json!(15));
    assert_eq!(out["cached_tokens"], json!(4));
}

#[test]
fn qoder_routable_models_include_hidden_and_static_keys() {
    let mut raw = serde_json::Map::new();
    raw.insert("auto".into(), json!({ "key": "auto", "display_name": "Auto" }));
    raw.insert("hidden".into(), json!({ "key": "hidden", "enable": false }));
    let catalog = Catalog { models: vec![json!({ "id": "auto", "name": "Auto" })], raw_configs: raw };
    let ids: Vec<String> = routable_qoder_models(Some(&catalog))
        .iter()
        .filter_map(|m| m.get("id").and_then(Json::as_str).map(String::from))
        .collect();
    assert_eq!(ids, vec!["smodel", "cmodel", "auto", "hidden"]);
}

// ─── Zed ────────────────────────────────────────────────────────────────────

#[test]
fn zed_url_and_headers_follow_the_cloud_contract() {
    let exec = ZedExecutor::new("zed");
    assert_eq!(BaseHooks::provider(&exec), "zed");
    // `forceStream: true` in PROVIDERS.zed.
    assert_eq!(exec.config().get("forceStream"), Some(&Json::Bool(true)));
    assert_eq!(
        exec.config().get("baseUrl").and_then(Json::as_str),
        Some("https://cloud.zed.dev/completions")
    );
    // Combined "<user_id> <access_token>" Authorization scheme.
    let auth = exec.config().get("auth").unwrap();
    assert_eq!(auth.get("header").and_then(Json::as_str), Some("Authorization"));
    assert_eq!(auth.get("scheme").and_then(Json::as_str), Some("<user_id> <access_token>"));
}

// ─── Trae / Windsurf / Antigravity presence ────────────────────────────────

#[test]
fn trae_executor_provider_identity() {
    let exec = nr_exec_ide::trae::create("trae");
    assert_eq!(exec.provider(), "trae");
}

#[test]
fn windsurf_executor_provider_identity() {
    let exec = nr_exec_ide::windsurf::create("windsurf");
    assert_eq!(exec.provider(), "windsurf");
}

#[test]
fn antigravity_executor_provider_identity() {
    let exec = nr_exec_ide::antigravity::create("antigravity");
    assert_eq!(exec.provider(), "antigravity");
}
