//! Media, search, fetch, and video endpoints.
//!
//! Port of:
//! - `src/app/api/v1/embeddings/route.js`
//! - `src/app/api/v1/images/generations/route.js`
//! - `src/app/api/v1/audio/speech/route.js`
//! - `src/app/api/v1/audio/transcriptions/route.js`
//! - `src/app/api/v1/audio/voices/route.js`
//! - `src/app/api/v1/search/route.js`
//! - `src/app/api/v1/web/fetch/route.js`
//! - `src/app/api/v1/systemone/route.js`
//! - `src/app/api/v1/videos/**/route.js`

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nr_core::http::HeaderList;
use nr_core::json;
use nr_db::repos::{get_api_key_by_key, get_settings};
use nr_db::Db;
use nr_engine::{
    handle_embeddings, handle_fetch, handle_images, handle_search, handle_stt, handle_systemone,
    handle_tts, handle_video, AudioRequest, EmbeddingsRequest, ImagesRequest, VideoRequest,
};
use serde_json::Value;

use crate::support;

pub fn routes() -> Router {
    Router::new()
        .route("/v1/embeddings", post(post_embeddings).options(options_post))
        .route("/v1/images/generations", post(post_images).options(options_post))
        .route("/v1/audio/speech", post(post_audio_speech).options(options_post))
        .route("/v1/audio/transcriptions", post(post_audio_transcriptions).options(options_post))
        .route("/v1/audio/voices", get(get_audio_voices).options(options_get))
        .route("/v1/search", post(post_search).options(options_post))
        .route("/v1/web/fetch", post(post_fetch).options(options_post))
        .route("/v1/systemone", post(post_systemone).options(options_post))
        .route("/v1/videos/generations", post(post_video_generations).options(options_post))
        .route("/v1/videos/edits", post(post_video_edits).options(options_post))
        .route("/v1/videos/extensions", post(post_video_extensions).options(options_post))
        .route("/v1/videos/{id}", get(get_video_by_id).options(options_get))
}

async fn options_post() -> Response {
    support::cors_headers("POST, OPTIONS")
}

async fn options_get() -> Response {
    support::cors_headers("GET, OPTIONS")
}

fn header_list(headers: &HeaderMap) -> HeaderList {
    let mut out = HeaderList::new();
    for (name, value) in headers {
        if let Ok(v) = value.to_str() {
            out.insert(name.as_str().to_string(), v.to_string());
        }
    }
    out
}

async fn check_auth(db: &Db, headers: &HeaderMap, uri: &axum::http::Uri) -> Option<Response> {
    let settings = get_settings(db).unwrap_or_else(|_| json!({}));
    let require_api_key = settings.get("requireApiKey").and_then(Value::as_bool).unwrap_or(true);
    let key = support::extract_key(headers, support::query_api_key(uri).as_deref());

    if !require_api_key {
        if let Some(k) = &key {
            if let Ok(Some(rec)) = get_api_key_by_key(db, k) {
                if !rec.is_active {
                    return Some(support::error_response(401, "API key is inactive"));
                }
            }
        }
        return None;
    }

    let Some(k) = key else {
        return Some(support::error_response(401, "Missing API key"));
    };
    let Ok(Some(rec)) = get_api_key_by_key(db, &k) else {
        return Some(support::error_response(401, "Invalid API key"));
    };
    if !rec.is_active {
        return Some(support::error_response(401, "API key is inactive"));
    }
    if let Some(exp) = &rec.expired_at {
        if let Some(ts) = nr_core::json::parse_time_ms(&json!(exp.as_str())) {
            if ts <= nr_core::json::now_ms() {
                return Some(support::error_response(401, "API key has expired"));
            }
        }
    }
    None
}

// ─── Embeddings ─────────────────────────────────────────────────────────────

