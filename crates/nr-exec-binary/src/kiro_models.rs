//! Port of open-sse/services/kiroModels.js
//! Kiro model catalog fetching and caching with synthetic variant expansion.

use nr_core::Credentials;
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const KIRO_RUNTIME_SDK_VERSION: &str = "1.0.0";
const KIRO_AGENT_OS: &str = "windows";
const KIRO_AGENT_OS_VERSION: &str = "10.0.26200";
const KIRO_NODE_VERSION: &str = "22.21.1";
const KIRO_VERSION: &str = "0.10.32";

const DEFAULT_REGION: &str = "us-east-1";
const FETCH_TIMEOUT_MS: u64 = 30_000;
const CACHE_TTL_MS: u64 = 5 * 60 * 1000; // 5 minutes

#[derive(Clone)]
struct CatalogCacheEntry {
    expires_at: u64,
    models: Vec<JsonValue>,
}

pub struct KiroModelsService {
    cache: Arc<Mutex<HashMap<String, CatalogCacheEntry>>>,
}

impl KiroModelsService {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn strip_synthetic_suffixes(id: &str) -> String {
        let mut out = id.to_string();
        if out.ends_with("-agentic") {
            out = out[..out.len() - "-agentic".len()].to_string();
        }
        if out.ends_with("-thinking") {
            out = out[..out.len() - "-thinking".len()].to_string();
        }
        out
    }

    fn region_from_profile_arn(profile_arn: Option<&str>) -> String {
        if let Some(arn) = profile_arn {
            let parts: Vec<&str> = arn.split(':').collect();
            if parts.len() >= 4 && !parts[3].is_empty() {
                return parts[3].to_string();
            }
        }
        DEFAULT_REGION.to_string()
    }

    fn build_kiro_fingerprint_headers(credentials: &Credentials) -> HashMap<String, String> {
        let seed = credentials
            .provider_specific_data()
            .and_then(|psd| psd.get("clientId").and_then(|v| v.as_str()))
            .or_else(|| credentials.refresh_token().map(|t| t.as_str()))
            .or_else(|| {
                credentials
                    .provider_specific_data()
                    .and_then(|psd| psd.get("profileArn").and_then(|v| v.as_str()))
            })
            .or_else(|| credentials.access_token().map(|t| t.as_str()))
            .unwrap_or("kiro-anonymous");

        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        let machine_id = format!("{:x}", hasher.finalize());

        let user_agent = format!(
            "aws-sdk-js/{} ua/2.1 os/{}#{} lang/js md/nodejs#{} api/codewhispererruntime#{} m/N,E KiroIDE-{}-{}",
            KIRO_RUNTIME_SDK_VERSION,
            KIRO_AGENT_OS,
            KIRO_AGENT_OS_VERSION,
            KIRO_NODE_VERSION,
            KIRO_RUNTIME_SDK_VERSION,
            KIRO_VERSION,
            machine_id
        );
        let amz_user_agent = format!(
            "aws-sdk-js/{} KiroIDE-{}-{}",
            KIRO_RUNTIME_SDK_VERSION, KIRO_VERSION, machine_id
        );

        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), user_agent);
        headers.insert("x-amz-user-agent".to_string(), amz_user_agent);
        headers.insert("x-amzn-kiro-agent-mode".to_string(), "vibe".to_string());
        headers.insert("x-amzn-codewhisperer-optout".to_string(), "true".to_string());
        headers.insert("amz-sdk-request".to_string(), "attempt=1; max=1".to_string());
        headers.insert("amz-sdk-invocation-id".to_string(), Uuid::new_v4().to_string());
        headers.insert("Accept".to_string(), "application/json".to_string());

        headers
    }

    fn build_variants(upstream_id: &str, display_name: Option<&str>) -> Vec<JsonValue> {
        let safe_upstream = Self::strip_synthetic_suffixes(upstream_id);
        let display = display_name.unwrap_or(&format!("Kiro {}", safe_upstream));
        let is_auto = safe_upstream == "auto";

        let mut variants = vec![
            json!({
                "id": safe_upstream,
                "name": display,
                "capabilities": { "thinking": false, "agentic": false }
            }),
            json!({
                "id": format!("{}-thinking", safe_upstream),
                "name": format!("{} (Thinking)", display),
                "capabilities": { "thinking": true, "agentic": false }
            }),
        ];

        if !is_auto {
            variants.push(json!({
                "id": format!("{}-agentic", safe_upstream),
                "name": format!("{} (Agentic)", display),
                "capabilities": { "thinking": false, "agentic": true }
            }));
            variants.push(json!({
                "id": format!("{}-thinking-agentic", safe_upstream),
                "name": format!("{} (Thinking + Agentic)", display),
                "capabilities": { "thinking": true, "agentic": true }
            }));
        }

        variants
    }

    fn format_display_name(
        model_name: Option<&str>,
        model_id: &str,
        rate_multiplier: Option<f64>,
    ) -> String {
        let base = model_name.unwrap_or(model_id).trim();
        if let Some(rate) = rate_multiplier {
            if (rate - 1.0).abs() < 1e-9 || rate <= 0.0 || !rate.is_finite() {
                return format!("Kiro {}", base);
            }
            let rate_str = format!("{:.1}", rate).replace(",", ".");
            return format!("Kiro {} ({}x credit)", base, rate_str);
        }
        format!("Kiro {}", base)
    }

    pub async fn resolve_kiro_models(
        &self,
        credentials: &Credentials,
        force_refresh: bool,
    ) -> Option<(Vec<JsonValue>, Vec<JsonValue>)> {
        if credentials.access_token().is_none() {
            return None;
        }

        let key = self.cache_key(credentials);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if !force_refresh {
            let cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&key) {
                if entry.expires_at > now {
                    return Some((entry.models.clone(), entry.models.clone()));
                }
            }
        }

        // TODO(dedupe): Fetch from Kiro API and cache results
        // This requires:
        // - HTTP client setup (reqwest)
        // - Building ListAvailableModels URL with profileArn
        // - Handling 401 with token refresh via nr_oauth::refresh_kiro_token
        // - Parsing response and building variants
        // - Caching per credential with TTL
        None
    }

    fn cache_key(&self, credentials: &Credentials) -> String {
        let psd = credentials.provider_specific_data().unwrap_or(&serde_json::json!({}));
        let seed = psd
            .get("profileArn")
            .and_then(|v| v.as_str())
            .or_else(|| psd.get("clientId").and_then(|v| v.as_str()))
            .or_else(|| credentials.refresh_token().map(|t| t.as_str()))
            .or_else(|| credentials.access_token().map(|t| t.as_str()))
            .unwrap_or("anonymous");

        let mut hasher = Sha256::new();
        hasher.update(format!("kiro:{}", seed).as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub fn clear_cache(&self) {
        let mut cache = self.cache.lock().unwrap();
        cache.clear();
    }
}

impl Default for KiroModelsService {
    fn default() -> Self {
        Self::new()
    }
}
