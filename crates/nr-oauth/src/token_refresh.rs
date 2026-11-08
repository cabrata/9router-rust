//! Port of open-sse/services/tokenRefresh.js and tokenRefresh/{providers,dedup}.js.
//!
//! FROZEN signatures (executor crates call them while this is being ported).
//! Each mirrors the JS function of the same name; JS returns the refreshed
//! fields (`accessToken`, `refreshToken`, `expiresIn`/`expiresAt`, …) or `null`.

use serde_json::json;

use nr_core::http::ProxyOptions;
use nr_core::{Credentials, Json, JsonMap, Logger};

const TOKEN_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000; // 5 minutes

pub fn is_unrecoverable_refresh_error(result: &Option<JsonMap>) -> bool {
    match result {
        Some(map) => {
            if let Some(Json::String(err)) = map.get("error") {
                matches!(err.as_str(), "unrecoverable_refresh_error" | "refresh_token_reused" | "invalid_request" | "invalid_grant")
            } else {
                false
            }
        }
        None => false,
    }
}

pub fn get_refresh_lead_ms(provider: &str) -> i64 {
    // TODO(wave2): read from REFRESH_LEAD_MS config when available
    if provider == "kimi-coding" {
        // Legacy id after kimi-coding → kimi merge
        return TOKEN_EXPIRY_BUFFER_MS; // TODO: check REFRESH_LEAD_MS.kimi
    }
    TOKEN_EXPIRY_BUFFER_MS
}

/// JS `parseVertexSaJson(apiKey)`: the service-account JSON, or `None`.
pub fn parse_vertex_sa_json(api_key: &str) -> Option<Json> {
    let parsed: JsonMap = match serde_json::from_str(api_key).ok() {
        Some(Json::Object(m)) => m,
        _ => return None,
    };

    // Must have type, client_email, private_key, project_id
    if parsed.get("type")?.as_str()? == "service_account"
        && parsed.get("client_email")?.as_str().is_some()
        && parsed.get("private_key")?.as_str().is_some()
        && parsed.get("project_id")?.as_str().is_some()
    {
        Some(Json::Object(parsed))
    } else {
        None
    }
}

/// JS `refreshVertexToken(saJson, log)`.
pub async fn refresh_vertex_token(sa_json: &Json, log: &Logger) -> Option<JsonMap> {
    let sa = sa_json.as_object()?;
    let client_email = sa.get("client_email")?.as_str()?;
    let _private_key_str = sa.get("private_key")?.as_str()?;
    let _project_id = sa.get("project_id")?.as_str()?;

    // Mint new token using JWT
    log.debug("TOKEN_REFRESH", &format!("Vertex minting token for {}", client_email));

    // TODO(wave2): implement JWT signing for service account (requires jose crate)
    // shortcut: Vertex token refresh requires asymmetric signing, skipped until full crypto support
    log.error("TOKEN_REFRESH", "Vertex token refresh not yet implemented");
    None
}

/// JS `refreshGoogleToken(refreshToken, clientId, clientSecret, log)`.
pub async fn refresh_google_token(
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
    log: &Logger,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];

    let response = match client
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Network error refreshing Google token: {}", e));
            return None;
        }
    };

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        log.error(
            "TOKEN_REFRESH",
            &format!("Failed to refresh Google token: {}", error_text),
        );
        return None;
    }

    match response.json::<JsonMap>().await {
        Ok(tokens) => {
            log.info(
                "TOKEN_REFRESH",
                "Successfully refreshed Google token",
            );
            let access_token = tokens.get("access_token")?.as_str()?;
            let new_refresh = tokens.get("refresh_token").and_then(|v| v.as_str()).unwrap_or(refresh_token);
            let mut result = JsonMap::new();
            result.insert("accessToken".to_string(), json!(access_token));
            result.insert("refreshToken".to_string(), json!(new_refresh));
            if let Some(expires_in) = tokens.get("expires_in") {
                result.insert("expiresIn".to_string(), expires_in.clone());
            }
            Some(result)
        }
        Err(e) => {
            log.error("TOKEN_REFRESH", &format!("Failed to parse Google token response: {}", e));
            None
        }
    }
}

