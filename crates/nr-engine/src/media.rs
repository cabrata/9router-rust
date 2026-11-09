//! Media request rotation: the credential + fallback loop shared by
//! embeddings, images, TTS and STT. Port of the `while (true)` account loop in
//! src/sse/handlers/{embeddings,imageGeneration,tts,stt}.js.
//!
//! Provider-specific URL/body/parse lives in the provider config
//! (`nr_data::provider_media`); this module only rotates accounts.

use std::collections::HashSet;

use nr_core::http::UpstreamResponse;
use nr_core::{Error, Json, Result};
use nr_db::Db;

use crate::credentials::{
    clear_account_error, get_provider_credentials, mark_account_unavailable, CredentialLookup, Strategy,
};

/// One media attempt against a selected account.
pub type MediaAttemptFn<'a> =
    Box<dyn Fn(nr_core::Credentials) -> futures::future::BoxFuture<'a, Result<UpstreamResponse>> + Send + Sync + 'a>;

/// Rotate accounts for a media capability.
///
/// `lock_key` scopes the cooldown (`model` for embeddings, `webfetch:<provider>`
/// for web fetch) so a media failure never takes an LLM account offline.
pub async fn rotate_media_accounts(
    db: &Db,
    provider: &str,
    lock_key: Option<&str>,
    attempt: MediaAttemptFn<'_>,
) -> Result<UpstreamResponse> {
    let mut exclude: HashSet<String> = HashSet::new();
    let mut last_status = 503u16;
    let mut last_error = String::from("Unavailable");

    loop {
        match get_provider_credentials(db, provider, &exclude, lock_key, Strategy::FillFirst) {
            CredentialLookup::None => {
                if exclude.is_empty() {
                    return Err(Error::BadRequest(format!("No credentials for provider: {provider}")));
                }
                return Err(status_error(last_status, &last_error));
            }
            CredentialLookup::AllRateLimited { retry_after_human, last_error: le, .. } => {
                let msg = if last_error != "Unavailable" { last_error } else { le.unwrap_or_else(|| "Unavailable".into()) };
                let _ = retry_after_human;
                return Err(Error::Upstream { status: 503, message: format!("[{provider}] {msg}") });
            }
            CredentialLookup::Selected(cred) => {
                let connection_id = cred.connection_id.clone();
                match attempt(cred.credentials.clone()).await {
                    Ok(resp) if resp.ok() => {
                        clear_account_error(db, &connection_id, lock_key);
                        return Ok(resp);
                    }
                    Ok(resp) => {
                        let status = resp.status;
                        let text = resp.text().await.unwrap_or_default();
                        let msg = extract_message(&text).unwrap_or_else(|| format!("HTTP {status}"));
                        let decision =
                            crate::credentials::mark_account_unavailable(
                                db,
                                &connection_id,
                                status,
                                &msg,
                                Some(provider),
                                lock_key,
                                0,
                            );
                        if decision.should_fallback {
                            exclude.insert(connection_id);
                            last_status = status;
                            last_error = msg;
                            continue;
                        }
                        return Err(status_error(status, &msg));
                    }
                    Err(e) => {
                        mark_account_unavailable(db, &connection_id, e.status(), &e.to_string(), Some(provider), lock_key, 0);
                        exclude.insert(connection_id);
                        last_status = e.status();
                        last_error = e.to_string();
                        if exclude.len() > 16 {
                            return Err(e);
                        }
                        continue;
                    }
                }
            }
        }
    }
}

fn status_error(status: u16, message: &str) -> Error {
    match status {
        400 => Error::BadRequest(message.to_string()),
        401 | 403 => Error::Provider(message.to_string()),
        404 => Error::NotFound(message.to_string()),
        429 => Error::Upstream { status: 429, message: message.to_string() },
        _ => Error::Upstream { status, message: message.to_string() },
    }
}

fn extract_message(body: &str) -> Option<String> {
    let v: Json = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("message").and_then(Json::as_str).map(str::to_string))
        .or_else(|| v.get("error").and_then(Json::as_str).map(str::to_string))
        .or_else(|| v.get("message").and_then(Json::as_str).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nr_core::json;
    use nr_db::repos::create_provider_connection;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        (dir, db)
    }

    fn add_conn(db: &Db, id: &str) {
        create_provider_connection(db, &json!({
            "id": id, "provider": "openai", "authType": "apikey",
            "name": id, "priority": 1, "isActive": true,
            "providerSpecificData": { "apiKey": id }
        })).unwrap();
    }

    #[tokio::test]
    async fn success_on_first_account() {
        let (_d, db) = setup();
        add_conn(&db, "c1");
        let f: MediaAttemptFn<'static> = Box::new(|_c| {
            Box::pin(async { Ok(UpstreamResponse::json(200, &json!({"data": []}))) })
        });
        let resp = rotate_media_accounts(&db, "openai", Some("emb"), f).await.unwrap();
        assert!(resp.ok());
    }

    #[tokio::test]
    async fn rotates_past_rate_limited_account() {
        let (_d, db) = setup();
        add_conn(&db, "c1");
        add_conn(&db, "c2");
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        let f: MediaAttemptFn<'static> = Box::new(move |_c| {
            let n = n2.clone();
            Box::pin(async move {
                let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i == 0 {
                    Ok(UpstreamResponse::json(429, &json!({"error": {"message": "rate limit"}})))
                } else {
                    Ok(UpstreamResponse::json(200, &json!({"ok": true})))
                }
            })
        });
        let resp = rotate_media_accounts(&db, "openai", Some("emb"), f).await.unwrap();
        assert!(resp.ok());
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn no_credentials_is_400() {
        let (_d, db) = setup();
        let f: MediaAttemptFn<'static> = Box::new(|_c| Box::pin(async { Ok(UpstreamResponse::json(200, &json!({}))) }));
        let err = rotate_media_accounts(&db, "openai", None, f).await.unwrap_err();
        assert_eq!(err.status(), 400);
    }
}
