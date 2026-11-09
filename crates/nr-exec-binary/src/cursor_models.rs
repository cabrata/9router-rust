//! Port of open-sse/services/cursorModels.js
//! Cursor live model catalog fetcher (AgentService `GetUsableModels`).

use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, ProxyOptions};
use nr_core::{Credentials, Error, Json, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::cursor_checksum::build_cursor_headers;
use crate::cursor_protobuf::decode_message;

const FETCH_TIMEOUT_MS: u64 = 10_000;
const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

// agent.v1.ModelDetails protobuf field numbers.
const MODEL_ID_FIELD: u32 = 1;
const DISPLAY_MODEL_ID_FIELD: u32 = 3;
const DISPLAY_NAME_FIELD: u32 = 4;
const DISPLAY_NAME_SHORT_FIELD: u32 = 5;
const RESPONSE_MODELS_FIELD: u32 = 1;

// agent.v1.AvailableModels (response field 2) carries the model-picker flags
// AgentService honours: agent support, chat-only, hidden, and the account's
// default-on pick used to resolve the "Auto (Server Picks)" entry.
const RESPONSE_AVAILABLE_MODELS_FIELD: u32 = 2;
const AVAILABLE_MODEL_NAME_FIELD: u32 = 1;
const AVAILABLE_MODEL_DEFAULT_ON_FIELD: u32 = 2;
const AVAILABLE_MODEL_IS_CHAT_ONLY_FIELD: u32 = 4;
const AVAILABLE_MODEL_SUPPORTS_AGENT_FIELD: u32 = 5;
const AVAILABLE_MODEL_IS_HIDDEN_FIELD: u32 = 35;

struct CacheEntry {
    expires_at: u64,
    models: Vec<Json>,
}

static CATALOG_CACHE: LazyLock<Arc<Mutex<HashMap<String, CacheEntry>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `${PROVIDER_OAUTH.cursor.agentEndpoint}/${modelsEndpoint}` (trailing `/` stripped).
fn cursor_models_url() -> Option<String> {
    let config = nr_data::provider_oauth("cursor")?;
    let agent_endpoint = config.get("agentEndpoint").and_then(Json::as_str)?;
    let models_endpoint = config.get("modelsEndpoint").and_then(Json::as_str)?;
    Some(format!("{}{}", agent_endpoint.trim_end_matches('/'), models_endpoint))
}

fn cache_key(creds: &Credentials) -> String {
    let machine_id = creds.psd_str("machineId").unwrap_or("");
    let access_token = creds.access_token().unwrap_or("");
    let seed = [machine_id, access_token]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(":");
    if seed.is_empty() {
        return "cursor-anonymous".to_string();
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("cursor:{seed}").as_bytes());
    format!("{:x}", hasher.finalize())
}

fn first_string(fields: &std::collections::BTreeMap<u32, Vec<crate::cursor_protobuf::DecodedField>>, field: u32) -> String {
    fields
        .get(&field)
        .and_then(|v| v.first())
        .filter(|f| f.wire_type != crate::cursor_protobuf::WIRE_TYPE_VARINT)
        .map(|f| String::from_utf8_lossy(&f.value).to_string())
        .unwrap_or_default()
}

fn first_bool(fields: &std::collections::BTreeMap<u32, Vec<crate::cursor_protobuf::DecodedField>>, field: u32) -> bool {
    fields
        .get(&field)
        .and_then(|v| v.first())
        .map(|f| !f.value.is_empty() && f.value[0] == 1)
        .unwrap_or(false)
}

/// Decode `agent.v1.GetUsableModelsResponse`: repeated ModelDetails in field 1.
pub fn parse_cursor_usable_models(payload: &[u8]) -> Vec<Json> {
    let response = decode_message(payload);
    let mut seen: Vec<String> = Vec::new();
    let mut models = Vec::new();

    for entry in response.get(&RESPONSE_MODELS_FIELD).map(Vec::as_slice).unwrap_or(&[]) {
        if entry.wire_type == crate::cursor_protobuf::WIRE_TYPE_VARINT {
            continue;
        }
        let detail = decode_message(&entry.value);
        let id = first_string(&detail, MODEL_ID_FIELD).trim().to_string();
        if id.is_empty() || seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());

        let name = {
            let candidates = [
                first_string(&detail, DISPLAY_NAME_FIELD),
                first_string(&detail, DISPLAY_NAME_SHORT_FIELD),
                first_string(&detail, DISPLAY_MODEL_ID_FIELD),
            ];
            candidates
                .into_iter()
                .find(|c| !c.trim().is_empty())
                .map(|c| c.trim().to_string())
                .unwrap_or_else(|| id.clone())
        };
        models.push(json!({ "id": id, "name": name }));
    }

    models
}

