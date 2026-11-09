//! OAuth provider implementations: authorize URL, code exchange, device code flow, polling.
//! Each provider module (claude.rs, github.rs, etc.) implements the same provider trait.

use async_trait::async_trait;
use nr_core::{Result, Logger, Json};
use super::{MappedTokens, DeviceCodeResponse};
use base64::Engine as _;

// Provider modules (oauth agent port of src/lib/oauth/providers/*.js)
// Assigned to oauth agent: antigravity, claude, cline, clinepass, codex, gemini-cli,
// github, gitlab, glm, iflow, kilocode, kimchi, kimi, minimax-code variants
mod claude;
mod cline;
mod clinepass;
mod codex;
mod gemini_cli;
mod github;
mod gitlab;
mod antigravity;
mod iflow;
mod kilocode;
mod glm;
mod kimchi;
mod kimi;
mod minimax_code;

// OAuth agent's provider ports for flows
mod zed;
mod cursor;
mod xai;
mod grok_cli;
mod muse;
mod kiro;
mod windsurf;
mod perchai;
mod nous;
mod qoder;
mod qoder_cn;
mod codebuddy_cn;
mod codebuddy_intl;
mod trae;
mod xiaomi_mimo;

/// OAuth provider interface: authorize URL building, code exchange, device flow.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Get provider configuration from registry.
    fn config_id(&self) -> &'static str;

    /// Flow type: "authorization_code_pkce", "authorization_code", or "device_code".
    fn flow_type(&self) -> &'static str;

    /// Build authorize URL for authorization_code flows.
    /// Returns None for device_code flows.
    async fn build_auth_url(
        &self,
        config: &Json,
        redirect_uri: &str,
        state: &str,
        code_challenge: Option<&str>,
        meta: Option<&Json>,
        log: &Logger,
    ) -> Result<Option<String>>;

    /// Exchange authorization code for access token.
    async fn exchange_token(
        &self,
        config: &Json,
        code: &str,
        redirect_uri: &str,
        code_verifier: Option<&str>,
        state: Option<&str>,
        meta: Option<&Json>,
        log: &Logger,
    ) -> Result<Json>;

    /// Request device code and user code (device_code flows only).
    async fn request_device_code(
        &self,
        config: &Json,
        code_challenge: Option<&str>,
        options: Option<&Json>,
        log: &Logger,
    ) -> Result<DeviceCodeResponse>;

    /// Poll for token after user authorizes (device_code flows only).
    /// Returns { ok: bool, data: { ... } } where data.access_token on success,
    /// data.error = authorization_pending | slow_down | ... while waiting.
    async fn poll_device_token(
        &self,
        config: &Json,
        device_code: &str,
        code_verifier: Option<&str>,
        extra_data: Option<&Json>,
        log: &Logger,
    ) -> Result<Json>;

    /// Post-exchange hook: fetch additional data (user info, copilot tokens, etc.).
    async fn post_exchange(
        &self,
        tokens: &Json,
        meta: Option<&Json>,
        log: &Logger,
    ) -> Result<Option<Json>>;

    /// Map raw token response to standard connection record schema.
    fn map_tokens(&self, tokens: &Json, extra: Option<&Json>) -> Result<MappedTokens>;
}

/// Get provider by name.
pub fn get_provider(name: &str) -> Option<&'static dyn OAuthProvider> {
    // Legacy: kimi-coding → kimi
    let key = if name == "kimi-coding" { "kimi" } else { name };
    let provider: &'static dyn OAuthProvider = match key {
        "claude" => &claude::PROVIDER,
        "cline" => &cline::PROVIDER,
        "clinepass" => &clinepass::PROVIDER,
        "codex" => &codex::PROVIDER,
        "gemini-cli" => &gemini_cli::PROVIDER,
        "github" => &github::PROVIDER,
        "gitlab" => &gitlab::PROVIDER,
        "antigravity" => &antigravity::PROVIDER,
        "iflow" => &iflow::PROVIDER,
        "kilocode" => &kilocode::PROVIDER,
        "kimchi" => &kimchi::PROVIDER,
        "kimi" => &kimi::PROVIDER,
        "glm" => &glm::PROVIDER,
        "minimax-code" => &minimax_code::PROVIDER_CN,
        "minimax-code-global" => &minimax_code::PROVIDER_GLOBAL,
        // OAuth agent flows providers
        "zed" => &zed::PROVIDER,
        "cursor" => &cursor::PROVIDER,
        "xai" => &xai::PROVIDER,
        "grok-cli" => &grok_cli::PROVIDER,
        "muse" => &muse::PROVIDER,
        "kiro" => &kiro::PROVIDER,
        "windsurf" => &windsurf::PROVIDER,
        "perchai" => &perchai::PROVIDER,
        "nous" => &nous::PROVIDER,
        "qoder" => &qoder::PROVIDER,
        "qoder-cn" => &qoder_cn::PROVIDER,
        "codebuddy-cn" => &codebuddy_cn::PROVIDER,
        "codebuddy-intl" => &codebuddy_intl::PROVIDER,
        "trae" => &trae::PROVIDER,
        "xiaomi-mimo" => &xiaomi_mimo::PROVIDER,
        _ => return None,
    };
    Some(provider)
}