/// JS `refreshKiroToken(refreshToken, providerSpecificData, log, proxyOptions)`.
pub async fn refresh_kiro_token(
    refresh_token: &str,
    _provider_specific_data: Option<&JsonMap>,
    log: &Logger,
    _proxy: Option<&ProxyOptions>,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Kiro token refresh with external_idp support
    log.warn("TOKEN_REFRESH", "Kiro token refresh not yet fully implemented");
    None
}

/// JS `refreshMiniMaxCodeToken(provider, refreshToken, log)`.
pub async fn refresh_minimax_code_token(
    provider: &str,
    refresh_token: &str,
    log: &Logger,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    let token_url = match provider {
        "minimax-code" => "https://account.minimax.cn/oauth2/token",
        "minimax-code-global" => "https://account.minimax.io/oauth2/token",
        _ => return None,
    };

    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", "mcode-public"),
        ("scope", "agent.default"),
        ("audience", "agent-backend"),
    ];

    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.post(token_url).form(&params).send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            log.error(
                "TOKEN_REFRESH",
                &format!("MiniMax token refresh failed ({}): {}", provider, e),
            );
            return None;
        }
        Err(_) => {
            log.error("TOKEN_REFRESH", "MiniMax token refresh timed out");
            return None;
        }
    };

    let data: JsonMap = response.json().await.ok()?;

    if data.get("error").and_then(|v| v.as_str()) == Some("invalid_grant") {
        log.warn(
            "TOKEN_REFRESH",
            &format!("MiniMax refresh token spent/rejected ({}) — sign-in gone", provider),
        );
        let mut error_result = JsonMap::new();
        error_result.insert("error".to_string(), json!("invalid_grant"));
        return Some(error_result);
    }

    let access_token = data.get("access_token")?.as_str()?;
    let new_refresh = data.get("refresh_token").and_then(|v| v.as_str()).unwrap_or(refresh_token);
    let mut result = JsonMap::new();
    result.insert("accessToken".to_string(), json!(access_token));
    result.insert("refreshToken".to_string(), json!(new_refresh));
    if let Some(expires_in) = data.get("expires_in") {
        result.insert("expiresIn".to_string(), expires_in.clone());
    }
    Some(result)
}

/// Cline OAuth refresh
pub async fn refresh_cline_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Cline token refresh via WorkOS API
    log.warn("TOKEN_REFRESH", "Cline token refresh not yet implemented");
    None
}

/// Claude OAuth refresh (JSON body, client_id only)
pub async fn refresh_claude_oauth_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Claude OAuth token refresh
    // Similar to refreshAccessToken but with JSON body and no client_secret
    log.warn("TOKEN_REFRESH", "Claude OAuth refresh not yet implemented");
    None
}

/// Codex (OpenAI) token refresh
pub async fn refresh_codex_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Codex token refresh with error classification
    log.warn("TOKEN_REFRESH", "Codex token refresh not yet implemented");
    None
}

/// iFlow token refresh
pub async fn refresh_iflow_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // iFlow uses Basic Auth + form body; delegate to generic path
    // TODO(wave2): implement iFlow token refresh with Basic Auth
    log.warn("TOKEN_REFRESH", "iFlow token refresh not yet implemented");
    None
}

/// GitHub token refresh
pub async fn refresh_github_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // GitHub may omit client_secret; delegate to generic path
    // TODO(wave2): implement GitHub token refresh
    log.warn("TOKEN_REFRESH", "GitHub token refresh not yet implemented");
    None
}

/// Trae token refresh - POST ExchangeToken with JSON body
pub async fn refresh_trae_token(refresh_token: &str, _credentials: &Credentials, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Trae token refresh with ClientID/ClientSecret/UserID
    log.warn("TOKEN_REFRESH", "Trae token refresh not yet implemented");
    None
}

/// Zed token refresh - returns None (long-lived tokens, no refresh)
pub fn refresh_zed_token() -> Option<JsonMap> {
    None
}

/// Windsurf token refresh - apiKey is long-lived, returns None
pub async fn refresh_windsurf_token(log: &Logger) -> Option<JsonMap> {
    log.info("TOKEN_REFRESH", "windsurf: apiKey is long-lived (no refresh_token flow) — skipping");
    None
}

