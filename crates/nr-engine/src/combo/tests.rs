//! Combo rotation tests: fallback ordering, request-scoped handling, round-robin.

use super::*;

fn ok_response() -> UpstreamResponse {
    UpstreamResponse::json(200, &serde_json::json!({"ok": true}))
}

fn err_response(status: u16, msg: &str) -> UpstreamResponse {
    UpstreamResponse::json(status, &serde_json::json!({"error": {"message": msg}}))
}

fn make_try(
    plan: Vec<Result<UpstreamResponse>>,
) -> (TryMemberFn<'static>, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = calls.clone();
    // `UpstreamResponse` holds a `Box<dyn Stream + Send>` (not `Sync`), so the
    // script lives behind a Mutex to keep this closure `Sync`.
    let it = std::sync::Arc::new(std::sync::Mutex::new(plan.into_iter()));
    let f: TryMemberFn<'static> = Box::new(move |model: &str| {
        let seen = seen.clone();
        let it = it.clone();
        let model = model.to_string();
        Box::pin(async move {
            seen.lock().unwrap().push(model);
            it.lock().unwrap().next().unwrap_or_else(|| Ok(ok_response()))
        })
    });
    (f, calls)
}

#[tokio::test]
async fn first_member_success_returns_immediately() {
    let (try_fn, calls) = make_try(vec![Ok(ok_response())]);
    let models = vec!["a/m1".to_string(), "b/m2".to_string()];
    match handle_combo_chat(&models, "combo-a", "fallback", try_fn).await {
        ComboOutcome::Response(r) => assert!(r.ok()),
        _ => panic!("expected Response"),
    }
    assert_eq!(*calls.lock().unwrap(), vec!["a/m1".to_string()]);
}

#[tokio::test]
async fn rate_limited_first_falls_back_to_second() {
    let (try_fn, calls) = make_try(vec![Ok(err_response(429, "rate limit")), Ok(ok_response())]);
    let models = vec!["a/m1".to_string(), "b/m2".to_string()];
    match handle_combo_chat(&models, "combo-b", "fallback", try_fn).await {
        ComboOutcome::Response(r) => assert!(r.ok()),
        _ => panic!("expected Response from second member"),
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn request_scoped_400_advances_to_next_member() {
    let (try_fn, calls) = make_try(vec![
        Ok(err_response(400, "context length exceeded")),
        Ok(ok_response()),
    ]);
    let models = vec!["a/m1".to_string(), "b/m2".to_string()];
    match handle_combo_chat(&models, "combo-c", "fallback", try_fn).await {
        ComboOutcome::Response(r) => assert!(r.ok()),
        _ => panic!("expected success from second member"),
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn last_member_error_is_returned_as_is() {
    let (try_fn, _calls) = make_try(vec![
        Ok(err_response(429, "rate limit")),
        Ok(err_response(500, "boom")),
    ]);
    let models = vec!["a/m1".to_string(), "b/m2".to_string()];
    match handle_combo_chat(&models, "combo-d", "fallback", try_fn).await {
        ComboOutcome::Exhausted { status, error, response } => {
            assert_eq!(status, 500);
            assert!(error.contains("boom"), "{error}");
            assert!(response.is_some());
        }
        _ => panic!("expected Exhausted"),
    }
}

#[tokio::test]
async fn empty_combo_is_400() {
    let (try_fn, _calls) = make_try(vec![]);
    match handle_combo_chat(&[], "combo-e", "fallback", try_fn).await {
        ComboOutcome::Exhausted { status, .. } => assert_eq!(status, 400),
        _ => panic!("expected Exhausted 400"),
    }
}

#[tokio::test]
async fn transport_error_advances() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = calls.clone();
    // Atomic, not a captured `bool`: `async move` copies a `bool` into each
    // future, so the flag would reset on every call and never advance.
    let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let try_fn: TryMemberFn<'static> = Box::new(move |m: &str| {
        let seen = seen.clone();
        let first = first.clone();
        let m = m.to_string();
        Box::pin(async move {
            seen.lock().unwrap().push(m);
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Err(nr_core::Error::Network("conn refused".into()))
            } else {
                Ok(ok_response())
            }
        })
    });
    let models = vec!["a/m1".to_string(), "b/m2".to_string()];
    match handle_combo_chat(&models, "combo-f", "fallback", try_fn).await {
        ComboOutcome::Response(r) => assert!(r.ok()),
        _ => panic!("expected success from second member"),
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[test]
fn round_robin_rotates_per_combo() {
    let models = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    // Distinct combos keep independent counters.
    let r1 = rotate_for_round_robin(&models, "rr-1", "round-robin");
    assert_eq!(r1, vec!["a", "b", "c"]);
    let r2 = rotate_for_round_robin(&models, "rr-1", "round-robin");
    assert_eq!(r2, vec!["b", "c", "a"]);
    let other = rotate_for_round_robin(&models, "rr-2", "round-robin");
    assert_eq!(other, vec!["a", "b", "c"], "second combo starts fresh");
    // fallback strategy keeps strict order
    let r3 = rotate_for_round_robin(&models, "rr-1", "fallback");
    assert_eq!(r3, vec!["a", "b", "c"]);
}

#[test]
fn single_model_combo_never_rotates() {
    let models = vec!["only/m".to_string()];
    let r = rotate_for_round_robin(&models, "rr-3", "round-robin");
    assert_eq!(r, vec!["only/m"]);
}

#[test]
fn combo_models_lookup_accepts_array_and_wrapper() {
    let combos = serde_json::json!([
        { "name": "fast", "models": ["a/m1", "b/m2"] },
        { "name": "empty", "models": [] }
    ]);
    assert_eq!(
        get_combo_models_from_data("fast", &combos),
        Some(vec!["a/m1".to_string(), "b/m2".to_string()])
    );
    // Object wrapper (`{ combos: [...] }`) is the API shape.
    let wrapped = serde_json::json!({ "combos": combos });
    assert_eq!(
        get_combo_models_from_data("fast", &wrapped),
        Some(vec!["a/m1".to_string(), "b/m2".to_string()])
    );
    assert_eq!(get_combo_models_from_data("empty", &combos), None);
    assert_eq!(get_combo_models_from_data("missing", &combos), None);
    assert_eq!(get_combo_models_from_data("fast", &serde_json::json!({})), None);
}

#[test]
fn combo_models_lookup_ignores_provider_model_strings() {
    let combos = serde_json::json!([{ "name": "plexus/fast", "models": ["a/m1"] }]);
    assert_eq!(get_combo_models_from_data("plexus/fast", &combos), None);
}
