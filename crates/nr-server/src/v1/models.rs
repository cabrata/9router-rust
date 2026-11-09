//! Models catalog endpoint (port of `src/app/api/v1/models/**`).
//!
//! Exposes:
//! - `GET /v1/models` (the standard OpenAI-compatible catalog)
//! - `GET /v1/models/info?id={alias}/{modelId}` (metadata for a single model)
//! - `GET /v1/models/{kind}` (filtered by service kind: image, tts, stt, embedding, image-to-text, web)
//! - `GET /v1/models/{provider}/{model}` (single model lookup)
//! - `GET /v1` (re-export of /v1/models per Next.js route parity)

use axum::extract::{Path, Query};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use nr_core::json;
use nr_db::repos::{get_api_key_by_key, get_combos, get_disabled_models, get_provider_connections, get_settings};
use nr_db::Db;
use serde_json::Value;

use crate::support;

pub fn routes() -> Router {
    Router::new()
        .route("/v1/models/info", get(get_models_info).options(options))
        .route("/v1/models", get(get_models).options(options))
        .route("/v1", get(get_models).options(options))
        .route("/v1/models/{kind}", get(get_models_by_kind).options(options))
        .route("/v1/models/{provider}/{model}", get(get_single_model).options(options))
}

async fn options() -> Response {
    support::cors_headers("GET, OPTIONS")
}

const KIND_SLUGS: &[(&str, &[&str])] = &[
    ("image", &["image"]),
    ("tts", &["tts"]),
    ("stt", &["stt"]),
    ("embedding", &["embedding"]),
    ("image-to-text", &["imageToText"]),
    ("web", &["webSearch", "webFetch"]),
];

fn parse_slug_kinds(slug: &str) -> Option<&'static [&'static str]> {
    KIND_SLUGS.iter().find(|(k, _)| *k == slug).map(|(_, kinds)| *kinds)
}

/// GET /v1/models and GET /v1
async fn get_models(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    match build_catalog(&db, &["llm"], &headers, &uri).await {
        Ok(models) => Json(json!({
            "object": "list",
            "data": models,
        })).into_response(),
        Err(r) => r,
    }
}

/// GET /v1/models/{kind}
async fn get_models_by_kind(
    axum::Extension(db): axum::Extension<Db>,
    Path(kind_slug): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let Some(kinds) = parse_slug_kinds(&kind_slug) else {
        // Not a kind slug: treat as single model without provider prefix, e.g. "gpt-4"
        return get_single_model_by_id(&db, &kind_slug, &headers, &uri).await;
    };

    match build_catalog(&db, kinds, &headers, &uri).await {
        Ok(models) => Json(json!({
            "object": "list",
            "data": models,
        })).into_response(),
        Err(r) => r,
    }
}

