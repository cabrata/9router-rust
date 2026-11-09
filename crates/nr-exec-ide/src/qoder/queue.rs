//! Port of open-sse/shared/qoder/queue.js — model-queue handling.
//!
//! When a model is out of capacity Qoder answers with a
//! `{statusCodeValue, body}` envelope whose body carries `code: "10605"` /
//! `isQueued: true`. That is a transient capacity signal, not a quota block:
//! the model comes back in ~30s and the same request then succeeds. The
//! payload is frequently double- or triple-encoded (a JSON object inside a
//! JSON string inside a JSON string), so every lookup walks the nested tree
//! and parses JSON-encoded string fields as it goes.

use nr_core::Json;

pub const QODER_QUEUE_CODE: &str = "10605";
pub const QODER_MODEL_QUEUE_MAX_WAIT_ENV: &str = "QODER_MODEL_QUEUE_MAX_WAIT_MS";

// qodercli caps the wait at 1h; that is far too long for a gateway holding a
// client request. The queue clears in ~30s windows, so 10 minutes covers
// several of them and still returns a retryable error instead of hanging.
const DEFAULT_MAX_WAIT_MS: u64 = 600_000;
const DEFAULT_RETRY_MS: u64 = 30_000;
// A sub-second floor came from qodercli, but Qoder's queue now answers
// retryAfterSeconds:2 while still refusing the request. Replaying that fast
// makes the throttle worse, so the floor is a full retry interval.
const MIN_RETRY_MS: u64 = 15_000;
const MAX_RETRY_MS: u64 = 60_000;
// The queue-status endpoint can report isQueued:false while the inference call
// is still refused, so its hint is not trusted below the default interval.
const STATUS_HINT_FLOOR_MS: u64 = 30_000;

const NESTED_KEYS: [&str; 7] = ["error", "data", "body", "message", "cause", "result", "payload"];

/// Normalised descriptor of a queue signal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueInfo {
    pub code: Option<String>,
    pub sub_code: Option<String>,
    pub is_queued: Option<bool>,
    pub model_key: Option<String>,
    pub queue_type: Option<String>,
    pub queue_count: Option<f64>,
    pub service_available: Option<bool>,
    pub retry_after_seconds: Option<f64>,
    pub wait_time: Option<f64>,
}

fn num_of(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Json::String(s) => {
            let t = s.trim();
            if t.is_empty() || t.starts_with('+') {
                return None;
            }
            // /^-?\d+(?:\.\d+)?$/
            let body = t.strip_prefix('-').unwrap_or(t);
            if body.is_empty() {
                return None;
            }
            let mut parts = body.splitn(2, '.');
            let int = parts.next().unwrap_or("");
            let frac = parts.next();
            if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            if let Some(f) = frac {
                if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
            }
            t.parse::<f64>().ok().filter(|f| f.is_finite())
        }
        _ => None,
    }
}

