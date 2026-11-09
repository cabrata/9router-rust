//! OAuth provider constants and configurations.
//! Ports src/lib/oauth/constants/oauth.js, src/lib/oauth/constants/xai.js, etc.

use serde_json::json;
use nr_core::JsonMap;

/// OAuth timeout (5 minutes)
pub const OAUTH_TIMEOUT_MS: u64 = 300_000;

/// AWS region pattern for SSRF prevention (GHSA-6mwv-4mrm-5p3m).
pub const AWS_REGION_PATTERN: &str = r"^[a-z]{2}-[a-z]+-\d{1,2}$";

/// Validate AWS region to prevent SSRF via region injection.
pub fn assert_valid_aws_region(region: &str) -> Result<(), String> {
    if let Ok(re) = regex::Regex::new(AWS_REGION_PATTERN) {
        if re.is_match(region) {
            return Ok(());
        }
    }
    Err("Invalid region".to_string())
}

/// Platform enum for Antigravity client metadata (matches binary's ClientMetadata.Platform).
pub fn get_oauth_platform_enum() -> u32 {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("macos", "aarch64") => 2,
        ("macos", _) => 1,
        ("linux", "aarch64") => 4,
        ("linux", _) => 3,
        ("windows", _) => 5,
        _ => 0,
    }
}

