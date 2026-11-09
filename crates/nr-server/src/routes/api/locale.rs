use axum::http::header::SET_COOKIE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::common::bad_request;

pub const LOCALES: &[&str] = &[
    "en", "vi", "zh-CN", "zh-TW", "ja", "pt-BR", "pt-PT", "ko", "es", "de", "fr",
    "he", "ar", "ru", "pl", "cs", "nl", "tr", "uk", "tl", "id", "km", "th", "hi",
    "bn", "ur", "ro", "sv", "it", "el", "hu", "fi", "da", "no", "fa",
];

pub fn is_supported_locale(locale: &str) -> bool {
    LOCALES.contains(&locale) || locale == "zh"
}

pub fn normalize_locale(locale: &str) -> &'static str {
    match locale {
        "zh" | "zh-CN" => "zh-CN",
        "en" => "en",
        "vi" => "vi",
        "zh-TW" => "zh-TW",
        "ja" => "ja",
        "pt-BR" => "pt-BR",
        "pt-PT" => "pt-PT",
        "ko" => "ko",
        "es" => "es",
        "de" => "de",
        "fr" => "fr",
        "he" => "he",
        "ar" => "ar",
        "ru" => "ru",
        "pl" => "pl",
        "cs" => "cs",
        "nl" => "nl",
        "tr" => "tr",
        "uk" => "uk",
        "tl" => "tl",
        "id" => "id",
        "th" => "th",
        "km" => "km",
        "hi" => "hi",
        "bn" => "bn",
        "ur" => "ur",
        "ro" => "ro",
        "sv" => "sv",
        "it" => "it",
        "el" => "el",
        "hu" => "hu",
        "fi" => "fi",
        "da" => "da",
        "no" => "no",
        "fa" => "fa",
        _ => "en",
    }
}

#[derive(Deserialize)]
pub struct SetLocaleRequest {
    pub locale: Option<String>,
}

pub async fn post_locale(Json(body): Json<SetLocaleRequest>) -> Response {
    let Some(raw_locale) = body.locale else {
        return bad_request("Invalid locale");
    };

    if !is_supported_locale(&raw_locale) {
        return bad_request("Invalid locale");
    }

    let normalized = normalize_locale(&raw_locale);
    let cookie_val = format!("locale={normalized}; Path=/; Max-Age=31536000");

    let mut resp = (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "locale": normalized,
        })),
    )
        .into_response();

    if let Ok(hv) = HeaderValue::from_str(&cookie_val) {
        resp.headers_mut().insert(SET_COOKIE, hv);
    }

    resp
}
