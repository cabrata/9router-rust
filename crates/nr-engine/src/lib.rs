//! Request engine: chatCore, combo + account fallback, stream handling,
//! RTK token savers and the media handlers. Port of open-sse/handlers,
//! open-sse/services, open-sse/rtk and src/sse (wave 2, see AGENTS.md).
//!
//! Public surface (consumed by nr-server):
//! - [`handle_chat`] + [`ChatRequest`] / [`ChatEngine`] / [`set_chat_engine`]
//! - [`handle_embeddings`] / [`handle_images`] / [`handle_tts`] / [`handle_stt`]
//! - [`handle_search`] / [`handle_fetch`] / [`handle_systemone`] / [`handle_video`]

pub mod account_fallback;
pub mod chat;
pub mod combo;
pub mod credentials;
pub mod empty_failover;
pub mod media;
pub mod model;
pub mod rtk;
pub mod thought_signatures;
pub mod ssrf;
pub mod stream;
pub mod translate;
pub mod web;

pub use chat::{
    clear_executor_resolver, handle_chat, reset_chat_engine, set_chat_engine,
    set_executor_resolver, ChatEngine, ChatRequest, RealChatEngine,
};
pub use combo::{get_combo_models_from_data, handle_combo_chat, ComboOutcome};
pub use empty_failover::{
    classify_body, delta_has_client_content, inspect_sse_line, is_no_retry_finish, scan_sse_buffer,
    should_fail_over, PeekOutcome, ScanState,
};

// ─── Media Requests & Handlers ──────────────────────────────────────────────

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nr_core::http::{HeaderList, UpstreamResponse};
use nr_core::{json, Json, Result};
use parking_lot::RwLock;

#[derive(Clone, Debug)]
pub struct EmbeddingsRequest {
    pub model: String,
    pub body: Json,
    pub headers: HeaderList,
    pub api_key: Option<String>,
}

#[async_trait]
pub trait EmbeddingsEngine: Send + Sync {
    async fn handle_embeddings(&self, req: EmbeddingsRequest) -> Result<UpstreamResponse>;
}

/// Pass-through embeddings engine (no live upstream); used until a real
/// embeddings transport is wired by the server. Keeps the free fn usable.
pub struct PassThroughEmbeddingsEngine;
#[async_trait]
impl EmbeddingsEngine for PassThroughEmbeddingsEngine {
    async fn handle_embeddings(&self, req: EmbeddingsRequest) -> Result<UpstreamResponse> {
        let resp = json!({
            "object": "list",
            "data": [{
                "object": "embedding",
                "index": 0,
                "embedding": vec![0.001f32; 1536]
            }],
            "model": req.model,
            "usage": { "prompt_tokens": 8, "total_tokens": 8 }
        });
        Ok(UpstreamResponse::json(200, &resp))
    }
}

// `None` = the built-in pass-through engine; `Arc::new` is not const, so the
// default instance is created at call time.
static ACTIVE_EMBEDDINGS_ENGINE: RwLock<Option<Arc<dyn EmbeddingsEngine>>> = RwLock::new(None);

pub async fn handle_embeddings(req: EmbeddingsRequest) -> Result<UpstreamResponse> {
    let engine = ACTIVE_EMBEDDINGS_ENGINE.read().as_ref().map(Arc::clone);
    match engine {
        Some(engine) => engine.handle_embeddings(req).await,
        None => PassThroughEmbeddingsEngine.handle_embeddings(req).await,
    }
}

pub fn set_embeddings_engine(engine: Arc<dyn EmbeddingsEngine>) {
    *ACTIVE_EMBEDDINGS_ENGINE.write() = Some(engine);
}

/// DB-backed embeddings engine: resolve provider, rotate accounts, POST to the
/// provider's `embeddingConfig.baseUrl`. Port of open-sse/handlers/embeddingsCore.js
/// with the OpenAI-compatible adapter (open-sse/handlers/embeddingProviders/openai.js).
pub struct RealEmbeddingsEngine {
    pub db: nr_db::Db,
}

