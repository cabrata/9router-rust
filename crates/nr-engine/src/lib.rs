//! Request engine: chatCore, combo + account fallback, stream handling,
//! RTK token savers and the media handlers. Port of open-sse/handlers,
//! open-sse/services (rest), open-sse/rtk, open-sse/utils (rest) and src/sse.
//! Wave 2 (see AGENTS.md).
//!
//! Provides minimal stub traits and default pass-through implementations
//! for the HTTP gateway routes.

use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, Stream};
use nr_core::http::{HeaderList, UpstreamResponse};
use nr_core::{json, Json, Result};
use parking_lot::RwLock;

/// Chat request passed from the HTTP gateway to the engine.
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub body: Json,
    pub stream: bool,
    pub headers: HeaderList,
    pub endpoint: String,
    pub api_key: Option<String>,
}

#[async_trait]
pub trait ChatEngine: Send + Sync {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse>;
}

/// Default pass-through chat engine for testing and stubbing.
pub struct PassThroughChatEngine;

#[async_trait]
impl ChatEngine for PassThroughChatEngine {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse> {
        let created = nr_core::json::now_ms() / 1000;
        let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());

        if req.stream {
            let chunk1 = format!(
                "data: {}\n\n",
                json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": req.model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "content": "Hello! I am a pass-through response from 9router."
                        },
                        "finish_reason": null
                    }]
                })
            );
            let chunk2 = format!(
                "data: {}\n\n",
                json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": req.model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 10,
                        "total_tokens": 20
                    }
                })
            );
            let chunk3 = "data: [DONE]\n\n".to_string();

            let items = vec![
                Ok(Bytes::from(chunk1)),
                Ok(Bytes::from(chunk2)),
                Ok(Bytes::from(chunk3)),
            ];
            let body_stream: Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static>> =
                Box::pin(stream::iter(items));
            Ok(UpstreamResponse::sse(body_stream))
        } else {
            let resp = json!({
                "id": id,
                "object": "chat.completion",
                "created": created,
                "model": req.model,
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello! I am a pass-through response from 9router."
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 10,
                    "total_tokens": 20
                }
            });
            Ok(UpstreamResponse::json(200, &resp))
        }
    }
}

static ACTIVE_CHAT_ENGINE: LazyLock<RwLock<Arc<dyn ChatEngine>>> =
    LazyLock::new(|| RwLock::new(Arc::new(PassThroughChatEngine)));

/// Execute a chat request with the currently active engine.
pub async fn handle_chat(req: ChatRequest) -> Result<UpstreamResponse> {
    let engine = ACTIVE_CHAT_ENGINE.read().clone();
    engine.handle_chat(req).await
}

/// Set a custom chat engine (e.g. for testing with mocks).
pub fn set_chat_engine(engine: Arc<dyn ChatEngine>) {
    *ACTIVE_CHAT_ENGINE.write() = engine;
}

/// Reset to the default pass-through chat engine.
pub fn reset_chat_engine() {
    *ACTIVE_CHAT_ENGINE.write() = Arc::new(PassThroughChatEngine);
}

// ─── Media Requests & Handlers ──────────────────────────────────────────────

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
            "usage": {
                "prompt_tokens": 8,
                "total_tokens": 8
            }
        });
        Ok(UpstreamResponse::json(200, &resp))
    }
}

static ACTIVE_EMBEDDINGS_ENGINE: LazyLock<RwLock<Arc<dyn EmbeddingsEngine>>> =
    LazyLock::new(|| RwLock::new(Arc::new(PassThroughEmbeddingsEngine)));

pub async fn handle_embeddings(req: EmbeddingsRequest) -> Result<UpstreamResponse> {
    let engine = ACTIVE_EMBEDDINGS_ENGINE.read().clone();
    engine.handle_embeddings(req).await
}

pub fn set_embeddings_engine(engine: Arc<dyn EmbeddingsEngine>) {
    *ACTIVE_EMBEDDINGS_ENGINE.write() = engine;
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
            "data": [{
                "url": "https://example.com/mock-image.png"
            }]
        });
        Ok(UpstreamResponse::json(200, &resp))
    }
}

static ACTIVE_IMAGES_ENGINE: LazyLock<RwLock<Arc<dyn ImagesEngine>>> =
    LazyLock::new(|| RwLock::new(Arc::new(PassThroughImagesEngine)));

pub async fn handle_images(req: ImagesRequest) -> Result<UpstreamResponse> {
    let engine = ACTIVE_IMAGES_ENGINE.read().clone();
    engine.handle_images(req).await
}

pub fn set_images_engine(engine: Arc<dyn ImagesEngine>) {
    *ACTIVE_IMAGES_ENGINE.write() = engine;
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
pub async fn handle_search(_req: Json, _headers: HeaderList) -> Result<UpstreamResponse> {
    let resp = json!({ "results": [] });
    Ok(UpstreamResponse::json(200, &resp))
}

pub async fn handle_fetch(_req: Json, _headers: HeaderList) -> Result<UpstreamResponse> {
    let resp = json!({ "content": "", "status": 200 });
    Ok(UpstreamResponse::json(200, &resp))
}

// SystemOne
pub async fn handle_systemone(_req: Json, _headers: HeaderList) -> Result<UpstreamResponse> {
    let resp = json!({ "decision": "pass", "model": "systemone" });
    Ok(UpstreamResponse::json(200, &resp))
}

// Video
#[derive(Clone, Debug)]
pub struct VideoRequest {
    pub action: String, // "generations", "edits", "extensions", "get"
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
