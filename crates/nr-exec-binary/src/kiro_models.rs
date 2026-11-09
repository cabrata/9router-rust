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
    raw_models: Vec<JsonValue>,
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
            .psd()
            .and_then(|psd| psd.get("clientId").and_then(|v| v.as_str()))
            .or_else(|| credentials.refresh_token())
            .or_else(|| {
                credentials
                    .psd()
                    .and_then(|psd| psd.get("profileArn").and_then(|v| v.as_str()))
            })
            .or_else(|| credentials.access_token())
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
        let fallback = format!("Kiro {}", safe_upstream);
        let display = display_name.unwrap_or(&fallback);
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
        proxy: Option<&nr_core::http::ProxyOptions>,
    ) -> Option<(Vec<JsonValue>, Vec<JsonValue>)> {
        if credentials.access_token().is_none() {
            return None;
        }

        let key = self.cache_key(credentials);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        if !force_refresh {
            let cached = {
                let cache = self.cache.lock().unwrap();
                cache
                    .get(&key)
                    .filter(|entry| entry.expires_at > now)
                    .map(|entry| (entry.models.clone(), entry.raw_models.clone()))
            };
            if let Some(hit) = cached {
                return Some(hit);
            }
        }

        // A 401 from Kiro is answered by one token refresh + retry; the
        // in-memory credential is updated so later retry logic uses the fresh
        // token.
        let mut creds = credentials.clone();
        let mut raw = match Self::fetch_catalog_raw(&creds, proxy).await {
            Ok(raw) => raw,
            Err(status) if status == 401 && credentials.refresh_token().is_some() => {
                let refresh_token = credentials.refresh_token().unwrap_or("").to_string();
                let refreshed = nr_oauth::token_refresh::refresh_kiro_token(
                    &refresh_token,
                    credentials.psd(),
                    &nr_core::Logger::new(),
                    proxy,
                )
                .await?;
                let access = refreshed.get("accessToken").and_then(JsonValue::as_str)?.to_string();
                creds.set("accessToken", JsonValue::String(access));
                if let Some(rt) = refreshed.get("refreshToken").and_then(JsonValue::as_str) {
                    creds.set("refreshToken", JsonValue::String(rt.to_string()));
                }
                match Self::fetch_catalog_raw(&creds, proxy).await {
                    Ok(raw) => raw,
                    Err(_) => return None,
                }
            }
            Err(_) => return None,
        };

        let expanded = Self::expand(&raw);
        raw.sort_by(|a, b| {
            a.get("modelId").and_then(JsonValue::as_str).unwrap_or("")
                .cmp(b.get("modelId").and_then(JsonValue::as_str).unwrap_or(""))
        });

        let mut cache = self.cache.lock().unwrap();
        cache.insert(
            key,
            CatalogCacheEntry {
                expires_at: now + CACHE_TTL_MS,
                models: expanded.clone(),
                raw_models: raw.clone(),
            },
        );
        Some((expanded, raw))
    }

    /// Expand each upstream entry into the synthetic 9router variant set.
    fn expand(raw: &[JsonValue]) -> Vec<JsonValue> {
        let mut expanded = Vec::new();
        for m in raw {
            if !m.is_object() {
                continue;
            }
            let upstream_id = m
                .get("modelId")
                .and_then(JsonValue::as_str)
                .or_else(|| m.get("id").and_then(JsonValue::as_str));
            let upstream_id = match upstream_id {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            let display = Self::format_display_name(
                m.get("modelName").and_then(JsonValue::as_str),
                &upstream_id,
                m.get("rateMultiplier").and_then(JsonValue::as_f64),
            );
            let ctx = m
                .pointer("/tokenLimits/maxInputTokens")
                .and_then(JsonValue::as_i64)
                .unwrap_or(200_000);
            let rate = m
                .get("rateMultiplier")
                .and_then(JsonValue::as_f64)
                .filter(|r| r.is_finite())
                .unwrap_or(1.0);
            let description = m.get("description").and_then(JsonValue::as_str).unwrap_or("");
            for mut variant in Self::build_variants(&upstream_id, Some(&display)) {
                if let Some(obj) = variant.as_object_mut() {
                    obj.insert("contextLength".into(), JsonValue::from(ctx));
                    obj.insert("rateMultiplier".into(), JsonValue::from(rate));
                    obj.insert("upstreamModelId".into(), JsonValue::String(upstream_id.clone()));
                    obj.insert("description".into(), JsonValue::String(description.to_string()));
                }
                expanded.push(variant);
            }
        }
        expanded
    }

    /// `fetchKiroCatalogRaw`: GET ListAvailableModels, or Err(http_status).
    async fn fetch_catalog_raw(
        credentials: &Credentials,
        proxy: Option<&nr_core::http::ProxyOptions>,
    ) -> std::result::Result<Vec<JsonValue>, u16> {
        let profile_arn = credentials.psd_str("profileArn").unwrap_or("").to_string();
        let region = Self::region_from_profile_arn(Some(&profile_arn));

        let mut params = vec![("origin".to_string(), "AI_EDITOR".to_string())];
        if !profile_arn.is_empty() {
            params.push(("profileArn".to_string(), profile_arn.clone()));
        }
        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let request_url = format!("https://q.{region}.amazonaws.com/ListAvailableModels?{query}");

        let mut headers = nr_core::http::HeaderList::new();
        for (k, v) in Self::build_kiro_fingerprint_headers(credentials) {
            headers.insert(k, v);
        }
        headers.insert(
            "Authorization".into(),
            format!("Bearer {}", credentials.access_token().unwrap_or("")),
        );

        let mut fetch = nr_core::http::FetchRequest::get(request_url);
        fetch.headers = headers;
        fetch.header_timeout = Some(std::time::Duration::from_millis(FETCH_TIMEOUT_MS));

        let response = nr_core::http::proxy_fetch(fetch, proxy)
            .await
            .map_err(|_| 0u16)?;
        if response.status != 200 {
            return Err(response.status);
        }
        let body = response.bytes().await.map_err(|_| 0u16)?;
        let data: JsonValue = serde_json::from_slice(&body).map_err(|_| 0u16)?;
        Ok(data
            .get("models")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn cache_key(&self, credentials: &Credentials) -> String {
        let psd = credentials.psd();
        let seed = psd
            .and_then(|p| p.get("profileArn").and_then(|v| v.as_str()))
            .or_else(|| psd.and_then(|p| p.get("clientId").and_then(|v| v.as_str())))
            .or_else(|| credentials.refresh_token())
            .or_else(|| credentials.access_token())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> KiroModelsService {
        KiroModelsService::new()
    }

    #[test]
    fn strips_synthetic_suffixes() {
        assert_eq!(KiroModelsService::strip_synthetic_suffixes("claude-sonnet-4.5-thinking"), "claude-sonnet-4.5");
        assert_eq!(KiroModelsService::strip_synthetic_suffixes("auto-agentic"), "auto");
        assert_eq!(
            KiroModelsService::strip_synthetic_suffixes("glm-5-thinking-agentic"),
            "glm-5"
        );
    }

    #[test]
    fn region_from_profile_arn() {
        assert_eq!(
            KiroModelsService::region_from_profile_arn(Some("arn:aws:codewhisperer:eu-central-1:1:profile/A")),
            "eu-central-1"
        );
        assert_eq!(KiroModelsService::region_from_profile_arn(None), "us-east-1");
        assert_eq!(KiroModelsService::region_from_profile_arn(Some("bogus")), "us-east-1");
    }

    #[test]
    fn variants_for_regular_model() {
        let variants = KiroModelsService::build_variants("claude-sonnet-4.5", None);
        let ids: Vec<&str> = variants
            .iter()
            .filter_map(|v| v.get("id").and_then(JsonValue::as_str))
            .collect();
        assert_eq!(
            ids,
            vec![
                "claude-sonnet-4.5",
                "claude-sonnet-4.5-thinking",
                "claude-sonnet-4.5-agentic",
                "claude-sonnet-4.5-thinking-agentic",
            ]
        );
        assert_eq!(variants[0]["name"], "Kiro claude-sonnet-4.5");
    }

    #[test]
    fn auto_skips_agentic_variants() {
        let variants = KiroModelsService::build_variants("auto", None);
        let ids: Vec<&str> = variants
            .iter()
            .filter_map(|v| v.get("id").and_then(JsonValue::as_str))
            .collect();
        assert_eq!(ids, vec!["auto", "auto-thinking"]);
    }

    #[test]
    fn display_name_marks_non_unit_rate() {
        assert_eq!(
            KiroModelsService::format_display_name(Some("Sonnet 4.5"), "x", Some(1.0)),
            "Kiro Sonnet 4.5"
        );
        assert_eq!(
            KiroModelsService::format_display_name(Some("Sonnet 4.5"), "x", Some(1.4)),
            "Kiro Sonnet 4.5 (1.4x credit)"
        );
        assert_eq!(KiroModelsService::format_display_name(None, "glm-5", None), "Kiro glm-5");
    }

    #[test]
    fn fingerprint_headers_are_stable_per_seed() {
        let creds = Credentials::from_value(serde_json::json!({
            "providerSpecificData": { "clientId": "abc" },
            "accessToken": "tok",
        }));
        let a = KiroModelsService::build_kiro_fingerprint_headers(&creds);
        let b = KiroModelsService::build_kiro_fingerprint_headers(&creds);
        assert_eq!(a["User-Agent"], b["User-Agent"]);
        assert!(a["User-Agent"].contains("KiroIDE-0.10.32-"));
        assert!(a["User-Agent"].contains("os/windows#10.0.26200"));
        assert_eq!(a["x-amzn-kiro-agent-mode"], "vibe");
        assert_ne!(a["amz-sdk-invocation-id"], b["amz-sdk-invocation-id"]);
    }

    #[test]
    fn expands_raw_models_with_metadata() {
        let raw = vec![serde_json::json!({
            "modelId": "glm-5",
            "modelName": "GLM 5",
            "rateMultiplier": 0.4,
            "description": "fast",
            "tokenLimits": { "maxInputTokens": 128000 },
        })];
        let expanded = KiroModelsService::expand(&raw);
        assert_eq!(expanded.len(), 4);
        assert_eq!(expanded[0]["upstreamModelId"], "glm-5");
        assert_eq!(expanded[0]["contextLength"], 128000);
        assert_eq!(expanded[0]["description"], "fast");
        assert_eq!(expanded[0]["name"], "Kiro GLM 5 (0.4x credit)");
    }

    #[test]
    fn expand_skips_entries_without_id() {
        let raw = vec![serde_json::json!({"modelName": "no id"})];
        assert!(KiroModelsService::expand(&raw).is_empty());
    }

    #[test]
    fn clear_cache_empties_store() {
        let svc = service();
        let creds = Credentials::from_value(serde_json::json!({"accessToken": "t"}));
        svc.cache.lock().unwrap().insert(
            svc.cache_key(&creds),
            CatalogCacheEntry { expires_at: u64::MAX, models: vec![], raw_models: vec![] },
        );
        assert_eq!(svc.cache.lock().unwrap().len(), 1);
        svc.clear_cache();
        assert!(svc.cache.lock().unwrap().is_empty());
    }

    #[test]
    fn cache_key_prefers_profile_arn() {
        let svc = service();
        let a = Credentials::from_value(serde_json::json!({
            "accessToken": "1",
            "providerSpecificData": { "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/A" },
        }));
        let b = Credentials::from_value(serde_json::json!({
            "accessToken": "2",
            "providerSpecificData": { "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/A" },
        }));
        assert_eq!(svc.cache_key(&a), svc.cache_key(&b));
    }
}