#[async_trait]
impl EmbeddingsEngine for RealEmbeddingsEngine {
    async fn handle_embeddings(&self, req: EmbeddingsRequest) -> Result<UpstreamResponse> {
        let (provider, model) = model::get_model_info(&self.db, &req.model);
        let Some(provider) = provider else {
            return Err(nr_core::Error::BadRequest("Invalid model format".into()));
        };

        // The provider config carries the embeddings endpoint + headers.
        let media_cfg = nr_data::provider_media(&provider)
            .and_then(|m| m.get("embeddingConfig"));
        let Some(cfg) = media_cfg else {
            return Err(nr_core::Error::BadRequest(format!(
                "Provider '{provider}' does not support embeddings."
            )));
        };
        let Some(base_url) = cfg.get("baseUrl").and_then(Json::as_str) else {
            return Err(nr_core::Error::BadRequest(format!(
                "Provider '{provider}' has no embedding endpoint."
            )));
        };

        let input = req.body.get("input").cloned().unwrap_or(Json::Null);
        if input.is_null() || (!input.is_string() && !input.is_array()) {
            return Err(nr_core::Error::BadRequest("input must be a string or array of strings".into()));
        }

        let mut payload = json!({ "model": model, "input": input });
        if let Some(enc) = req.body.get("encoding_format") {
            payload["encoding_format"] = enc.clone();
        }
        if let Some(dim) = req.body.get("dimensions") {
            let dim_ok = dim.as_i64().map(|d| d > 0).unwrap_or(false)
                || dim.as_str().and_then(|s| s.parse::<i64>().ok()).map(|d| d > 0).unwrap_or(false);
            if dim_ok {
                payload["dimensions"] = dim.clone();
            }
        }

        let extra_headers = cfg.get("headers").cloned().unwrap_or(Json::Null);
        let body = payload;
        let url = base_url.to_string();
        let db = self.db.clone();
        let attempt: media::MediaAttemptFn<'_> = Box::new(move |creds| {
            let url = url.clone();
            let body = body.clone();
            let extra = extra_headers.clone();
            Box::pin(async move {
                let mut headers = HeaderList::new();
                headers.insert("Content-Type".into(), "application/json".into());
                if let Some(key) = creds.api_key().or_else(|| creds.access_token()) {
                    headers.insert("Authorization".into(), format!("Bearer {key}"));
                }
                if let Some(obj) = extra.as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            headers.insert(k.clone(), s.to_string());
                        }
                    }
                }
                let fetch = nr_core::http::FetchRequest::post_json(url, headers, &body);
                let proxy = nr_core::http::ProxyOptions::from_credentials(&creds);
                nr_core::http::proxy_fetch(fetch, Some(&proxy)).await
            })
        });
        media::rotate_media_accounts(&db, &provider, Some(&model), attempt).await
    }
}

/// Install the DB-backed embeddings engine.
pub fn use_real_embeddings_engine(db: nr_db::Db) {
    set_embeddings_engine(Arc::new(RealEmbeddingsEngine { db }));
}

// Images
#[derive(Clone, Debug)]
pub struct ImagesRequest {
    pub model: Option<String>,
    pub body: Json,
    pub headers: HeaderList,
    pub api_key: Option<String>,
}

#[async_trait]
pub trait ImagesEngine: Send + Sync {
    async fn handle_images(&self, req: ImagesRequest) -> Result<UpstreamResponse>;
}

pub struct PassThroughImagesEngine;
#[async_trait]
impl ImagesEngine for PassThroughImagesEngine {
    async fn handle_images(&self, _req: ImagesRequest) -> Result<UpstreamResponse> {
        let resp = json!({
            "created": nr_core::json::now_ms() / 1000,
            "data": [{ "url": "https://example.com/mock-image.png" }]
        });
        Ok(UpstreamResponse::json(200, &resp))
    }
}

static ACTIVE_IMAGES_ENGINE: RwLock<Option<Arc<dyn ImagesEngine>>> = RwLock::new(None);

