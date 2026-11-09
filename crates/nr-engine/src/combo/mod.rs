//! Combo (multi-model) rotation with fallback. Port of open-sse/services/combo.js
//! `handleComboChat` (the "fallback" / "round-robin" strategies; smart/fusion
//! routing is a follow-up, see report).
//!
//! A combo is an ordered list of member models. We try each in order; on a
//! member failure we advance to the next. A request-scoped 4xx (context
//! overflow, unsupported parameter) advances without evicting a healthy
//! account, since the next member may run on a different provider.

use bytes::Bytes;
use futures::StreamExt;
use nr_core::http::UpstreamResponse;
use nr_core::{Json, Result};

use crate::account_fallback::check_fallback_error;

/// Result of rotating through a combo.
pub enum ComboOutcome {
    /// A member served the request.
    Response(UpstreamResponse),
    /// Every member was exhausted; surface the last error.
    Exhausted {
        status: u16,
        error: String,
        response: Option<UpstreamResponse>,
    },
}

/// How one combo member is attempted.
pub type TryMemberFn<'a> =
    Box<dyn Fn(&str) -> futures::future::BoxFuture<'a, Result<UpstreamResponse>> + Send + Sync + 'a>;

/// Rotate through combo `models` in order, falling back on member failure.
///
/// `strategy` is `"fallback"` (strict order) or `"round-robin"` (rotate the
/// starting index per combo). The last member's response is always returned
/// even on error so the client sees the real upstream error.
pub async fn handle_combo_chat(
    models: &[String],
    combo_name: &str,
    strategy: &str,
    try_member: TryMemberFn<'_>,
) -> ComboOutcome {
    if models.is_empty() {
        return ComboOutcome::Exhausted {
            status: 400,
            error: format!("combo {combo_name} has no models"),
            response: None,
        };
    }

    let ordered = rotate_for_round_robin(models, combo_name, strategy);
    let is_last = |i: usize| i == ordered.len() - 1;

    let mut last_error = String::new();
    let mut last_status: u16 = 502;
    let mut request_scoped_response: Option<UpstreamResponse> = None;

    for (i, model_str) in ordered.iter().enumerate() {
        let response = match try_member(model_str).await {
            Ok(r) => r,
            Err(e) => {
                // Transport failure: always advance to the next member.
                last_error = e.to_string();
                last_status = e.status();
                if is_last(i) {
                    // JS `handleComboChat` always receives a Response from
                    // `handleSingleModelChat` (never a throw), so the last
                    // member's failure still reaches the client as its own
                    // status + error envelope. Rebuild that here.
                    return ComboOutcome::Exhausted {
                        status: last_status,
                        response: Some(error_response(last_status, &last_error)),
                        error: last_error,
                    };
                }
                continue;
            }
        };

        if response.ok() {
            return ComboOutcome::Response(response);
        }

        // Non-ok: buffer the error body so the rule engine can classify it,
        // then rebuild the response so it can still be returned as-is.
        let status = response.status;
        let (error_text, rebuilt) = read_error(response).await;

        if is_last(i) {
            return ComboOutcome::Exhausted { status, error: error_text, response: Some(rebuilt) };
        }

        let provider = crate::model::parse_model(model_str).provider;
        let decision = check_fallback_error(status, &error_text, 0, provider.as_deref());

        if !decision.should_fallback && !decision.request_scoped {
            // Hard failure that will not improve by rotating — surface it.
            return ComboOutcome::Exhausted { status, error: error_text, response: Some(rebuilt) };
        }

        if decision.request_scoped {
            // This member rejected the request; keep the first such response to
            // surface if every other member also rejects it.
            if request_scoped_response.is_none() {
                request_scoped_response = Some(rebuilt);
            }
            last_error = error_text;
            last_status = status;
            continue;
        }

        last_error = error_text;
        last_status = status;
    }

    ComboOutcome::Exhausted {
        status: last_status,
        error: if last_error.is_empty() { "all combo models unavailable".into() } else { last_error },
        response: request_scoped_response,
    }
}

/// Read an error body into `(message, rebuilt response)`.
async fn read_error(response: UpstreamResponse) -> (String, UpstreamResponse) {
    let status = response.status;
    let headers = response.headers.clone();
    let chunks: Vec<_> = response.body.collect().await;
    let bytes: Bytes = chunks.into_iter().filter_map(|c| c.ok()).flatten().collect();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let message = extract_message(&text).unwrap_or_else(|| format!("HTTP {status}"));
    (message, UpstreamResponse::from_bytes(status, headers, bytes))
}

fn extract_message(body: &str) -> Option<String> {
    let v: Json = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("message").and_then(Json::as_str).map(str::to_string))
        .or_else(|| v.get("error").and_then(Json::as_str).map(str::to_string))
        .or_else(|| v.get("message").and_then(Json::as_str).map(str::to_string))
}

/// OpenAI error envelope with the given status (JS `errorResponse`).
fn error_response(status: u16, message: &str) -> UpstreamResponse {
    UpstreamResponse::json(status, &serde_json::json!({ "error": crate::stream::json_error(status, message) }))
}

/// Round-robin: derive a per-combo counter and rotate the starting seat.
fn rotate_for_round_robin(models: &[String], combo_name: &str, strategy: &str) -> Vec<String> {
    if strategy != "round-robin" || models.len() <= 1 {
        return models.to_vec();
    }
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::LazyLock;
    // Per-combo counters so two combos do not rotate in lockstep.
    static COUNTERS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);
    let mut counters = COUNTERS.lock().expect("combo counter poisoned");
    let counter = counters.entry(combo_name.to_string()).or_insert(0);
    let start = (*counter % models.len() as u64) as usize;
    *counter = counter.wrapping_add(1);
    drop(counters);
    let (a, b) = models.split_at(start);
    b.iter().chain(a.iter()).cloned().collect()
}

#[cfg(test)]
mod tests;