/// Get OAuth client metadata (platform-specific).
pub fn get_oauth_client_metadata() -> JsonMap {
    json!({
        "ideType": 9,
        "platform": get_oauth_platform_enum(),
        "pluginType": 2
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Claude OAuth Configuration (Authorization Code Flow with PKCE).
pub fn claude_config() -> JsonMap {
    json!({
        "clientId": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        "authorizeUrl": "https://api.anthropic.com/v1/oauth/authorize",
        "tokenUrl": "https://api.anthropic.com/v1/oauth/token",
        "scopes": ["user:email"],
        "codeChallengeMethod": "S256"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Codex (OpenAI) OAuth Configuration.
pub fn codex_config() -> JsonMap {
    json!({
        "clientId": "0cc98f3a-97b9-4f30-8d10-e7cfed9c3e9e",
        "authorizeUrl": "https://chatgpt.com/auth/login",
        "tokenUrl": "https://api.openai.com/oauth/token",
        "scopes": ["openid", "profile", "email"],
        "codeChallengeMethod": "S256"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// GitHub OAuth Configuration (Device Code Flow).
pub fn github_config() -> JsonMap {
    json!({
        "clientId": "178c6fc778ccc68efc45",
        "clientSecret": "34ff2c4e58615f83feb5e4b619e9c0599911d315",
        "deviceCodeUrl": "https://github.com/login/device/code",
        "tokenUrl": "https://github.com/login/oauth/access_token",
        "authorizeUrl": "https://github.com/login/oauth/authorize",
        "scope": "read:user user:email",
        "flowType": "device_code"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Qoder OAuth Configuration (Device Token Flow with PKCE).
pub fn qoder_config() -> JsonMap {
    json!({
        "clientId": "qoder_cli_default",
        "deviceCodeUrl": "https://center.qoder.sh/api/device/auth",
        "tokenUrl": "https://center.qoder.sh/api/device/token",
        "flowType": "device_code",
        "codeChallengeMethod": "S256"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Qoder CN Configuration (qoder.com.cn).
pub fn qoder_cn_config() -> JsonMap {
    json!({
        "clientId": "qoder_cli_default_cn",
        "deviceCodeUrl": "https://api.qoder.com.cn/api/device/auth",
        "tokenUrl": "https://api.qoder.com.cn/api/device/token",
        "flowType": "device_code",
        "codeChallengeMethod": "S256"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// iFlow OAuth Configuration (Authorization Code).
pub fn iflow_config() -> JsonMap {
    json!({
        "clientId": "iflow-cli",
        "authorizeUrl": "https://iflow.dev/oauth/authorize",
        "tokenUrl": "https://iflow.dev/oauth/token",
        "scopes": ["read:user"],
        "codeChallengeMethod": "S256"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// xAI OAuth Configuration (Authorization Code with PKCE, dynamic endpoint discovery).
// ponytail: XAI_CONFIG used fn (not lazy_static) to match sibling config accessors; flip back if mutable/global needed
pub fn xai_config() -> JsonMap {
    json!({
        "clientId": "xai-cli",
        "authorizeUrl": "https://auth.x.ai/authorize",
        "tokenUrl": "https://auth.x.ai/token",
        "discoveryUrl": "https://auth.x.ai/.well-known/oauth-authorization-server",
        "scope": "openid profile email",
        "codeChallengeMethod": "S256",
        "loopbackPort": 8080,
        "callbackPath": "/callback"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// xAI PKCE verifier bytes (96 bytes for xAI, vs 32 for most others).
pub const XAI_PKCE_VERIFIER_BYTES: usize = 96;

/// Kiro OAuth Configuration (multi-method: AWS Builder ID / IDC / Social / Import Token).
pub fn kiro_config() -> JsonMap {
    json!({
        "authorizeUrl": "https://signin.aws.amazon.com/oauth",
        "tokenUrl": "https://signin.aws.amazon.com/oauth/token",
        "clientId": "kiro-cli",
        "flowType": "authorization_code_pkce",
        "codeChallengeMethod": "S256"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Cursor OAuth Configuration (Import Token from Cursor IDE).
pub fn cursor_config() -> JsonMap {
    json!({
        "flowType": "import_token",
        "tokenStoragePaths": {
            "linux": "~/.config/Cursor/User/globalStorage/state.vscdb",
            "macos": "/Users/<user>/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
            "windows": "%APPDATA%\\Cursor\\User\\globalStorage\\state.vscdb"
        }
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Cline OAuth Configuration (Local Callback Flow via app.cline.bot).
pub fn cline_config() -> JsonMap {
    json!({
        "clientId": "cline-cli",
        "authorizeUrl": "https://app.cline.bot/oauth/authorize",
        "tokenUrl": "https://api.cline.bot/api/v1/auth/token",
        "flowType": "authorization_code",
        "codeChallengeMethod": "S256",
        "callbackPath": "/callback",
        "loopbackPort": 8080
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Trae (ByteDance marscode) OAuth Configuration.
pub fn trae_config() -> JsonMap {
    json!({
        "clientId": "ono9krqynydwx5",
        "clientSecret": "-",
        "flowType": "trae_custom",
        "loginGuidanceUrls": [
            "https://api.marscode.com/cloudide/api/v3/trae/GetLoginGuidance",
            "https://api.trae.ai/cloudide/api/v3/trae/GetLoginGuidance"
        ],
        "exchangeTokenPath": "/cloudide/api/v3/trae/oauth/ExchangeToken",
        "callbackPath": "/callback",
        "timeoutMs": 600_000
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Zed hosted LLM aggregator OAuth Configuration.
pub fn zed_config() -> JsonMap {
    json!({
        "webBaseUrl": "https://zed.dev",
        "cloudBaseUrl": "https://cloud.zed.dev",
        "llmBaseUrl": "https://cloud.zed.dev",
        "defaultNativeAppPort": 58443,
        "flowType": "rsa_keypair",
        "oauthTimeoutMs": 600_000
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Nous Research Portal OAuth Configuration (Device Code flow).
pub fn nous_config() -> JsonMap {
    json!({
        "clientId": "hermes-cli",
        "portalBaseUrl": "https://portal.nousresearch.com",
        "deviceCodeUrl": "https://portal.nousresearch.com/api/oauth/device/code",
        "tokenUrl": "https://portal.nousresearch.com/api/oauth/token",
        "agentKeyUrl": "https://portal.nousresearch.com/api/oauth/agent-key",
        "inferenceBaseUrl": "https://inference-api.nousresearch.com/v1",
        "scope": "inference:invoke inference:mint_agent_key",
        "flowType": "device_code"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Google OAuth Client (shared with Antigravity, Gemini).
pub fn google_oauth_client() -> JsonMap {
    json!({
        "clientId": "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com",
        "clientSecret": "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"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// Antigravity OAuth Client (custom Google OAuth).
pub fn antigravity_oauth_client() -> JsonMap {
    json!({
        "clientId": "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com",
        "clientSecret": "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"
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}