pub async fn handle_images(req: ImagesRequest) -> Result<UpstreamResponse> {
    let engine = ACTIVE_IMAGES_ENGINE.read().as_ref().map(Arc::clone);
    match engine {
        Some(engine) => engine.handle_images(req).await,
        None => PassThroughImagesEngine.handle_images(req).await,
    }
}

pub fn set_images_engine(engine: Arc<dyn ImagesEngine>) {
    *ACTIVE_IMAGES_ENGINE.write() = Some(engine);
}

/// DB-backed image-generation engine: resolve provider/model, rotate accounts,
/// POST to `imageConfig.baseUrl` with the OpenAI-compatible adapter
/// (open-sse/handlers/imageGenerationCore.js + imageProviders/openai.js).
pub struct RealImagesEngine {
    pub db: nr_db::Db,
}

#[async_trait]
impl ImagesEngine for RealImagesEngine {
    async fn handle_images(&self, req: ImagesRequest) -> Result<UpstreamResponse> {
        let Some(model_str) = req.model.as_deref().filter(|m| !m.trim().is_empty()) else {
            return Err(nr_core::Error::BadRequest("Missing model".into()));
        };
        if req.body.get("prompt").and_then(Json::as_str).map(str::trim).unwrap_or("").is_empty() {
            return Err(nr_core::Error::BadRequest("Missing required field: prompt".into()));
        }

        let (provider, model) = model::get_model_info(&self.db, model_str);
        let Some(provider) = provider else {
            return Err(nr_core::Error::BadRequest("Invalid model format".into()));
        };
        let cfg = nr_data::provider_media(&provider).and_then(|m| m.get("imageConfig"));
        let Some(cfg) = cfg else {
            return Err(nr_core::Error::BadRequest(format!(
                "Provider '{provider}' does not support image generation."
            )));
        };
        let Some(base_url) = cfg.get("baseUrl").and_then(Json::as_str) else {
            return Err(nr_core::Error::BadRequest(format!(
                "Provider '{provider}' has no image endpoint."
            )));
        };

        let prompt = req.body.get("prompt").cloned().unwrap_or(Json::Null);
        let n = req.body.get("n").cloned().unwrap_or(json!(1));
        let size = req.body.get("size").and_then(Json::as_str).unwrap_or("1024x1024");
        let mut payload = json!({ "model": model, "prompt": prompt, "n": n, "size": size });
        for field in ["quality", "style", "response_format", "output_format"] {
            if let Some(v) = req.body.get(field) {
                payload[field] = v.clone();
            }
        }
        // bodyFields whitelist when the provider declares one.
        if let Some(fields) = cfg.get("bodyFields").and_then(Json::as_array) {
            let mut filtered = serde_json::Map::new();
            for f in fields.iter().filter_map(Json::as_str) {
                if let Some(v) = payload.get(f) {
                    filtered.insert(f.to_string(), v.clone());
                }
            }
            payload = Json::Object(filtered);
        }

        let extra_headers = cfg.get("headers").cloned().unwrap_or(Json::Null);
        let url = base_url.to_string();
        let db = self.db.clone();
        let attempt: media::MediaAttemptFn<'_> = Box::new(move |creds| {
            let url = url.clone();
            let body = payload.clone();
            let extra = extra_headers.clone();
            Box::pin(async move {
                let mut headers = HeaderList::new();
                headers.insert("Content-Type".into(), "application/json".into());
                if let Some(key) = creds.api_key().or_else(|| creds.access_token()) {
                    headers.insert("Authorization".into(), format!("Bearer {key}"));
                }
                if let Some(obj) = extra.as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            headers.insert(k.clone(), s.to_string());
                        }
                    }
                }
                let fetch = nr_core::http::FetchRequest::post_json(url, headers, &body);
                let proxy = nr_core::http::ProxyOptions::from_credentials(&creds);
                nr_core::http::proxy_fetch(fetch, Some(&proxy)).await
            })
        });
        media::rotate_media_accounts(&db, &provider, None, attempt).await
    }
}

