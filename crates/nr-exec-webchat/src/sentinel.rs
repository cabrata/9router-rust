//! Port of open-sse/executors/chatgpt-web/sentinel.js
//!
//! Pure-Node Sentinel client for chatgpt.com.
//!
//! The web app solves three layers before a /backend-api call is accepted:
//!   1. `chat-requirements/prepare` wants an `OpenAI-Sentinel-Chat-Requirements-Prepare-Token`
//!      — `gAAAAAC` + base64 of a 24-slot browser fingerprint.
//!   2. `chat-requirements` may demand proof-of-work: brute-force a counter until
//!      the FNV-1a hash of `seed + base64(fingerprint)` sorts below `difficulty`.
//!   3. It may also demand a Turnstile VM challenge (browser-only, not handled here).

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use chrono::Utc;
use rand::Rng;
use serde_json::json;
use std::collections::HashMap;

const PREPARE_PREFIX: &str = "gAAAAAC";
const PROOF_PREFIX: &str = "gAAAAAB";
const PROOF_SUFFIX: &str = "~S";
const MAX_POW_ITERATIONS: u32 = 500000;

pub struct SentinelHeaders {
    pub prepare: &'static str,
    pub requirements: &'static str,
    pub proof: &'static str,
    pub turnstile: &'static str,
}

pub const SENTINEL_HEADERS: SentinelHeaders = SentinelHeaders {
    prepare: "OpenAI-Sentinel-Chat-Requirements-Prepare-Token",
    requirements: "OpenAI-Sentinel-Chat-Requirements-Token",
    proof: "OpenAI-Sentinel-Proof-Token",
    turnstile: "OpenAI-Sentinel-Turnstile-Token",
};

/// FNV-1a 32-bit hash with three avalanche rounds.
fn sentinel_hash(input: &str) -> String {
    let mut h: u32 = 0x811c9dc5;
    for byte in input.as_bytes() {
        h ^= *byte as u32;
        h = h.wrapping_mul(0x1000193);
    }
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^= h >> 16;
    format!("{:08x}", h)
}

/// Encode fingerprint array to base64.
fn encode_fingerprint(slots: &[serde_json::Value]) -> String {
    let json_str = serde_json::to_string(slots).unwrap_or_default();
    BASE64_STANDARD.encode(json_str.as_bytes())
}

static SCRIPT_CANDIDATES: &[&str] = &[
    "https://chatgpt.com/cdn/assets/685809.dec86dcdc1.js",
    "https://chatgpt.com/cdn/assets/399565.a1117ca8b4.js",
];
static DOCUMENT_KEYS: &[&str] = &[
    "cookie", "domain", "readyState", "title", "URL", "charset", "location",
];
static WINDOW_KEYS: &[&str] = &[
    "caches", "crypto", "fetch", "indexedDB", "localStorage", "origin",
];
static NAV_PROTOTYPE: &[&str] = &["userAgent", "language", "hardwareConcurrency"];

/// Build the 24-slot fingerprint array.
fn build_fingerprint(user_agent: &str) -> Vec<serde_json::Value> {
    let mut rng = rand::thread_rng();

    let nav_key = NAV_PROTOTYPE[rng.gen_range(0..NAV_PROTOTYPE.len())];
    let script = SCRIPT_CANDIDATES[rng.gen_range(0..SCRIPT_CANDIDATES.len())];
    let doc_key = DOCUMENT_KEYS[rng.gen_range(0..DOCUMENT_KEYS.len())];
    let window_key = WINDOW_KEYS[rng.gen_range(0..WINDOW_KEYS.len())];

    let now = Utc::now().to_rfc2822();
    let uuid = uuid::Uuid::new_v4().to_string();

    vec![
        json!(1280 + 800),                                        // 0: screen width + height
        json!(now),                                               // 1: date string
        json!(serde_json::Value::Null),                           // 2: heap size limit
        json!(0),                                                 // 3: overwritten by counter
        json!(user_agent),                                        // 4: user agent
        json!(script),                                            // 5: script candidate
        json!(serde_json::Value::Null),                           // 6: document build id
        json!("en-US"),                                           // 7: language
        json!("en-US,en"),                                        // 8: languages
        json!(0),                                                 // 9: overwritten by elapsed ms
        json!(format!("{nav_key}−{user_agent}")),                 // 10: nav key + UA (U+2212)
        json!(doc_key),                                           // 11: random document key
        json!(window_key),                                        // 12: random window key
        json!(0),                                                 // 13: performance.now()
        json!(uuid),                                              // 14: random UUID
        json!(""),                                                // 15: search-param keys
        json!(8),                                                 // 16: hardwareConcurrency
        json!(0),                                                 // 17: performance.timeOrigin
        json!(0),                                                 // 18-23: feature probes
        json!(0),
        json!(0),
        json!(0),
        json!(0),
        json!(0),
    ]
}

/// Generate the `gAAAAAC…` prepare-token header value.
pub fn build_prepare_token(user_agent: &str) -> String {
    let mut slots = build_fingerprint(user_agent);
    slots[3] = json!(1);
    slots[9] = json!(0);
    format!("{}{}", PREPARE_PREFIX, encode_fingerprint(&slots))
}