/// Generate auth data (auth URL + PKCE state) for a provider.
pub async fn generate_auth_data(
    provider_name: &str,
    redirect_uri: &str,
    meta: Option<&Json>,
    log: &Logger,
) -> Result<Json> {
    let provider = get_provider(provider_name)
        .ok_or_else(|| nr_core::Error::BadRequest(format!("Unknown provider: {}", provider_name)))?;

    let config = nr_data::provider_oauth(provider_name)
        .ok_or_else(|| nr_core::Error::Config(format!("No OAuth config for: {}", provider_name)))?
        .clone();

    let state = uuid::Uuid::new_v4().to_string();
    let (code_verifier, code_challenge) = generate_pkce();

    let auth_url = provider
        .build_auth_url(&config, redirect_uri, &state, Some(&code_challenge), meta, log)
        .await?;

    Ok(serde_json::json!({
        "authUrl": auth_url,
        "state": state,
        "codeVerifier": code_verifier,
        "codeChallenge": code_challenge,
        "redirectUri": redirect_uri,
        "flowType": provider.flow_type(),
        "callbackPath": "/callback",
    }))
}

/// Exchange authorization code for tokens.
pub async fn exchange_code(
    provider_name: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
    state: Option<&str>,
    meta: Option<&Json>,
    log: &Logger,
) -> Result<MappedTokens> {
    let provider = get_provider(provider_name)
        .ok_or_else(|| nr_core::Error::BadRequest(format!("Unknown provider: {}", provider_name)))?;

    let config = nr_data::provider_oauth(provider_name)
        .ok_or_else(|| nr_core::Error::Config(format!("No OAuth config for: {}", provider_name)))?
        .clone();

    let tokens = provider
        .exchange_token(&config, code, redirect_uri, code_verifier, state, meta, log)
        .await?;

    let extra = provider.post_exchange(&tokens, meta, log).await?;
    provider.map_tokens(&tokens, extra.as_ref())
}

/// Request device code for device_code flow.
pub async fn request_device_code(
    provider_name: &str,
    code_challenge: Option<&str>,
    options: Option<&Json>,
    log: &Logger,
) -> Result<DeviceCodeResponse> {
    let provider = get_provider(provider_name)
        .ok_or_else(|| nr_core::Error::BadRequest(format!("Unknown provider: {}", provider_name)))?;

    if provider.flow_type() != "device_code" {
        return Err(nr_core::Error::BadRequest(format!(
            "Provider {} does not support device code flow",
            provider_name
        )));
    }

    let config = nr_data::provider_oauth(provider_name)
        .ok_or_else(|| nr_core::Error::Config(format!("No OAuth config for: {}", provider_name)))?
        .clone();

    provider
        .request_device_code(&config, code_challenge, options, log)
        .await
}

/// Poll for token in device_code flow.
pub async fn poll_device_token(
    provider_name: &str,
    device_code: &str,
    code_verifier: Option<&str>,
    extra_data: Option<&Json>,
    log: &Logger,
) -> Result<Json> {
    let provider = get_provider(provider_name)
        .ok_or_else(|| nr_core::Error::BadRequest(format!("Unknown provider: {}", provider_name)))?;

    if provider.flow_type() != "device_code" {
        return Err(nr_core::Error::BadRequest(format!(
            "Provider {} does not support device code flow",
            provider_name
        )));
    }

    let config = nr_data::provider_oauth(provider_name)
        .ok_or_else(|| nr_core::Error::Config(format!("No OAuth config for: {}", provider_name)))?
        .clone();

    provider
        .poll_device_token(&config, device_code, code_verifier, extra_data, log)
        .await
}

/// Generate PKCE code verifier and challenge (S256).
fn generate_pkce() -> (String, String) {
    use sha2::{Sha256, Digest};

    // Generate 32 random bytes and base64-encode (no padding)
    let random: [u8; 32] = rand::random();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(&random);

    // S256: SHA256(verifier), base64url encode
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(&digest);

    (verifier, challenge)
}

/// `decodeJwtPayload(jwt)` from src/lib/oauth/providerHelpers.js.
pub(crate) fn decode_jwt_payload(jwt: &str) -> Option<Json> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| {
            let normalized = payload.replace('-', "+").replace('_', "/");
            let padding = (4 - normalized.len() % 4) % 4;
            base64::engine::general_purpose::STANDARD.decode(format!("{normalized}{}", "=".repeat(padding)))
        })
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `decodeXaiIdTokenEmail(idToken)`.
pub(crate) fn decode_xai_id_token_email(id_token: Option<&str>) -> Option<String> {
    let payload = decode_jwt_payload(id_token?)?;
    ["email", "preferred_username", "sub"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Json::as_str))
        .map(str::to_string)
}

/// `extractEmailFromAccessToken(accessToken)`.
pub(crate) fn extract_email_from_access_token(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    ["email", "preferred_username", "sub"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Json::as_str))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_generation() {
        let (verifier, challenge) = generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);
    }

    #[test]
    fn test_get_known_providers() {
        assert!(get_provider("claude").is_some());
        assert!(get_provider("github").is_some());
        assert!(get_provider("unknown").is_none());
    }

    #[test]
    fn jwt_email_extraction_prefers_email_then_username_then_sub() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let jwt_with = |claims: Json| {
            format!(
                "h.{}.s",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            )
        };

        let with_email = jwt_with(serde_json::json!({"email": "a@b.c", "sub": "u1"}));
        assert_eq!(decode_xai_id_token_email(Some(&with_email)), Some("a@b.c".into()));
        assert_eq!(extract_email_from_access_token(&with_email), Some("a@b.c".into()));

        let with_username = jwt_with(serde_json::json!({"preferred_username": "dev"}));
        assert_eq!(decode_xai_id_token_email(Some(&with_username)), Some("dev".into()));

        let sub_only = jwt_with(serde_json::json!({"sub": "user-42"}));
        assert_eq!(decode_xai_id_token_email(Some(&sub_only)), Some("user-42".into()));

        // Malformed tokens never panic.
        assert_eq!(decode_xai_id_token_email(Some("not-a-jwt")), None);
        assert_eq!(decode_xai_id_token_email(None), None);
        assert_eq!(extract_email_from_access_token("a.b.c"), None);
    }
}