/// Decode the model-picker catalog (response field 2, agent.v1.AvailableModels).
pub fn parse_cursor_available_models(payload: &[u8]) -> Vec<Json> {
    let response = decode_message(payload);
    let mut seen: Vec<String> = Vec::new();
    let mut models = Vec::new();

    for entry in response.get(&RESPONSE_AVAILABLE_MODELS_FIELD).map(Vec::as_slice).unwrap_or(&[]) {
        if entry.wire_type == crate::cursor_protobuf::WIRE_TYPE_VARINT {
            continue;
        }
        let fields = decode_message(&entry.value);
        let id = first_string(&fields, AVAILABLE_MODEL_NAME_FIELD).trim().to_string();
        if id.is_empty() || seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());

        models.push(json!({
            "id": id,
            "defaultOn": first_bool(&fields, AVAILABLE_MODEL_DEFAULT_ON_FIELD),
            "isChatOnly": first_bool(&fields, AVAILABLE_MODEL_IS_CHAT_ONLY_FIELD),
            "supportsAgent": first_bool(&fields, AVAILABLE_MODEL_SUPPORTS_AGENT_FIELD),
            "isHidden": first_bool(&fields, AVAILABLE_MODEL_IS_HIDDEN_FIELD),
        }));
    }

    models
}

/// Attach picker flags onto the usable-models list (field 2 only, when sent).
pub fn merge_cursor_model_flags(models: &[Json], payload: &[u8]) -> Vec<Json> {
    let available = parse_cursor_available_models(payload);
    if available.is_empty() {
        return models.to_vec();
    }
    let mut flags_by_id: HashMap<String, Json> = HashMap::new();
    for model in available {
        if let Some(id) = model.get("id").and_then(Json::as_str) {
            flags_by_id.insert(id.to_string(), model);
        }
    }
    models
        .iter()
        .map(|model| {
            let id = model.get("id").and_then(Json::as_str).unwrap_or("");
            match flags_by_id.get(id) {
                Some(flags) => {
                    let mut merged = model.clone();
                    if let (Some(dst), Some(src)) = (merged.as_object_mut(), flags.as_object()) {
                        for (k, v) in src {
                            dst.insert(k.clone(), v.clone());
                        }
                    }
                    merged
                }
                None => model.clone(),
            }
        })
        .collect()
}

/// `resolveAutoModelSelection`: pick the account's default agent-capable model.
pub fn resolve_auto_model_selection(models: &[Json]) -> Option<Json> {
    let usable: Vec<&Json> = models
        .iter()
        .filter(|m| {
            m.get("supportsAgent").and_then(Json::as_bool).unwrap_or(false)
                && !m.get("isChatOnly").and_then(Json::as_bool).unwrap_or(false)
                && !m.get("isHidden").and_then(Json::as_bool).unwrap_or(false)
        })
        .collect();
    let preferred = usable
        .iter()
        .find(|m| m.get("defaultOn").and_then(Json::as_bool).unwrap_or(false))
        .or_else(|| usable.first())?;
    Some(json!({
        "modelId": preferred.get("id").cloned().unwrap_or(Json::Null),
        "matchedBy": "auto-default",
    }))
}

/// Fetch the catalog over the AgentService endpoint.
async fn fetch_cursor_catalog(
    creds: &Credentials,
    proxy: Option<&ProxyOptions>,
) -> Result<Vec<Json>> {
    let access_token = creds.access_token().ok_or_else(|| Error::Config("Cursor: no access token".into()))?;
    let machine_id = creds.psd_str("machineId").ok_or_else(|| Error::Config("Cursor: no machine id".into()))?;
    let url = cursor_models_url().ok_or_else(|| Error::Config("Cursor: models endpoint is not configured".into()))?;
    let ghost_mode = creds.psd().and_then(|p| p.get("ghostMode")).and_then(Json::as_bool).unwrap_or(true);

    let mut headers = HeaderList::new();
    for (k, v) in build_cursor_headers(access_token, Some(machine_id), ghost_mode) {
        headers.insert(k, v);
    }
    // Connect unary calls use an unframed protobuf body, unlike Cursor chat's
    // streaming `application/connect+proto` endpoint.
    headers.insert("accept".into(), "application/proto".into());
    headers.insert("content-type".into(), "application/proto".into());
    headers.remove("connect-accept-encoding");
    headers.remove("connect-protocol-version");

    let mut fetch = FetchRequest::new(http::Method::POST, url);
    fetch.headers = headers;
    fetch.body = Some(bytes::Bytes::new());
    fetch.header_timeout = Some(std::time::Duration::from_millis(FETCH_TIMEOUT_MS));

    let response = proxy_fetch(fetch, proxy).await?;
    if response.status != 200 {
        return Err(Error::Network(format!("Cursor GetUsableModels returned {}", response.status)));
    }
    let body = response.bytes().await?;
    Ok(merge_cursor_model_flags(&parse_cursor_usable_models(&body), &body))
}