/// Solve a proof-of-work challenge.
/// Returns `gAAAAAB<base64>~S`
pub fn solve_proof_of_work(seed: &str, difficulty: &str, user_agent: &str) -> Result<String, String> {
    if seed.is_empty() || difficulty.is_empty() {
        return Err("ChatGPT sentinel asked for proof of work without seed/difficulty".to_string());
    }
    let started = std::time::Instant::now();
    let mut slots = build_fingerprint(user_agent);

    for counter in 0..MAX_POW_ITERATIONS {
        slots[3] = json!(counter);
        slots[9] = json!(started.elapsed().as_millis() as u32);
        let encoded = encode_fingerprint(&slots);
        let hash_input = format!("{}{}", seed, encoded);
        let hash = sentinel_hash(&hash_input);
        if &hash[..difficulty.len()] <= difficulty {
            return Ok(format!("{}{}{}", PROOF_PREFIX, encoded, PROOF_SUFFIX));
        }
    }
    Err("ChatGPT sentinel proof of work exhausted its iteration budget".to_string())
}

/// Post a JSON request and parse the response.
pub async fn post_json(
    url: &str,
    headers: &HashMap<String, String>,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::new();
    let mut req_headers = reqwest::header::HeaderMap::new();
    req_headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json"
            .parse()
            .map_err(|_| "Invalid content type".to_string())?,
    );
    req_headers.insert(
        reqwest::header::ACCEPT,
        "application/json"
            .parse()
            .map_err(|_| "Invalid accept".to_string())?,
    );
    for (k, v) in headers {
        let header_name = k
            .parse::<reqwest::header::HeaderName>()
            .map_err(|_| format!("Invalid header name: {}", k))?;
        let header_value = v
            .parse::<reqwest::header::HeaderValue>()
            .map_err(|_| format!("Invalid header value for {}", k))?;
        req_headers.insert(header_name, header_value);
    }

    let res = client
        .post(url)
        .headers(req_headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    let status = res.status();
    let text = res.text().await.map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        let truncated = if text.len() > 200 {
            &text[..200]
        } else {
            &text
        };
        return Err(format!(
            "ChatGPT {} failed (HTTP {}) {}",
            url.split('/').last().unwrap_or("request"),
            status,
            truncated
        ));
    }

    serde_json::from_str(&text)
        .map_err(|_| format!("ChatGPT {} returned invalid JSON", url.split('/').last().unwrap_or("request")))
}

/// Run prepare → requirements and return the sentinel headers for a `/backend-api` call.
pub async fn fetch_sentinel_headers(
    base_url: &str,
    access_token: &str,
    user_agent: &str,
) -> Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {}", access_token));
    headers.insert("User-Agent".to_string(), user_agent.to_string());

    let prepare_token = build_prepare_token(user_agent);
    headers.insert(SENTINEL_HEADERS.prepare.to_string(), prepare_token);

    let prepare_url = format!("{}/backend-api/sentinel/chat-requirements/prepare", base_url);
    let prepared = post_json(&prepare_url, &headers, json!({})).await?;

    let prepare_token = prepared
        .get("prepare_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "ChatGPT sentinel prepare returned no prepare_token".to_string())?;

    headers.insert(SENTINEL_HEADERS.prepare.to_string(), prepare_token.to_string());

    let requirements_url = format!("{}/backend-api/sentinel/chat-requirements", base_url);
    let requirements = post_json(&requirements_url, &headers, json!({})).await?;

    let mut tokens = HashMap::new();
    if let Some(token) = requirements.get("token").and_then(|v| v.as_str()) {
        tokens.insert(SENTINEL_HEADERS.requirements.to_string(), token.to_string());
    }

    if let Some(pow) = requirements.get("proofofwork").and_then(|v| v.as_object()) {
        if pow.get("required").and_then(|v| v.as_bool()).unwrap_or(false) {
            let seed = pow
                .get("seed")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "PoW seed missing".to_string())?;
            let difficulty = pow
                .get("difficulty")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "PoW difficulty missing".to_string())?;
            let proof_token = solve_proof_of_work(seed, difficulty, user_agent)?;
            tokens.insert(SENTINEL_HEADERS.proof.to_string(), proof_token);
        }
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fnv1a_hash() {
        let hash = sentinel_hash("test");
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 8);
    }

    #[test]
    fn test_build_prepare_token() {
        let token = build_prepare_token("Mozilla/5.0");
        assert!(token.starts_with(PREPARE_PREFIX));
        assert!(token.len() > PREPARE_PREFIX.len());
    }

    #[test]
    fn test_encode_fingerprint() {
        let fingerprint = vec![json!(1), json!("test"), json!(null)];
        let encoded = encode_fingerprint(&fingerprint);
        let decoded = BASE64_STANDARD.decode(&encoded);
        assert!(decoded.is_ok());
    }

    #[test]
    fn test_solve_proof_of_work_error_cases() {
        let result = solve_proof_of_work("", "00000000", "Mozilla/5.0");
        assert!(result.is_err());

        let result = solve_proof_of_work("seed", "", "Mozilla/5.0");
        assert!(result.is_err());
    }
}