/// GET /v1/models/{provider}/{model}
async fn get_single_model(
    axum::Extension(db): axum::Extension<Db>,
    Path((provider, model)): Path<(String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let full_id = format!("{provider}/{model}");
    get_single_model_by_id(&db, &full_id, &headers, &uri).await
}

async fn get_single_model_by_id(
    db: &Db,
    model_id: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Response {
    let models = match build_catalog(db, &["llm"], headers, uri).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    if let Some(found) = models.iter().find(|m| m.get("id").and_then(Value::as_str) == Some(model_id)) {
        return Json(found.clone()).into_response();
    }
    support::error_response(
        404,
        &format!("The model '{model_id}' does not exist or you do not have access to it."),
    )
}

/// GET /v1/models/info?id={alias}/{modelId}
#[derive(serde::Deserialize)]
struct InfoQuery {
    id: Option<String>,
    kind: Option<String>,
}

async fn get_models_info(
    Query(query): Query<InfoQuery>,
) -> Response {
    let Some(full_id) = query.id else {
        return support::error_response(400, "Missing required query param: id (e.g. ?id=openai/dall-e-3)");
    };
    let Some((alias, model_id)) = full_id.split_once('/') else {
        return support::error_response(404, &format!("Model not found: {full_id}"));
    };

    let provider_models = nr_data::provider_models();
    let models_for_alias = provider_models.get(alias).and_then(Value::as_array);
    let matched = models_for_alias.and_then(|list| {
        list.iter().find(|m| m.get("id").and_then(Value::as_str) == Some(model_id))
    });

    if let Some(m) = matched {
        let kind = nr_data::model_kind(m);
        let mut info = json!({
            "id": full_id,
            "name": m.get("name").unwrap_or(&json!(model_id)),
            "kind": kind,
            "owned_by": alias,
        });
        if let Some(caps) = m.get("capabilities") {
            info["capabilities"] = caps.clone();
        }
        if let Some(cw) = m.get("contextWindow") {
            info["contextWindow"] = cw.clone();
        }
        return Json(info).into_response();
    }

    // Virtual search/fetch
    if model_id == "search" {
        return Json(json!({
            "id": full_id,
            "name": format!("{alias} Search"),
            "kind": "webSearch",
            "owned_by": alias,
        })).into_response();
    }
    if model_id == "fetch" {
        return Json(json!({
            "id": full_id,
            "name": format!("{alias} Fetch"),
            "kind": "webFetch",
            "owned_by": alias,
        })).into_response();
    }

    support::error_response(404, &format!("Model not found: {full_id}"))
}

/// Build the model list according to DB settings, provider connections and
/// combos, mirroring `buildModelsList` in `models/route.js`.
async fn build_catalog(
    db: &Db,
    kind_filter: &[&str],
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<Vec<Value>, Response> {
    // Check if key is required
    let settings = get_settings(db).unwrap_or_else(|_| json!({}));
    let require_api_key = settings.get("requireApiKey").and_then(Value::as_bool).unwrap_or(true);
    let key = support::extract_key(headers, support::query_api_key(uri).as_deref());
    let allowed_models = if let Some(k) = &key {
        match get_api_key_by_key(db, k) {
            Ok(Some(rec)) => {
                if !rec.is_active {
                    return Err(support::error_response(401, "API key is inactive"));
                }
                Some(rec.allowed_models)
            }
            Ok(None) if require_api_key => return Err(support::error_response(401, "Invalid API key")),
            _ => None,
        }
    } else if require_api_key {
        return Err(support::error_response(401, "Missing API key"));
    } else {
        None
    };

    let mut out: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. Combos first
    if let Ok(combos) = get_combos(db) {
        for c in combos {
            // JS: `combo?.kind || LLM_KIND`, so a missing *or empty* kind is llm.
            let combo_kind = c.kind.as_deref().filter(|k| !k.is_empty()).unwrap_or("llm");
            if !kind_filter.contains(&combo_kind) {
                continue;
            }
            if seen.insert(c.name.clone()) {
                out.push(json!({
                    "id": c.name,
                    "object": "model",
                    "owned_by": "combo",
                }));
            }
        }
    }

    // 2. Active connections or fallback to all static models
    let connections = get_provider_connections(db, None).unwrap_or_default();
    let active_connections: Vec<_> = connections
        .into_iter()
        .filter(|c| c.get("isActive").and_then(Value::as_bool).unwrap_or(true))
        .collect();

    let disabled = get_disabled_models(db).unwrap_or_else(|_| json!({}));
    let is_disabled = |alias: &str, id: &str| -> bool {
        disabled.get(alias)
            .and_then(Value::as_array)
            .map(|arr| arr.iter().any(|v| v.as_str() == Some(id)))
            .unwrap_or(false)
    };

    if active_connections.is_empty() {
        // Return static models from nr-data
        for (alias, models) in nr_data::provider_models().iter() {
            let Some(list) = models.as_array() else { continue };
            for m in list {
                let kind = nr_data::model_kind(m);
                if !kind_filter.contains(&kind.as_str()) {
                    continue;
                }
                let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
                if is_disabled(alias, id) {
                    continue;
                }
                let full_id = format!("{alias}/{id}");
                if seen.insert(full_id.clone()) {
                    out.push(json!({
                        "id": full_id,
                        "object": "model",
                        "owned_by": alias,
                        "context_length": m.get("contextWindow").unwrap_or(&json!(200000)),
                        "max_completion_tokens": m.get("maxOutput").unwrap_or(&json!(64000)),
                    }));
                }
            }
        }
    } else {
        // Return models for configured active providers
        for conn in &active_connections {
            let Some(provider) = conn.get("provider").and_then(Value::as_str) else { continue };
            let prefix = conn.get("data")
                .and_then(|d| d.get("prefix"))
                .and_then(Value::as_str)
                .unwrap_or(provider);

            let models = nr_data::get_provider_models(provider);
            for m in models {
                let kind = nr_data::model_kind(&m);
                if !kind_filter.contains(&kind.as_str()) {
                    continue;
                }
                let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
                if is_disabled(prefix, id) || is_disabled(provider, id) {
                    continue;
                }
                let full_id = format!("{prefix}/{id}");
                if seen.insert(full_id.clone()) {
                    out.push(json!({
                        "id": full_id,
                        "object": "model",
                        "owned_by": prefix,
                        "context_length": m.get("contextWindow").unwrap_or(&json!(200000)),
                        "max_completion_tokens": m.get("maxOutput").unwrap_or(&json!(64000)),
                    }));
                }
            }
        }
    }

    // Filter by allowed_models if restricted
    if let Some(allowed) = allowed_models {
        if !allowed.is_empty() {
            out.retain(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .map(|id| allowed.iter().any(|a| a == "*" || a == id || (a.ends_with("/*") && id.starts_with(&a[..a.len() - 1]))))
                    .unwrap_or(false)
            });
        }
    }

    Ok(out)
}