/// Nous Research Portal token refresh
pub async fn refresh_nous_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Nous token refresh + agent key minting
    log.warn("TOKEN_REFRESH", "Nous token refresh not yet implemented");
    None
}

/// xAI token refresh
pub async fn refresh_xai_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): load XaiService singleton and refresh
    log.warn("TOKEN_REFRESH", "xAI token refresh not yet implemented");
    None
}

/// CodeBuddy (Tencent) token refresh - POST with X-Refresh-Token header
pub async fn refresh_codebuddy_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement CodeBuddy token refresh with custom headers
    log.warn("TOKEN_REFRESH", "CodeBuddy token refresh not yet implemented");
    None
}

/// CodeBuddy International token refresh
pub async fn refresh_codebuddy_intl_token(refresh_token: &str, log: &Logger) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement CodeBuddy intl token refresh
    log.warn("TOKEN_REFRESH", "CodeBuddy intl token refresh not yet implemented");
    None
}

/// GitHub Copilot token refresh from GitHub access token
pub async fn refresh_copilot_token(github_access_token: &str, log: &Logger) -> Option<JsonMap> {
    if github_access_token.is_empty() {
        return None;
    }

    // TODO(wave2): implement Copilot token refresh from GitHub access token
    log.warn("TOKEN_REFRESH", "Copilot token refresh not yet implemented");
    None
}

/// JS `refreshTokenByProvider(provider, credentials, log)`.
pub async fn refresh_token_by_provider(
    provider: &str,
    creds: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    let refresh_token = creds.refresh_token()?;

    // Dispatch to provider-specific handlers
    match provider {
        "gemini-cli" | "antigravity" => {
            // Google OAuth with client credentials from config
            // TODO(wave2): implement with proper clientId/clientSecret from PROVIDERS config
            log.warn("TOKEN_REFRESH", "Google OAuth refresh not yet implemented for this provider");
            None
        }
        "claude" => refresh_claude_oauth_token(refresh_token, log).await,
        "codex" => refresh_codex_token(refresh_token, log).await,
        "iflow" => refresh_iflow_token(refresh_token, log).await,
        "github" => refresh_github_token(refresh_token, log).await,
        "kiro" => refresh_kiro_token(refresh_token, creds.psd(), log, None).await,
        "xai" | "grok-cli" | "gcli" => refresh_xai_token(refresh_token, log).await,
        "codebuddy-cn" => refresh_codebuddy_token(refresh_token, log).await,
        "codebuddy-intl" => refresh_codebuddy_intl_token(refresh_token, log).await,
        "trae" => refresh_trae_token(refresh_token, creds, log).await,
        "cline" | "clinepass" => refresh_cline_token(refresh_token, log).await,
        "zed" => refresh_zed_token(),
        "windsurf" => refresh_windsurf_token(log).await,
        "kimi" | "kimi-coding" => {
            // Kimi uses refreshAccessToken with X-Msh-* headers from providerSpecificData
            // TODO(wave2): implement with kimi-specific headers
            log.warn("TOKEN_REFRESH", "Kimi token refresh not yet implemented");
            None
        }
        "minimax-code" | "minimax-code-global" => {
            refresh_minimax_code_token(provider, refresh_token, log).await
        }
        "nous" => refresh_nous_token(refresh_token, log).await,
        "vertex" | "vertex-partner" => {
            // Vertex uses service account JSON in apiKey
            // TODO(wave2): parse vertex SA JSON and refresh
            log.warn("TOKEN_REFRESH", "Vertex token refresh not yet implemented");
            None
        }
        _ => {
            // Generic OAuth2 path for unknown providers
            refresh_access_token(provider, refresh_token, creds, log).await
        }
    }
}

// Generic OAuth2 refresh with config-driven profiles
async fn refresh_access_token(
    provider: &str,
    refresh_token: &str,
    _credentials: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    if refresh_token.is_empty() {
        log.warn("TOKEN_REFRESH", &format!("No refresh token for {}", provider));
        return None;
    }

    // TODO(wave2): implement full provider-specific refresh profiles
    // For now, return None for unknown providers
    log.warn("TOKEN_REFRESH", &format!("Generic token refresh not yet implemented for {}", provider));
    None
}