/// Resolve the live Cursor catalog. `Ok(None)` on any failure so callers fall
/// back to the static registry models.
pub async fn resolve_cursor_models(
    credentials: &Credentials,
    force_refresh: bool,
    proxy: Option<&ProxyOptions>,
) -> Result<Option<Json>> {
    if credentials.access_token().is_none() || credentials.psd_str("machineId").is_none() {
        return Ok(None);
    }

    let key = cache_key(credentials);
    let now = now_ms();
    if !force_refresh {
        if let Ok(cache) = CATALOG_CACHE.lock() {
            if let Some(entry) = cache.get(&key) {
                if entry.expires_at > now {
                    return Ok(Some(json!({ "models": entry.models })));
                }
            }
        }
    }

    let models = match fetch_cursor_catalog(credentials, proxy).await {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };

    if let Ok(mut cache) = CATALOG_CACHE.lock() {
        cache.insert(
            key,
            CacheEntry { expires_at: now + CACHE_TTL_MS, models: models.clone() },
        );
    }
    Ok(Some(json!({ "models": models })))
}

/// `clearCursorModelCache`.
pub fn clear_cursor_model_cache() {
    if let Ok(mut cache) = CATALOG_CACHE.lock() {
        cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor_protobuf::{encode_varint, WIRE_TYPE_LEN};

    fn len_field(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_varint(((field << 3) | WIRE_TYPE_LEN as u32) as u64);
        out.extend(encode_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn text_field(field: u32, text: &str) -> Vec<u8> {
        len_field(field, text.as_bytes())
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = encode_varint(((field << 3) | crate::cursor_protobuf::WIRE_TYPE_VARINT as u32) as u64);
        out.extend(encode_varint(value));
        out
    }

    #[test]
    fn parses_usable_models() {
        let details = [
            text_field(MODEL_ID_FIELD, "composer-1"),
            text_field(DISPLAY_NAME_FIELD, "Composer 1"),
        ]
        .concat();
        let payload = len_field(RESPONSE_MODELS_FIELD, &details);
        let models = parse_cursor_usable_models(&payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "composer-1");
        assert_eq!(models[0]["name"], "Composer 1");
    }

    #[test]
    fn dedupes_and_falls_back_to_id_name() {
        let detail = text_field(MODEL_ID_FIELD, "gpt-5");
        let payload = [len_field(RESPONSE_MODELS_FIELD, &detail), len_field(RESPONSE_MODELS_FIELD, &detail)].concat();
        let models = parse_cursor_usable_models(&payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"], "gpt-5");
    }

    #[test]
    fn parses_available_model_flags() {
        let entry = [
            text_field(AVAILABLE_MODEL_NAME_FIELD, "composer-1"),
            varint_field(AVAILABLE_MODEL_DEFAULT_ON_FIELD, 1),
            varint_field(AVAILABLE_MODEL_SUPPORTS_AGENT_FIELD, 1),
        ]
        .concat();
        let payload = len_field(RESPONSE_AVAILABLE_MODELS_FIELD, &entry);
        let models = parse_cursor_available_models(&payload);
        assert_eq!(models[0]["defaultOn"], true);
        assert_eq!(models[0]["supportsAgent"], true);
        assert_eq!(models[0]["isHidden"], false);
    }

    #[test]
    fn merges_flags_onto_usable_models() {
        let usable = vec![json!({"id": "composer-1", "name": "Composer 1"})];
        let entry = [
            text_field(AVAILABLE_MODEL_NAME_FIELD, "composer-1"),
            varint_field(AVAILABLE_MODEL_SUPPORTS_AGENT_FIELD, 1),
            varint_field(AVAILABLE_MODEL_DEFAULT_ON_FIELD, 1),
        ]
        .concat();
        let payload = len_field(RESPONSE_AVAILABLE_MODELS_FIELD, &entry);
        let merged = merge_cursor_model_flags(&usable, &payload);
        assert_eq!(merged[0]["name"], "Composer 1");
        assert_eq!(merged[0]["supportsAgent"], true);
    }

    #[test]
    fn auto_selection_prefers_default_on_agent_model() {
        let models = vec![
            json!({"id": "hidden", "supportsAgent": true, "isHidden": true}),
            json!({"id": "chat", "supportsAgent": true, "isChatOnly": true}),
            json!({"id": "second", "supportsAgent": true}),
            json!({"id": "default", "supportsAgent": true, "defaultOn": true}),
        ];
        let selection = resolve_auto_model_selection(&models).unwrap();
        assert_eq!(selection["modelId"], "default");
        assert_eq!(selection["matchedBy"], "auto-default");
    }

    #[test]
    fn auto_selection_none_without_agent_models() {
        let models = vec![json!({"id": "chat", "isChatOnly": true, "supportsAgent": true})];
        assert!(resolve_auto_model_selection(&models).is_none());
    }

    #[test]
    fn cache_key_differs_per_token() {
        let a = Credentials::from_value(json!({"accessToken": "a", "providerSpecificData": {"machineId": "m"}}));
        let b = Credentials::from_value(json!({"accessToken": "b", "providerSpecificData": {"machineId": "m"}}));
        assert_ne!(cache_key(&a), cache_key(&b));
        let anon = Credentials::from_value(json!({}));
        assert_eq!(cache_key(&anon), "cursor-anonymous");
    }
}