async fn post_embeddings(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => return support::error_response(400, "Missing model"),
    };
    if body.get("input").is_none() {
        return support::error_response(400, "Missing required field: input");
    }

    let req = EmbeddingsRequest {
        model,
        body,
        headers: header_list(&headers),
        api_key: support::extract_key(&headers, support::query_api_key(&uri).as_deref()),
    };
    match handle_embeddings(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

// ─── Images ─────────────────────────────────────────────────────────────────

async fn post_images(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    let req = ImagesRequest {
        model: body.get("model").and_then(Value::as_str).map(String::from),
        body,
        headers: header_list(&headers),
        api_key: support::extract_key(&headers, support::query_api_key(&uri).as_deref()),
    };
    match handle_images(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

// ─── Audio (TTS, STT, Voices) ───────────────────────────────────────────────

async fn post_audio_speech(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    let req = AudioRequest {
        endpoint: "/v1/audio/speech".into(),
        body,
        raw_bytes: None,
        headers: header_list(&headers),
        api_key: support::extract_key(&headers, support::query_api_key(&uri).as_deref()),
    };
    match handle_tts(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

async fn post_audio_transcriptions(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    let body = body.map(|Json(b)| b).unwrap_or_else(|| json!({}));
    let req = AudioRequest {
        endpoint: "/v1/audio/transcriptions".into(),
        body,
        raw_bytes: None,
        headers: header_list(&headers),
        api_key: support::extract_key(&headers, support::query_api_key(&uri).as_deref()),
    };
    match handle_stt(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct VoicesQuery {
    provider: Option<String>,
    lang: Option<String>,
}

async fn get_audio_voices(
    Query(query): Query<VoicesQuery>,
) -> Response {
    const VALID_PROVIDERS: &[&str] = &["elevenlabs", "deepgram", "inworld", "edge-tts", "local-device"];
    let Some(provider) = query.provider else {
        return support::error_response(
            400,
            &format!("provider must be one of: {}", VALID_PROVIDERS.join(", ")),
        );
    };
    if !VALID_PROVIDERS.contains(&provider.as_str()) {
        return support::error_response(
            400,
            &format!("provider must be one of: {}", VALID_PROVIDERS.join(", ")),
        );
    }

    // Return voices from nr-data tts_models config
    let voices = nr_data::tts_models::get_tts_voices_for_model(&provider, "default")
        .unwrap_or_default();

    let data_out: Vec<Value> = voices
        .into_iter()
        .map(|v| {
            let id = v.get("id").or_else(|| v.get("voice_id")).and_then(Value::as_str).unwrap_or("voice");
            let name = v.get("name").and_then(Value::as_str).unwrap_or(id);
            json!({
                "id": id,
                "name": name,
                "lang": query.lang.as_deref().unwrap_or(""),
                "gender": v.get("gender").and_then(Value::as_str).unwrap_or(""),
                "model": format!("{provider}/{id}"),
            })
        })
        .collect();

    Json(json!({
        "object": "list",
        "data": data_out,
    })).into_response()
}

// ─── Search & Fetch ─────────────────────────────────────────────────────────

async fn post_search(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    match handle_search(body, header_list(&headers)).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

async fn post_fetch(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    match handle_fetch(body, header_list(&headers)).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

// ─── SystemOne ──────────────────────────────────────────────────────────────

async fn post_systemone(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    if body.get("model").and_then(Value::as_str).is_none() {
        return support::error_response(400, "Missing model");
    }
    if body.get("state").is_none() {
        return support::error_response(400, "Missing required field: state");
    }
    if !body.get("questions").is_some_and(Value::is_object) {
        return support::error_response(400, "Missing required field: questions");
    }
    match handle_systemone(body, header_list(&headers)).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

// ─── Videos ─────────────────────────────────────────────────────────────────

async fn post_video_generations(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    dispatch_video(db, headers, uri, body, "generations", None).await
}

async fn post_video_edits(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    dispatch_video(db, headers, uri, body, "edits", None).await
}

async fn post_video_extensions(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    dispatch_video(db, headers, uri, body, "extensions", None).await
}

async fn get_video_by_id(
    axum::Extension(db): axum::Extension<Db>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    dispatch_video(db, headers, uri, None, "get", Some(id)).await
}

async fn dispatch_video(
    db: Db,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
    action: &str,
    id: Option<String>,
) -> Response {
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    let req = VideoRequest {
        action: action.to_string(),
        id,
        body: body.map(|Json(b)| b),
        headers: header_list(&headers),
        api_key: support::extract_key(&headers, support::query_api_key(&uri).as_deref()),
    };
    match handle_video(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}