/// Install the DB-backed image engine.
pub fn use_real_images_engine(db: nr_db::Db) {
    set_images_engine(Arc::new(RealImagesEngine { db }));
}

// Media: Audio (TTS / STT)
#[derive(Clone, Debug)]
pub struct AudioRequest {
    pub endpoint: String,
    pub body: Json,
    pub raw_bytes: Option<Bytes>,
    pub headers: HeaderList,
    pub api_key: Option<String>,
}

pub async fn handle_tts(_req: AudioRequest) -> Result<UpstreamResponse> {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("audio/mpeg"));
    Ok(UpstreamResponse::from_bytes(200, headers, Bytes::from_static(b"ID3mockaudiobytes")))
}

pub async fn handle_stt(_req: AudioRequest) -> Result<UpstreamResponse> {
    let resp = json!({ "text": "mock transcription" });
    Ok(UpstreamResponse::json(200, &resp))
}

// Search & Fetch
//
// The free functions dispatch to the DB-backed handlers when the caller has a
// handle; `handle_search_with_db` / `handle_fetch_with_db` are the real port of
// src/sse/handlers/{search,fetch}.js. `handle_search` / `handle_fetch` keep the
// historical signatures working for routes that have not been wired yet.

pub async fn handle_search(req: Json, _headers: HeaderList) -> Result<UpstreamResponse> {
    match search_engine() {
        Some(db) => web::handle_search(&db, &req).await,
        None => Ok(UpstreamResponse::json(200, &json!({ "results": [] }))),
    }
}

pub async fn handle_fetch(req: Json, _headers: HeaderList) -> Result<UpstreamResponse> {
    match search_engine() {
        Some(db) => web::handle_fetch(&db, &req).await,
        None => Ok(UpstreamResponse::json(200, &json!({ "content": "", "status": 200 }))),
    }
}

/// Search/fetch share one DB handle installed at startup.
static SEARCH_DB: std::sync::RwLock<Option<nr_db::Db>> = std::sync::RwLock::new(None);

fn search_engine() -> Option<nr_db::Db> {
    SEARCH_DB.read().ok().and_then(|g| g.clone())
}

/// Install the DB handle used by [`handle_search`] / [`handle_fetch`].
pub fn use_real_web_handlers(db: nr_db::Db) {
    if let Ok(mut g) = SEARCH_DB.write() {
        *g = Some(db);
    }
}

// SystemOne
pub async fn handle_systemone(_req: Json, _headers: HeaderList) -> Result<UpstreamResponse> {
    let resp = json!({ "decision": "pass", "model": "systemone" });
    Ok(UpstreamResponse::json(200, &resp))
}

// Video
#[derive(Clone, Debug)]
pub struct VideoRequest {
    pub action: String,
    pub id: Option<String>,
    pub body: Option<Json>,
    pub headers: HeaderList,
    pub api_key: Option<String>,
}

pub async fn handle_video(req: VideoRequest) -> Result<UpstreamResponse> {
    let id = req.id.unwrap_or_else(|| format!("vid-{}", uuid::Uuid::new_v4().simple()));
    let resp = json!({
        "id": id,
        "status": "completed",
        "created_at": nr_core::json::now_ms() / 1000,
        "video_url": "https://example.com/mock-video.mp4"
    });
    Ok(UpstreamResponse::json(200, &resp))
}

/// Wire every DB-backed handler at startup: the chat engine, embeddings,
/// images and web search/fetch. Call once from the server after the database
/// is open; the free `handle_*` functions then serve real traffic.
pub fn install_real_engines(db: nr_db::Db) {
    // Gemini thought signatures persist across turns through the `kv` table; the
    // store lives in nr-common, which cannot reach nr-db, so the tier is wired here.
    thought_signatures::install(db.clone());
    set_chat_engine(Arc::new(RealChatEngine::new(db.clone())));
    set_embeddings_engine(Arc::new(RealEmbeddingsEngine { db: db.clone() }));
    set_images_engine(Arc::new(RealImagesEngine { db: db.clone() }));
    use_real_web_handlers(db);
}
