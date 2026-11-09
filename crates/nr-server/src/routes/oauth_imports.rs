//! Port of the static `src/app/api/oauth/<provider>/<action>/route.js` import
//! surfaces.
//!
//! These are credential **import** endpoints (paste a token, or read an IDE's
//! local store), not new providers: every provider's OAuth flow already lives in
//! `nr_oauth::flows::providers`, and `/api/oauth/{provider}/{action}` keeps
//! serving the dynamic flows.
//!
//! The routes that need a live upstream validation call (`kiro/*`, `iflow/cookie`,
//! `zed/import`, `xiaomi-mimo/*`, `perchai/plan/*`, `cursor/import`) live in
//! `super::oauth_flow`, next to the dynamic provider flows they share helpers
//! with.

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use nr_db::{create_provider_connection, Db};
use serde_json::{json, Value};

use super::oauth_jwt::{decode_jwt_payload, extract_codex_account_info, extract_email_from_access_token};
use super::oauth_local_creds;

fn err_json(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn db_of(ext: Option<Extension<Db>>) -> Result<Db, Response> {
    match ext {
        Some(Extension(db)) => Ok(db),
        None => nr_db::db().map_err(|e| err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Epoch milliseconds offset from now, formatted like `new Date(...).toISOString()`.
fn iso_in(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339()
}

/// Epoch seconds → RFC 3339.
fn iso_at(epoch_seconds: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_seconds, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

fn trimmed<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `normalizeAccounts(body)`: array, `{ accounts: [...] }`, or a single object.
pub fn normalize_accounts(body: &Value) -> Option<Vec<Value>> {
    if let Some(array) = body.as_array() {
        return Some(array.clone());
    }
    if let Some(array) = body.get("accounts").and_then(|v| v.as_array()) {
        return Some(array.clone());
    }
    if body.is_object() {
        return Some(vec![body.clone()]);
    }
    None
}

/// Fields the server owns; stripped from bulk items before insert.
const SERVER_FIELDS: &[&str] = &["id", "provider", "authType", "createdAt", "updatedAt"];

fn strip_server_fields(item: &Value) -> Value {
    let mut out = item.clone();
    if let Some(obj) = out.as_object_mut() {
        for field in SERVER_FIELDS {
            obj.remove(*field);
        }
    }
    out
}

// ─── codex ──────────────────────────────────────────────────────────────────

/// POST /api/oauth/codex/import-token
pub async fn codex_import_token(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(token) = trimmed(&body, "accessToken") else {
        return err_json(StatusCode::BAD_REQUEST, "Access token is required");
    };

    let info = extract_codex_account_info(token);
    let mut psd = serde_json::Map::new();
    psd.insert("authMethod".into(), json!("access_token"));
    for (from, to) in [
        ("chatgptAccountId", "chatgptAccountId"),
        ("chatgptPlanType", "chatgptPlanType"),
        ("jwtExp", "jwtExp"),
    ] {
        if let Some(value) = info.get(from).filter(|v| !v.is_null()) {
            psd.insert(to.into(), value.clone());
        }
    }

    // The route decodes the token a second time in JS to pick up the profile
    // email and `preferred_username`, which `extractCodexAccountInfo` ignores.
    let payload_email = decode_jwt_payload(token).and_then(|payload| {
        payload
            .get("https://api.openai.com/profile")
            .and_then(|profile| profile.get("email"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| payload.get("email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
            .or_else(|| {
                payload
                    .get("preferred_username")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_string)
    });

    let email = info
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(payload_email);
    let name = trimmed(&body, "name")
        .map(str::to_string)
        .or_else(|| email.clone())
        .unwrap_or_else(|| "ChatGPT Access Token".to_string());

    let db = match db_of(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let payload = json!({
        "provider": "codex",
        "authType": "access_token",
        "accessToken": token,
        "name": name,
        "email": email,
        "providerSpecificData": Value::Object(psd.clone()),
        "testStatus": "active",
    });

    match create_provider_connection(&db, &payload) {
        Ok(conn) => Json(json!({
            "success": true,
            "connection": {
                "id": conn.get("id").cloned().unwrap_or(Value::Null),
                "provider": "codex",
                "email": conn.get("email").cloned().unwrap_or(Value::Null),
                "name": conn.get("name").cloned().unwrap_or(Value::Null),
                "workspace": psd.get("chatgptAccountId").cloned().unwrap_or(Value::Null),
                "plan": psd.get("chatgptPlanType").cloned().unwrap_or(Value::Null),
            },
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/oauth/codex/bulk-import
///
/// Items are inserted **serially**: `create_provider_connection` reads
/// `max(priority)` and reorders inside a transaction, so parallel calls race.
pub async fn codex_bulk_import(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(accounts) = normalize_accounts(&body) else {
        return err_json(StatusCode::BAD_REQUEST, "No accounts provided");
    };
    if accounts.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "No accounts provided");
    }

    let db = match db_of(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let mut results = Vec::new();
    let mut success = 0usize;
    let mut failed = 0usize;

    for (index, raw) in accounts.iter().enumerate() {
        match import_codex_account(&db, raw) {
            Ok(id) => {
                success += 1;
                results.push(json!({ "index": index, "ok": true, "id": id }));
            }
            Err(message) => {
                failed += 1;
                results.push(json!({ "index": index, "ok": false, "error": message }));
            }
        }
    }

    Json(json!({ "success": success, "failed": failed, "results": results })).into_response()
}

fn import_codex_account(db: &Db, raw: &Value) -> Result<Value, String> {
    if !raw.is_object() {
        return Err("Item is not an object".into());
    }
    let mut item = strip_server_fields(raw);

    let access_token = item
        .get("accessToken")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or("Missing accessToken")?;

    // Backfill missing identity fields from JWT claims.
    let mut psd = item
        .get("providerSpecificData")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let needs_email = item.get("email").map(|v| v.is_null()).unwrap_or(true);
    let needs_account = !psd.contains_key("chatgptAccountId");
    let needs_plan = !psd.contains_key("chatgptPlanType");

    if needs_email || needs_account || needs_plan {
        let source = item
            .get("idToken")
            .and_then(|v| v.as_str())
            .unwrap_or(&access_token);
        let info = extract_codex_account_info(source);
        if needs_email {
            if let Some(email) = info.get("email").filter(|v| !v.is_null()) {
                item["email"] = email.clone();
            }
        }
        if needs_account {
            if let Some(id) = info.get("chatgptAccountId").filter(|v| !v.is_null()) {
                psd.insert("chatgptAccountId".into(), id.clone());
            }
        }
        if needs_plan {
            if let Some(plan) = info.get("chatgptPlanType").filter(|v| !v.is_null()) {
                psd.insert("chatgptPlanType".into(), plan.clone());
            }
        }
    }
    if !psd.is_empty() {
        item["providerSpecificData"] = Value::Object(psd);
    }

    // Compute expiresAt from expiresIn when absent.
    let has_expires_at = item
        .get("expiresAt")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if !has_expires_at {
        if let Some(expires_in) = item.get("expiresIn").and_then(|v| v.as_i64()) {
            if expires_in > 0 {
                item["expiresAt"] = json!(iso_in(expires_in));
            }
        }
    }

    // Defaults aligned with the OAuth-completed flow.
    if item.get("testStatus").is_none() {
        item["testStatus"] = json!("active");
    }
    if item.get("isActive").is_none() {
        item["isActive"] = json!(true);
    }
    if item.get("lastRefreshAt").map(|v| v.is_null()).unwrap_or(true) {
        item["lastRefreshAt"] = json!(chrono::Utc::now().to_rfc3339());
    }

    item["provider"] = json!("codex");
    item["authType"] = json!("oauth");

    create_provider_connection(db, &item)
        .map(|conn| conn.get("id").cloned().unwrap_or(Value::Null))
        .map_err(|e| e.to_string())
}

// ─── grok-cli ───────────────────────────────────────────────────────────────

/// POST /api/oauth/grok-cli/bulk-import
///
/// Items accept snake_case or camelCase. Serial for the same priority reason.
pub async fn grok_cli_bulk_import(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(accounts) = normalize_accounts(&body) else {
        return err_json(StatusCode::BAD_REQUEST, "No accounts provided");
    };
    if accounts.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "No accounts provided");
    }

    let db = match db_of(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let mut results = Vec::new();
    let mut success = 0usize;
    let mut failed = 0usize;

    for (index, raw) in accounts.iter().enumerate() {
        match import_grok_account(&db, raw) {
            Ok((id, email)) => {
                success += 1;
                results.push(json!({ "index": index, "ok": true, "id": id, "email": email }));
            }
            Err(message) => {
                failed += 1;
                results.push(json!({ "index": index, "ok": false, "error": message }));
            }
        }
    }

    Json(json!({
        "total": accounts.len(),
        "success": success,
        "failed": failed,
        "results": results,
    }))
    .into_response()
}

fn import_grok_account(db: &Db, raw: &Value) -> Result<(Value, Value), String> {
    if !raw.is_object() {
        return Err("Item is not an object".into());
    }

    let pick = |a: &str, b: &str| -> Option<String> {
        raw.get(a)
            .or_else(|| raw.get(b))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };

    let access_token = pick("access_token", "accessToken")
        .ok_or("Missing access_token / accessToken")?;
    let refresh_token = pick("refresh_token", "refreshToken");
    let id_token = pick("id_token", "idToken");

    let mut email = pick("email", "email");
    if email.as_deref().map(str::is_empty).unwrap_or(true) {
        email = id_token
            .as_deref()
            .and_then(oauth_local_creds_email_from_id_token)
            .or_else(|| extract_email_from_access_token(&access_token));
    }

    let mut expires_at = pick("expires_at", "expiresAt");
    if expires_at.is_none() {
        if let Some(expires_in) = raw
            .get("expires_in")
            .or_else(|| raw.get("expiresIn"))
            .and_then(|v| v.as_i64())
        {
            if expires_in > 0 {
                expires_at = Some(iso_in(expires_in));
            }
        }
    }

    let mut psd = serde_json::Map::new();
    psd.insert("authMethod".into(), json!("device_code"));
    if let Some(id_token) = &id_token {
        psd.insert("idToken".into(), json!(id_token));
    }
    if let Some(email) = &email {
        psd.insert("email".into(), json!(email));
    }
    if let Some(extra) = raw.get("providerSpecificData").and_then(|v| v.as_object()) {
        for (k, v) in extra {
            psd.insert(k.clone(), v.clone());
        }
    }

    let payload = json!({
        "provider": "grok-cli",
        "authType": "oauth",
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "expiresAt": expires_at,
        "email": email,
        "displayName": raw
            .get("displayName")
            .or_else(|| raw.get("name"))
            .cloned()
            .unwrap_or(Value::Null),
        "providerSpecificData": Value::Object(psd),
        "testStatus": "active",
    });

    let conn = create_provider_connection(db, &payload).map_err(|e| e.to_string())?;
    Ok((
        conn.get("id").cloned().unwrap_or(Value::Null),
        conn.get("email").cloned().unwrap_or(Value::Null),
    ))
}

/// `decodeXaiIdTokenEmail`: `email` → `preferred_username` → `sub`.
fn oauth_local_creds_email_from_id_token(id_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(id_token)?;
    for key in ["email", "preferred_username", "sub"] {
        if let Some(value) = payload.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

// ─── simple single-shot imports ─────────────────────────────────────────────

/// POST /api/oauth/gitlab/pat — verify the PAT then store the connection.
pub async fn gitlab_pat(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let Some(token) = trimmed(&body, "token") else {
        return err_json(StatusCode::BAD_REQUEST, "Personal Access Token is required");
    };
    let base = trimmed(&body, "baseUrl")
        .unwrap_or("https://gitlab.com")
        .trim_end_matches('/')
        .to_string();

    // SSRF guard: the base URL is user-supplied.
    if let Err(reason) = nr_engine::ssrf::assert_public_url(&format!("{base}/api/v4/user")) {
        return err_json(StatusCode::BAD_REQUEST, format!("Invalid GitLab base URL: {reason}"));
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let response = match client
        .get(format!("{base}/api/v4/user"))
        .header("PRIVATE-TOKEN", token)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return err_json(
            StatusCode::UNAUTHORIZED,
            format!("GitLab token verification failed: {text} (HTTP {status})"),
        );
    }

    let user: Value = response.json().await.unwrap_or_else(|_| json!({}));
    let pick = |key: &str| {
        user.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let email = {
        let primary = pick("email");
        if primary.is_empty() {
            pick("public_email")
        } else {
            primary
        }
    };
    let name = pick("name");
    let username = pick("username");
    let display_name = if !name.is_empty() {
        name.clone()
    } else if !username.is_empty() {
        username.clone()
    } else {
        email.clone()
    };

    let db = match db_of(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let payload = json!({
        "provider": "gitlab",
        "authType": "oauth",
        "accessToken": token,
        "refreshToken": Value::Null,
        "expiresAt": Value::Null,
        "email": email,
        "displayName": display_name,
        "testStatus": "active",
        "providerSpecificData": {
            "username": username,
            "email": email,
            "name": name,
            "baseUrl": base,
            "authKind": "personal_access_token",
        },
    });

    match create_provider_connection(&db, &payload) {
        Ok(_) => Json(json!({ "success": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/oauth/perchai/import — store a Perch session, falling back to the
/// CLI session file when no token is posted.
pub async fn perchai_import(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let mut access_token = trimmed(&body, "accessToken").map(str::to_string);
    let mut refresh_token = trimmed(&body, "refreshToken").map(str::to_string);
    let mut email = body.get("email").cloned().filter(|v| !v.is_null());
    let mut user_id = body.get("userId").cloned().filter(|v| !v.is_null());
    // JS treats `expiresAt` as epoch seconds here.
    let mut expires_at = body.get("expiresAt").and_then(|v| v.as_i64()).map(iso_at);

    if access_token.is_none() {
        let cli = oauth_local_creds::read_perch_cli_credentials();
        if cli.get("found").and_then(|v| v.as_bool()) != Some(true) {
            return err_json(
                StatusCode::BAD_REQUEST,
                "Access token is required or run `perch login` first",
            );
        }
        access_token = cli.get("accessToken").and_then(|v| v.as_str()).map(str::to_string);
        refresh_token = refresh_token.or_else(|| {
            cli.get("refreshToken").and_then(|v| v.as_str()).map(str::to_string)
        });
        if email.is_none() {
            email = cli.get("email").cloned().filter(|v| !v.is_null());
        }
        if user_id.is_none() {
            user_id = cli.get("userId").cloned().filter(|v| !v.is_null());
        }
        if expires_at.is_none() {
            expires_at = cli.get("expiresAt").and_then(|v| v.as_i64()).map(iso_at);
        }
    }

    let Some(access_token) = access_token else {
        return err_json(StatusCode::BAD_REQUEST, "Access token is required");
    };

    let display_name = match email.as_ref().and_then(|v| v.as_str()) {
        Some(email) => format!("Perch AI ({email})"),
        None => "Perch AI (CLI)".to_string(),
    };

    let db = match db_of(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let payload = json!({
        "provider": "perchai",
        "authType": "oauth",
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "expiresAt": expires_at,
        "email": email,
        "displayName": display_name,
        "providerSpecificData": {
            "authMethod": if refresh_token.is_some() { "oauth" } else { "access_token" },
            "userId": user_id,
            "appUrl": "https://app.perchai.app",
            "source": "cli-import",
        },
        "testStatus": "active",
    });

    match create_provider_connection(&db, &payload) {
        Ok(conn) => Json(json!({
            "success": true,
            "connection": {
                "id": conn.get("id").cloned().unwrap_or(Value::Null),
                "provider": conn.get("provider").cloned().unwrap_or(Value::Null),
                "email": conn.get("email").cloned().unwrap_or(Value::Null),
                "displayName": conn.get("displayName").cloned().unwrap_or(Value::Null),
            },
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ─── auto-import (GET) ──────────────────────────────────────────────────────

/// GET /api/oauth/commandcode/auto-import
pub async fn commandcode_auto_import() -> Response {
    Json(oauth_local_creds::read_commandcode_credentials()).into_response()
}

/// GET /api/oauth/cursor/auto-import
pub async fn cursor_auto_import() -> Response {
    Json(oauth_local_creds::read_cursor_credentials()).into_response()
}

/// GET /api/oauth/kiro/auto-import
pub async fn kiro_auto_import() -> Response {
    Json(oauth_local_creds::read_kiro_credentials()).into_response()
}

/// GET /api/oauth/perchai/auto-import
pub async fn perchai_auto_import() -> Response {
    let result = oauth_local_creds::read_perch_cli_credentials();
    if result.get("found").and_then(|v| v.as_bool()) == Some(true) {
        return Json(json!({
            "found": true,
            "accessToken": result.get("accessToken").cloned().unwrap_or(Value::Null),
            "refreshToken": result.get("refreshToken").cloned().unwrap_or(Value::Null),
            "email": result.get("email").cloned().unwrap_or(Value::Null),
            "userId": result.get("userId").cloned().unwrap_or(Value::Null),
            "expiresAt": result.get("expiresAt").cloned().unwrap_or(Value::Null),
            "source": result.get("source").cloned().unwrap_or(Value::Null),
        }))
        .into_response();
    }
    Json(json!({
        "found": false,
        "error": result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Perch CLI credentials not found"),
    }))
    .into_response()
}

/// GET /api/oauth/xiaomi-mimo/auto-import
pub async fn xiaomi_mimo_auto_import() -> Response {
    Json(oauth_local_creds::read_xiaomi_mimo_credentials()).into_response()
}

/// GET /api/oauth/zed/auto-import
pub async fn zed_auto_import() -> Response {
    let result = oauth_local_creds::read_zed_ide_credentials();
    if result.get("found").and_then(|v| v.as_bool()) == Some(true) {
        return Json(json!({
            "found": true,
            "userId": result.get("userId").cloned().unwrap_or(Value::Null),
            "accessToken": result.get("accessToken").cloned().unwrap_or(Value::Null),
            "systemId": result.get("systemId").cloned().unwrap_or(Value::Null),
            "credentialsUrl": result.get("credentialsUrl").cloned().unwrap_or(Value::Null),
        }))
        .into_response();
    }
    Json(json!({
        "found": false,
        "error": result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Zed IDE credentials not found"),
        "credentialsUrl": result.get("credentialsUrl").cloned().unwrap_or(Value::Null),
    }))
    .into_response()
}

pub fn routes() -> Router {
    Router::new()
        // codex
        .route("/api/oauth/codex/import-token", post(codex_import_token))
        .route("/api/oauth/codex/bulk-import", post(codex_bulk_import))
        // grok-cli
        .route("/api/oauth/grok-cli/bulk-import", post(grok_cli_bulk_import))
        // gitlab
        .route("/api/oauth/gitlab/pat", post(gitlab_pat))
        // perchai import surface (plan routes live in oauth_flow)
        .route("/api/oauth/perchai/import", post(perchai_import))
        .route("/api/oauth/perchai/auto-import", get(perchai_auto_import))
        // auto-imports that only read local state
        .route(
            "/api/oauth/commandcode/auto-import",
            get(commandcode_auto_import),
        )
        .route("/api/oauth/cursor/auto-import", get(cursor_auto_import))
        .route("/api/oauth/kiro/auto-import", get(kiro_auto_import))
        .route("/api/oauth/xiaomi-mimo/auto-import", get(xiaomi_mimo_auto_import))
        .route("/api/oauth/zed/auto-import", get(zed_auto_import))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn normalizes_all_three_body_shapes() {
        assert_eq!(normalize_accounts(&json!([{ "a": 1 }])).unwrap().len(), 1);
        assert_eq!(
            normalize_accounts(&json!({ "accounts": [{ "a": 1 }, { "b": 2 }] }))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(normalize_accounts(&json!({ "accessToken": "t" })).unwrap().len(), 1);
        assert!(normalize_accounts(&json!("string")).is_none());
    }

    #[test]
    fn strips_server_controlled_fields() {
        let item = strip_server_fields(&json!({
            "id": "x", "provider": "codex", "authType": "oauth",
            "createdAt": "a", "updatedAt": "b", "accessToken": "t"
        }));
        assert!(item.get("id").is_none());
        assert!(item.get("provider").is_none());
        assert_eq!(item["accessToken"], json!("t"));
    }

    #[tokio::test]
    async fn codex_import_token_requires_a_token() {
        let (_dir, db) = test_db();
        let resp = codex_import_token(Some(Extension(db)), Json(json!({}))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn codex_import_token_stores_connection() {
        let (_dir, db) = test_db();
        let resp = codex_import_token(
            Some(Extension(db)),
            Json(json!({ "accessToken": "not-a-jwt", "name": "my token" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn codex_bulk_import_reports_per_item_results() {
        let (_dir, db) = test_db();
        let resp = codex_bulk_import(
            Some(Extension(db)),
            Json(json!({
                "accounts": [
                    { "accessToken": "tok-1" },
                    { "nameless": true }
                ]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], json!(1));
        assert_eq!(json["failed"], json!(1));
        assert_eq!(json["results"][1]["ok"], json!(false));
        assert!(json["results"][1]["error"]
            .as_str()
            .unwrap()
            .contains("accessToken"));
    }

    #[tokio::test]
    async fn bulk_import_rejects_empty_body() {
        let (_dir, db) = test_db();
        let resp = codex_bulk_import(Some(Extension(db)), Json(json!({ "accounts": [] }))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn grok_bulk_import_accepts_snake_case() {
        let (_dir, db) = test_db();
        let resp = grok_cli_bulk_import(
            Some(Extension(db)),
            Json(json!({ "access_token": "tok", "refresh_token": "r" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], json!(1));
        assert_eq!(json["success"], json!(1));
    }

    #[tokio::test]
    async fn perchai_import_requires_a_token_or_cli_session() {
        std::env::set_var("PERCH_CLI_AUTH_DIR", "/nonexistent-perch-dir");
        let (_dir, db) = test_db();
        let resp = perchai_import(Some(Extension(db)), Json(json!({}))).await;
        std::env::remove_var("PERCH_CLI_AUTH_DIR");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn perchai_import_stores_posted_token() {
        let (_dir, db) = test_db();
        let resp = perchai_import(
            Some(Extension(db)),
            Json(json!({ "accessToken": "tok", "email": "e@x" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn id_token_email_uses_the_same_claim_order() {
        use base64::Engine;
        let payload = json!({ "preferred_username": "grok-user" });
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("h.{body}.s");
        assert_eq!(
            oauth_local_creds_email_from_id_token(&token).as_deref(),
            Some("grok-user")
        );
    }
}
