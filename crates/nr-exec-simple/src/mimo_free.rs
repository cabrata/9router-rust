//! Port of open-sse/executors/mimo-free.js.
//!
//! MiMoCode Free requires bootstrapping a JWT from an open API, caching it per machine,
//! and retrying on 401/403 with a fresh JWT.

use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Error};
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList};
use serde_json::json;

const BOOTSTRAP_URL: &str = "https://api.xiaomimimo.com/api/free-ai/bootstrap";
const CHAT_URL: &str = "https://api.xiaomimimo.com/chat/v1/chat/completions";
const SESSION_AFFINITY_PREFIX: &str = "ses_";
const SESSION_ID_LENGTH: usize = 24;
const JWT_FALLBACK_TTL_SEC: u64 = 3000;
const JWT_EXPIRY_BUFFER_MS: u64 = 300_000;
const SESSION_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789";

/// Anti-abuse gate: upstream rejects requests without a Chrome-like User-Agent.
const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
];

/// Anti-abuse gate marker: the free chat endpoint returns 403 unless a system message contains this.
pub const MIMO_SYSTEM_MARKER: &str =
    "You are MiMoCode, an interactive CLI tool that helps users with software engineering tasks.";

// In-process JWT cache: per-process, survives across requests but not restarts
static JWT_CACHE: Mutex<HashMap<String, JwtCacheEntry>> = Mutex::new(HashMap::new());

struct JwtCacheEntry {
    jwt: String,
    expires_at: u64, // milliseconds
}

/// Generate a fingerprint from machine info (username, hostname, CPU, OS).
fn generate_fingerprint() -> String {
    use std::env;
    use sha2::{Sha256, Digest};

    let username = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown-user".to_string());
    let hostname_str = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown-host".to_string());

    let seed = format!("{}|{}", hostname_str, username);
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a random session ID.
fn generate_session_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut id = SESSION_AFFINITY_PREFIX.to_string();
    for _ in 0..SESSION_ID_LENGTH {
        let idx = rng.gen_range(0..SESSION_CHARS.len());
        id.push(SESSION_CHARS.chars().nth(idx).unwrap());
    }
    id
}

/// Parse JWT exp claim to get expiry timestamp.
fn parse_jwt_exp(jwt: &str) -> u64 {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() < 2 {
        return std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 + JWT_FALLBACK_TTL_SEC * 1000)
            .unwrap_or(JWT_FALLBACK_TTL_SEC * 1000);
    }

    // Decode the payload (second part)
    let payload = parts[1];
    // Add padding if needed
    let padding = (4 - (payload.len() % 4)) % 4;
    let padded = format!("{}{}", payload, "=".repeat(padding));

    if let Ok(decoded) = base64::decode(&padded) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&decoded) {
            if let Some(exp_secs) = v.get("exp").and_then(|e| e.as_u64()) {
                return exp_secs * 1000;
            }
        }
    }

    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 + JWT_FALLBACK_TTL_SEC * 1000)
        .unwrap_or(JWT_FALLBACK_TTL_SEC * 1000)
}

/// Ensure the body carries the anti-abuse marker in a system message.
fn inject_system_marker(mut body: Json) -> Json {
    let obj = match body.as_object_mut() {
        Some(o) => o,
        None => return body,
    };

    let messages = obj.get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    let has_marker = messages.iter().any(|m| {
        m.get("role").and_then(Json::as_str) == Some("system")
            && m.get("content").and_then(Json::as_str)
                .map(|c| c.contains(MIMO_SYSTEM_MARKER))
                .unwrap_or(false)
    });

    if has_marker {
        return body;
    }

    let mut new_messages = vec![json!({
        "role": "system",
        "content": MIMO_SYSTEM_MARKER
    })];
    new_messages.extend(messages);
    obj.insert("messages".to_string(), Json::Array(new_messages));
    body
}