fn str_of_trimmed(v: &Json) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn bool_of(v: &Json) -> Option<bool> {
    match v {
        Json::Bool(b) => Some(*b),
        Json::String(s) if s == "true" => Some(true),
        Json::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

/// `tryParse` — only strings that look like JSON objects/arrays.
fn try_parse(v: &Json) -> Option<Json> {
    let s = v.as_str()?.trim();
    if s.is_empty() || (s.as_bytes()[0] != b'{' && s.as_bytes()[0] != b'[') {
        return None;
    }
    serde_json::from_str::<Json>(s).ok()
}

/// Breadth-first walk over nested objects and JSON-encoded string fields.
/// Returns every plain object reachable from the root.
fn walk_objects(root: &Json) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();
    let mut queue: Vec<Json> = vec![root.clone()];
    let mut seen_strings: Vec<String> = Vec::new();
    // Depth-safety guard: a cyclic-looking payload cannot loop forever because
    // parsed strings are only expanded once.
    let mut guard = 0usize;

    while !queue.is_empty() && guard < 10_000 {
        guard += 1;
        let node = queue.remove(0);
        match &node {
            Json::Object(map) => {
                out.push(node.clone());
                for key in NESTED_KEYS {
                    let Some(v) = map.get(key) else { continue };
                    match v {
                        Json::Object(_) | Json::Array(_) => queue.push(v.clone()),
                        Json::String(s) => {
                            if seen_strings.iter().any(|x| x == s) {
                                continue;
                            }
                            seen_strings.push(s.clone());
                            if let Some(parsed) = try_parse(v) {
                                queue.push(parsed);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Json::Array(items) => {
                for item in items {
                    if item.is_object() || item.is_array() {
                        queue.push(item.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse a queue payload into a normalised descriptor (`parseQoderQueueInfo`).
///
/// Returns `None` unless the payload actually carries a queue signal — a plain
/// `code: "10605"` without the inner object still counts, because the code
/// alone is the signal the executor keys off.
pub fn parse_qoder_queue_info(input: &Json) -> Option<QueueInfo> {
    let owned;
    let root = match input {
        Json::String(_) => {
            owned = try_parse(input)?;
            &owned
        }
        Json::Object(_) | Json::Array(_) => input,
        _ => return None,
    };

    let objects = walk_objects(root);
    let mut info = QueueInfo::default();

    for obj in &objects {
        let Some(map) = obj.as_object() else { continue };
        if info.code.is_none() {
            info.code = map.get("code").and_then(str_of_trimmed);
        }
        if info.sub_code.is_none() {
            info.sub_code = map.get("subCode").and_then(str_of_trimmed);
        }
        if info.is_queued.is_none() {
            info.is_queued = map.get("isQueued").and_then(bool_of);
        }
        if info.model_key.is_none() {
            info.model_key = map.get("modelKey").and_then(str_of_trimmed);
        }
        if info.queue_type.is_none() {
            info.queue_type = map.get("queueType").and_then(str_of_trimmed);
        }
        if info.queue_count.is_none() {
            info.queue_count = map.get("queueCount").and_then(num_of);
        }
        if info.service_available.is_none() {
            info.service_available = map.get("serviceAvailable").and_then(bool_of);
        }
        if info.retry_after_seconds.is_none() {
            info.retry_after_seconds = map.get("retryAfterSeconds").and_then(num_of);
        }
        if info.wait_time.is_none() {
            info.wait_time = map.get("waitTime").and_then(num_of);
        }
    }

    let queued = info.is_queued == Some(true)
        || info.service_available == Some(false)
        || info.sub_code.as_deref() == Some("RESOURCE_LIMIT")
        || info.code.as_deref() == Some(QODER_QUEUE_CODE);

    queued.then_some(info)
}

/// `isQoderQueuedBody` — true when a parsed body carries the queue signal.
pub fn is_qoder_queued_body(input: &Json) -> bool {
    parse_qoder_queue_info(input).is_some()
}

/// `computeQoderQueueDelayMs` — delay before the next attempt.
///
/// The inference refusal carries waitTime/retryAfterSeconds for the queue the
/// request actually sits in, so its hint is honoured; a status-derived delay
/// never drops below [`STATUS_HINT_FLOOR_MS`] because the status endpoint has
/// been observed lying about readiness.
pub fn compute_qoder_queue_delay_ms(info: &QueueInfo, from_status: bool) -> u64 {
    let hint = info.retry_after_seconds.or(info.wait_time);
    let mut ms = match hint {
        Some(v) if v.is_finite() && v >= 0.0 => (v * 1000.0) as u64,
        _ => DEFAULT_RETRY_MS,
    };
    if from_status {
        ms = ms.max(STATUS_HINT_FLOOR_MS);
    }
    ms.clamp(MIN_RETRY_MS, MAX_RETRY_MS)
}

/// Total budget for holding a request open while its model is queued.
pub fn qoder_queue_max_wait_ms() -> u64 {
    match std::env::var(QODER_MODEL_QUEUE_MAX_WAIT_ENV).ok().and_then(|v| v.trim().parse::<f64>().ok()) {
        Some(v) if v.is_finite() && v > 0.0 => v as u64,
        _ => DEFAULT_MAX_WAIT_MS,
    }
}

/// The queue-status endpoint qodercli polls. Both query parameters are
/// required — omitting either answers HTTP 400.
pub fn qoder_queue_status_url(base_url: &str, request_set_id: &str, model_key: &str, queue_type: Option<&str>) -> String {
    let mut params: Vec<(String, String)> = Vec::new();
    if !request_set_id.is_empty() {
        params.push(("requestSetId".into(), request_set_id.to_string()));
    }
    if !model_key.is_empty() {
        params.push(("modelKey".into(), model_key.to_string()));
    }
    if let Some(qt) = queue_type.filter(|s| !s.is_empty()) {
        params.push(("queueType".into(), qt.to_string()));
    }
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}/algo/api/v2/service/ask/queue/status?{}", base_url.trim_end_matches('/'), qs)
}

/// `parseQoderQueueStatus` — `None` when the body is not the shape we expect
/// (the caller treats that as "status endpoint unusable").
pub fn parse_qoder_queue_status(input: &Json) -> Option<QueueInfo> {
    let owned;
    let root = match input {
        Json::String(_) => {
            owned = try_parse(input)?;
            &owned
        }
        Json::Object(_) => input,
        _ => return None,
    };
    let map = root.as_object()?;
    let is_queued = map.get("isQueued").and_then(bool_of)?;
    Some(QueueInfo {
        is_queued: Some(is_queued),
        service_available: map.get("serviceAvailable").and_then(bool_of),
        model_key: map.get("modelKey").and_then(str_of_trimmed),
        queue_type: map.get("queueType").and_then(str_of_trimmed),
        queue_count: map.get("queueCount").and_then(num_of),
        retry_after_seconds: map.get("retryAfterSeconds").and_then(num_of),
        wait_time: map.get("waitTime").and_then(num_of),
        ..QueueInfo::default()
    })
}

/// Extract a queue descriptor from a raw error-body string (helper for callers
/// holding text rather than a parsed value).
pub fn parse_qoder_queue_info_str(text: &str) -> Option<QueueInfo> {
    parse_qoder_queue_info(&Json::String(text.to_string()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_nested_queue_payload() {
        // The real shape: code 10605 with the queue object inside `message`,
        // itself a JSON string.
        let inner = json!({
            "isQueued": true,
            "modelKey": "auto",
            "queueCount": 0,
            "queueType": "p3",
            "retryAfterSeconds": 30,
            "serviceAvailable": false,
            "subCode": "RESOURCE_LIMIT",
            "waitTime": 30
        })
        .to_string();
        let payload = json!({
            "code": "10605",
            "message": inner,
            "subCode": "RESOURCE_LIMIT"
        });

        let info = parse_qoder_queue_info(&payload).expect("queue signal");
        assert_eq!(info.code.as_deref(), Some("10605"));
        assert_eq!(info.is_queued, Some(true));
        assert_eq!(info.model_key.as_deref(), Some("auto"));
        assert_eq!(info.queue_type.as_deref(), Some("p3"));
        assert_eq!(info.retry_after_seconds, Some(30.0));
        assert_eq!(info.service_available, Some(false));
    }

    #[test]
    fn parses_queue_from_json_string() {
        let payload = json!({
            "error": { "body": "{\"isQueued\":true,\"queueType\":\"p3\"}" }
        });
        let info = parse_qoder_queue_info_str(&payload.to_string()).expect("queue signal");
        assert_eq!(info.queue_type.as_deref(), Some("p3"));
    }

    #[test]
    fn plain_completion_is_not_queued() {
        let payload = json!({ "choices": [{ "delta": { "content": "hi" } }] });
        assert!(parse_qoder_queue_info(&payload).is_none());
    }

    #[test]
    fn resource_limit_subcode_alone_is_a_signal() {
        let info = parse_qoder_queue_info(&json!({ "subCode": "RESOURCE_LIMIT" })).expect("signal");
        assert_eq!(info.sub_code.as_deref(), Some("RESOURCE_LIMIT"));
    }

    #[test]
    fn delay_is_clamped_to_retry_window() {
        let mut info = QueueInfo { retry_after_seconds: Some(2.0), ..QueueInfo::default() };
        assert_eq!(compute_qoder_queue_delay_ms(&info, false), MIN_RETRY_MS);
        info.retry_after_seconds = Some(600.0);
        assert_eq!(compute_qoder_queue_delay_ms(&info, false), MAX_RETRY_MS);
        // Status-derived hints never drop below the 30s floor.
        info.retry_after_seconds = Some(2.0);
        assert_eq!(compute_qoder_queue_delay_ms(&info, true), STATUS_HINT_FLOOR_MS);
        info.retry_after_seconds = None;
        info.wait_time = Some(45.0);
        assert_eq!(compute_qoder_queue_delay_ms(&info, false), 45_000);
    }

    #[test]
    fn queue_status_url_includes_every_present_param() {
        let url = qoder_queue_status_url("https://api2.qoder.sh", "rs-1", "auto", Some("p3"));
        assert_eq!(
            url,
            "https://api2.qoder.sh/algo/api/v2/service/ask/queue/status?requestSetId=rs-1&modelKey=auto&queueType=p3"
        );
        let url = qoder_queue_status_url("https://api2.qoder.sh", "rs-1", "auto", None);
        assert!(!url.contains("queueType"));
    }

    #[test]
    fn parses_status_body() {
        let info = parse_qoder_queue_status(&json!({
            "isQueued": false, "queueType": "p3", "retryAfterSeconds": 2
        }))
        .expect("status shape");
        assert_eq!(info.is_queued, Some(false));
        assert_eq!(info.queue_count, None);
        // Missing isQueued → not the shape we expect.
        assert!(parse_qoder_queue_status(&json!({ "queueType": "p3" })).is_none());
    }
}
