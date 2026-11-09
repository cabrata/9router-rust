use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{get_provider_connections, Db};
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{bad_request, get_db};

#[derive(Deserialize)]
pub struct TtsVoicesQuery {
    pub provider: Option<String>,
    pub lang: Option<String>,
    pub apiKey: Option<String>,
}

fn default_edge_voices() -> Value {
    json!({
        "languages": [
            { "code": "en", "name": "English", "voices": [
                { "id": "en-US-AriaNeural", "name": "Aria (Natural)", "locale": "en-US", "lang": "en", "gender": "Female" },
                { "id": "en-US-GuyNeural", "name": "Guy (Natural)", "locale": "en-US", "lang": "en", "gender": "Male" }
            ]},
            { "code": "zh", "name": "Chinese", "voices": [
                { "id": "zh-CN-XiaoxiaoNeural", "name": "Xiaoxiao (Natural)", "locale": "zh-CN", "lang": "zh", "gender": "Female" }
            ]}
        ],
        "byLang": {
            "en": { "code": "en", "name": "English", "voices": [
                { "id": "en-US-AriaNeural", "name": "Aria (Natural)", "locale": "en-US", "lang": "en", "gender": "Female" },
                { "id": "en-US-GuyNeural", "name": "Guy (Natural)", "locale": "en-US", "lang": "en", "gender": "Male" }
            ]},
            "zh": { "code": "zh", "name": "Chinese", "voices": [
                { "id": "zh-CN-XiaoxiaoNeural", "name": "Xiaoxiao (Natural)", "locale": "zh-CN", "lang": "zh", "gender": "Female" }
            ]}
        }
    })
}