/// Bootstrap a JWT from the MiMo free API.
async fn bootstrap_jwt(proxy_options: Option<&nr_core::ProxyOptions>) -> Result<String> {
    let fingerprint = generate_fingerprint();

    let body = json!({ "client": fingerprint });

    let mut headers = HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("User-Agent".into(), USER_AGENTS[rand::random::<usize>() % USER_AGENTS.len()].to_string());

    let fetch = FetchRequest::post_json(BOOTSTRAP_URL.to_string(), headers, &body);
    let response = proxy_fetch(fetch, proxy_options).await?;

    if response.status != 200 {
        return Err(Error::Network(format!("MiMo bootstrap failed: {}", response.status)));
    }

    let resp_body = response.bytes().await.map_err(|e| Error::Network(e.to_string()))?;
    let data: serde_json::Value = serde_json::from_slice(&resp_body)
        .map_err(|e| Error::Network(format!("MiMo bootstrap response parse failed: {}", e)))?;

    let jwt = data.get("jwt")
        .and_then(|j| j.as_str())
        .ok_or_else(|| Error::Network("MiMo bootstrap returned no JWT".to_string()))?;

    Ok(jwt.to_string())
}

/// Get a cached JWT or bootstrap a new one.
async fn get_or_bootstrap_jwt(proxy_options: Option<&nr_core::ProxyOptions>) -> Result<String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Check cache
    {
        let cache = JWT_CACHE.lock().unwrap();
        let key = "mimo-free-jwt";
        if let Some(entry) = cache.get(key) {
            if now_ms < entry.expires_at - JWT_EXPIRY_BUFFER_MS {
                return Ok(entry.jwt.clone());
            }
        }
    }

    // Bootstrap new JWT
    let jwt = bootstrap_jwt(proxy_options).await?;
    let expires_at = parse_jwt_exp(&jwt);

    // Update cache
    {
        let mut cache = JWT_CACHE.lock().unwrap();
        cache.insert("mimo-free-jwt".to_string(), JwtCacheEntry { jwt: jwt.clone(), expires_at });
    }

    Ok(jwt)
}

struct MimoFreeExecutor;

#[async_trait]
impl Executor for MimoFreeExecutor {
    fn provider(&self) -> &str {
        "mimo-free"
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Bootstrap JWT
        let jwt = get_or_bootstrap_jwt(req.proxy.as_ref()).await?;

        let url = CHAT_URL.to_string();
        let transformed_body = inject_system_marker(req.body.clone());

        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("X-Mimo-Source".into(), "mimocode-cli-free".into());

        use rand::Rng;
        let mut rng = rand::thread_rng();
        headers.insert("User-Agent".into(), USER_AGENTS[rng.gen_range(0..USER_AGENTS.len())].to_string());
        headers.insert("x-session-affinity".into(), generate_session_id());
        headers.insert("Authorization".into(), format!("Bearer {}", jwt));
        headers.insert("Accept".into(), if req.stream { "text/event-stream".into() } else { "application/json".into() });

        req.log.debug("FETCH", format!("MIMO-FREE → {}", url));

        let fetch = FetchRequest::post_json(url.clone(), headers.clone(), &transformed_body);
        let mut response = proxy_fetch(fetch, req.proxy.as_ref()).await?;

        // On 401/403, retry once with a fresh JWT
        if response.status == 401 || response.status == 403 {
            req.log.debug("AUTH", "MiMo auth failed, re-bootstrapping...");

            let fresh_jwt = bootstrap_jwt(req.proxy.as_ref()).await?;
            let mut cache = JWT_CACHE.lock().unwrap();
            cache.remove("mimo-free-jwt");
            drop(cache);

            headers.insert("Authorization".into(), format!("Bearer {}", fresh_jwt));
            let fetch_retry = FetchRequest::post_json(url.clone(), headers.clone(), &transformed_body);
            response = proxy_fetch(fetch_retry, req.proxy.as_ref()).await?;
        }

        Ok(ExecResult {
            response,
            url,
            headers,
            transformed_body,
            response_format: None,
        })
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(MimoFreeExecutor)
}
