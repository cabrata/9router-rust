use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{get_pricing, reset_all_pricing, reset_pricing, update_pricing, Db};
use serde::Deserialize;
use serde_json::Value;

use super::common::{bad_request, get_db, internal_error};

const VALID_PRICING_FIELDS: &[&str] = &[
    "input",
    "output",
    "cached",
    "reasoning",
    "cache_creation",
];

// GET /api/pricing - Get current pricing
pub async fn get_pricing_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_pricing(&db) {
        Ok(pricing) => (StatusCode::OK, Json(pricing)).into_response(),
        Err(e) => internal_error(format!("Failed to fetch pricing: {e}")),
    }
}

// GET /api/pricing/defaults - Get default pricing
pub async fn get_pricing_defaults() -> Response {
    let defaults = nr_data::pricing::get_default_pricing();
    (StatusCode::OK, Json(defaults)).into_response()
}

// PATCH /api/pricing - Update pricing
pub async fn patch_pricing(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let obj = match body.as_object() {
        Some(o) => o,
        None => return bad_request("Invalid pricing data format"),
    };

    for (provider, models_val) in obj {
        let models_map = match models_val.as_object() {
            Some(m) => m,
            None => return bad_request(format!("Invalid pricing for provider: {provider}")),
        };

        for (model, pricing_val) in models_map {
            let pricing_map = match pricing_val.as_object() {
                Some(p) => p,
                None => return bad_request(format!("Invalid pricing for model: {provider}/{model}")),
            };

            for (key, val) in pricing_map {
                if !VALID_PRICING_FIELDS.contains(&key.as_str()) {
                    return bad_request(format!(
                        "Invalid pricing field: {key} for {provider}/{model}"
                    ));
                }

                match val.as_f64() {
                    Some(n) if n >= 0.0 && !n.is_nan() => {}
                    _ => {
                        return bad_request(format!(
                            "Invalid pricing value for {key} in {provider}/{model}: must be non-negative number"
                        ))
                    }
                }
            }
        }
    }

    match update_pricing(&db, &body) {
        Ok(updated) => (StatusCode::OK, Json(updated)).into_response(),
        Err(e) => internal_error(format!("Failed to update pricing: {e}")),
    }
}

#[derive(Deserialize)]
pub struct PricingDeleteQuery {
    pub provider: Option<String>,
    pub model: Option<String>,
}

// DELETE /api/pricing - Reset pricing
pub async fn delete_pricing(
    ext: Option<Extension<Db>>,
    Query(query): Query<PricingDeleteQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let res = if let (Some(provider), Some(model)) = (&query.provider, &query.model) {
        reset_pricing(&db, Some(provider), Some(model))
    } else if let Some(provider) = &query.provider {
        reset_pricing(&db, Some(provider), None)
    } else {
        reset_all_pricing(&db)
    };

    if let Err(e) = res {
        return internal_error(format!("Failed to reset pricing: {e}"));
    }

    match get_pricing(&db) {
        Ok(pricing) => (StatusCode::OK, Json(pricing)).into_response(),
        Err(e) => internal_error(format!("Failed to fetch pricing: {e}")),
    }
}