// GET /api/media-providers/tts/voices
pub async fn get_tts_voices(Query(query): Query<TtsVoicesQuery>) -> Response {
    let provider = query.provider.as_deref().unwrap_or("edge-tts");
    let lang_filter = query.lang.as_deref();

    let data = match provider {
        "edge-tts" | "local-device" | "elevenlabs" => default_edge_voices(),
        _ => return bad_request(format!("Provider '{provider}' does not support voice listing")),
    };

    if let Some(lang) = lang_filter {
        let voices = data.get("byLang")
            .and_then(|bl| bl.get(lang))
            .and_then(|l| l.get("voices"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return (StatusCode::OK, Json(json!({ "voices": voices }))).into_response();
    }

    (StatusCode::OK, Json(data)).into_response()
}

// GET /api/media-providers/tts/elevenlabs/voices
pub async fn get_elevenlabs_voices(
    ext: Option<Extension<Db>>,
    Query(query): Query<TtsVoicesQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let filter = json!({ "provider": "elevenlabs", "isActive": true });
    let conns = get_provider_connections(&db, Some(&filter)).unwrap_or_default();
    let api_key = query.apiKey.as_deref().or_else(|| {
        conns.first().and_then(|c| c.get("apiKey")).and_then(Value::as_str)
    });

    if api_key.is_none() {
        return bad_request("No ElevenLabs connection found");
    }

    // Default ElevenLabs preset voices
    let voices_data = json!({
        "languages": [
            { "code": "en", "name": "English", "voices": [
                { "id": "21m00Tcm4TlvDq8ikWAM", "name": "Rachel", "gender": "female", "lang": "en", "free_users_allowed": true },
                { "id": "AZnzlk1XvdvUeBnXmlld", "name": "Domi", "gender": "female", "lang": "en", "free_users_allowed": true }
            ]}
        ],
        "byLang": {
            "en": {
                "code": "en", "name": "English",
                "voices": [
                    { "id": "21m00Tcm4TlvDq8ikWAM", "name": "Rachel", "gender": "female", "lang": "en", "free_users_allowed": true },
                    { "id": "AZnzlk1XvdvUeBnXmlld", "name": "Domi", "gender": "female", "lang": "en", "free_users_allowed": true }
                ]
            }
        }
    });

    if let Some(lang) = query.lang.as_deref() {
        let voices = voices_data.get("byLang")
            .and_then(|bl| bl.get(lang))
            .and_then(|l| l.get("voices"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return (StatusCode::OK, Json(json!({ "voices": voices }))).into_response();
    }

    (StatusCode::OK, Json(voices_data)).into_response()
}

// GET /api/media-providers/tts/deepgram/voices
pub async fn get_deepgram_voices(
    ext: Option<Extension<Db>>,
    Query(query): Query<TtsVoicesQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let filter = json!({ "provider": "deepgram", "isActive": true });
    let conns = get_provider_connections(&db, Some(&filter)).unwrap_or_default();
    if conns.first().and_then(|c| c.get("apiKey")).is_none() {
        return bad_request("No Deepgram connection found");
    }

    let voices_data = json!({
        "languages": [
            { "code": "en", "name": "English", "voices": [
                { "id": "aura-2-thalia-en", "name": "Thalia", "gender": "feminine", "lang": "en" },
                { "id": "aura-2-orion-en", "name": "Orion", "gender": "masculine", "lang": "en" }
            ]}
        ],
        "byLang": {
            "en": {
                "code": "en", "name": "English",
                "voices": [
                    { "id": "aura-2-thalia-en", "name": "Thalia", "gender": "feminine", "lang": "en" },
                    { "id": "aura-2-orion-en", "name": "Orion", "gender": "masculine", "lang": "en" }
                ]
            }
        }
    });

    if let Some(lang) = query.lang.as_deref() {
        let voices = voices_data.get("byLang")
            .and_then(|bl| bl.get(lang))
            .and_then(|l| l.get("voices"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return (StatusCode::OK, Json(json!({ "voices": voices }))).into_response();
    }

    (StatusCode::OK, Json(voices_data)).into_response()
}

// GET /api/media-providers/tts/inworld/voices
pub async fn get_inworld_voices(
    ext: Option<Extension<Db>>,
    Query(query): Query<TtsVoicesQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let filter = json!({ "provider": "inworld", "isActive": true });
    let conns = get_provider_connections(&db, Some(&filter)).unwrap_or_default();
    if conns.first().and_then(|c| c.get("apiKey")).is_none() {
        return bad_request("No Inworld connection found");
    }

    let voices_data = json!({
        "languages": [
            { "code": "en", "name": "English", "voices": [
                { "id": "inworld-en-1", "name": "Inworld Voice 1", "gender": "female", "lang": "en" }
            ]}
        ],
        "byLang": {
            "en": {
                "code": "en", "name": "English",
                "voices": [
                    { "id": "inworld-en-1", "name": "Inworld Voice 1", "gender": "female", "lang": "en" }
                ]
            }
        }
    });

    if let Some(lang) = query.lang.as_deref() {
        let voices = voices_data.get("byLang")
            .and_then(|bl| bl.get(lang))
            .and_then(|l| l.get("voices"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return (StatusCode::OK, Json(json!({ "voices": voices }))).into_response();
    }

    (StatusCode::OK, Json(voices_data)).into_response()
}

// GET /api/media-providers/tts/minimax/voices
pub async fn get_minimax_voices(
    ext: Option<Extension<Db>>,
    Query(query): Query<TtsVoicesQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let filter = json!({ "provider": "minimax", "isActive": true });
    let conns = get_provider_connections(&db, Some(&filter)).unwrap_or_default();
    if conns.first().and_then(|c| c.get("apiKey")).is_none() {
        return bad_request("No MiniMax connection found");
    }

    let voices_data = json!({
        "languages": [
            { "code": "Custom", "name": "Custom", "voices": [
                { "id": "male-qn-qingse", "name": "Qing Se", "lang": "Custom" }
            ]}
        ],
        "byLang": {
            "Custom": {
                "code": "Custom", "name": "Custom",
                "voices": [
                    { "id": "male-qn-qingse", "name": "Qing Se", "lang": "Custom" }
                ]
            }
        }
    });

    if let Some(lang) = query.lang.as_deref() {
        let voices = voices_data.get("byLang")
            .and_then(|bl| bl.get(lang))
            .and_then(|l| l.get("voices"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return (StatusCode::OK, Json(json!({ "voices": voices }))).into_response();
    }

    (StatusCode::OK, Json(voices_data)).into_response()
}
