//! Port of open-sse/shared/awsCredentials.js
//! AWS credential resolution for Bedrock in static and profile modes.

use lazy_static::lazy_static;
use nr_core::{Credentials, Result};
use regex::Regex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// AWS credential resolution modes
pub mod mode {
    pub const STATIC: &str = "static";
    pub const PROFILE: &str = "profile";
}

/// AWS credential constants (from awsConstants.js)
const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
const DEFAULT_REGION: &str = "us-east-1";
const CREDENTIAL_REFRESH_LEAD_MS: u64 = 5 * 60 * 1000;
const CREDENTIAL_NO_EXPIRY_TTL_MS: u64 = 60 * 1000;
const CREDENTIAL_RESOLVE_TIMEOUT_MS: u64 = 10 * 1000;

lazy_static! {
    static ref AWS_REGION_PATTERN: Regex = Regex::new(r"^[a-z0-9][a-z0-9-]{0,30}$").unwrap();
    static ref AWS_PROFILE_PATTERN: Regex =
        Regex::new(r"^[\w@+.%: -]{1,64}$").unwrap();
    static ref CREDENTIAL_CACHE: Mutex<HashMap<String, CachedCredential>> = Mutex::new(HashMap::new());
}

#[derive(Clone)]
struct CachedCredential {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expires_at: u64,
}

/// Resolved AWS credentials with region and metadata.
#[derive(Clone, Debug)]
pub struct ResolvedAwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: String,
    pub mode: String,
    pub expires_at: Option<u64>,
}

/// Test seam: drop cached credentials between cases.
pub fn clear_aws_credential_cache() {
    if let Ok(mut cache) = CREDENTIAL_CACHE.lock() {
        cache.clear();
    }
}

/// Detect whether a connection is configured for static keys or profile mode.
/// A named profile wins over stray static keys.
pub fn detect_credential_mode(credentials: &Credentials) -> String {
    if credentials.psd_str("profile").is_some() {
        mode::PROFILE.to_string()
    } else {
        mode::STATIC.to_string()
    }
}

/// Resolve AWS region from connection setting, env vars, or default.
/// Validates the region because it is interpolated into the request hostname.
pub fn resolve_region(
    credentials: &Credentials,
    env_vars: Option<&HashMap<String, String>>,
) -> Result<String> {
    let env = env_vars.unwrap_or(&Default::default());

    // Try connection setting first
    if let Some(region) = credentials.psd_str("region") {
        let trimmed = region.trim();
        if validate_region(trimmed) {
            return Ok(trimmed.to_string());
        }
    }

    // Try AWS_REGION
    if let Some(region) = env.get("AWS_REGION") {
        if validate_region(region) {
            return Ok(region.clone());
        }
    }

    // Try AWS_DEFAULT_REGION
    if let Some(region) = env.get("AWS_DEFAULT_REGION") {
        if validate_region(region) {
            return Ok(region.clone());
        }
    }

    // Default to us-east-1
    Ok(DEFAULT_REGION.to_string())
}

fn validate_region(region: &str) -> bool {
    AWS_REGION_PATTERN.is_match(region)
}

/// Read static credentials from connection config.
fn read_static_credentials(credentials: &Credentials) -> Result<ResolvedAwsCredentials> {
    let access_key_id = credentials
        .psd_str("accessKeyId")
        .ok_or_else(|| {
            nr_core::Error::BadRequest(
                "Bedrock static credentials are incomplete. Set the AWS secret access key as the \
                provider API key and providerSpecificData.accessKeyId to the AWS access key id, \
                or set providerSpecificData.profile to use a local AWS profile instead.".to_string()
            )
        })?
        .to_string();

    let secret_access_key = credentials.api_key.as_deref().ok_or_else(|| {
        nr_core::Error::BadRequest(
            "Bedrock static credentials are incomplete. Set the AWS secret access key as the \
            provider API key and providerSpecificData.accessKeyId to the AWS access key id, \
            or set providerSpecificData.profile to use a local AWS profile instead.".to_string()
        )
    })?
    .to_string();

    // Validate temporary keys
    if access_key_id.starts_with("ASIA") {
        if credentials.psd_str("sessionToken").is_none() {
            return Err(nr_core::Error::BadRequest(
                "This looks like a temporary AWS access key (ASIA…) but no sessionToken was provided. \
                Add providerSpecificData.sessionToken, or use providerSpecificData.profile so \
                credentials are resolved and refreshed automatically.".to_string()
            ));
        }
    }

    let region = resolve_region(credentials, None)?;
    let session_token = credentials.psd_str("sessionToken").map(|s| s.to_string());

    Ok(ResolvedAwsCredentials {
        access_key_id,
        secret_access_key,
        session_token,
        region,
        mode: mode::STATIC.to_string(),
        expires_at: None,
    })
}

fn validate_profile(profile: &str) -> Result<()> {
    if !AWS_PROFILE_PATTERN.is_match(profile) {
        return Err(nr_core::Error::BadRequest(
            format!(
                "Invalid AWS profile name {:?}. Use the profile's name as it appears in \
                ~/.aws/config or ~/.aws/credentials: letters, digits, spaces and _ - @ + . % :, \
                up to 64 characters.",
                profile
            )
        ));
    }
    Ok(())
}

/// Compute when a resolved credential expires, accounting for refresh lead time.
fn compute_expires_at(expiration: Option<u64>, now: u64) -> u64 {
    match expiration {
        Some(exp) => {
            if exp > CREDENTIAL_REFRESH_LEAD_MS {
                exp - CREDENTIAL_REFRESH_LEAD_MS
            } else {
                now
            }
        }
        None => now + CREDENTIAL_NO_EXPIRY_TTL_MS,
    }
}

/// Get current time in milliseconds since UNIX_EPOCH.
fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

/// Resolve AWS credentials from connection config.
/// Supports both static keys and profile-based resolution.
pub async fn resolve_aws_credentials(
    credentials: &Credentials,
) -> Result<ResolvedAwsCredentials> {
    let mode = detect_credential_mode(credentials);

    // Static keys: no caching, no I/O
    if mode == mode::STATIC {
        return read_static_credentials(credentials);
    }

    // Profile mode: read from config files with caching
    let profile = credentials
        .psd_str("profile")
        .ok_or_else(|| {
            nr_core::Error::BadRequest("Profile mode requires providerSpecificData.profile".to_string())
        })?;

    validate_profile(profile)?;

    let region = resolve_region(credentials, None)?;
    let key = format!("profile:{}", profile);
    let now = current_time_ms();

    // Check cache
    if let Ok(cache) = CREDENTIAL_CACHE.lock() {
        if let Some(cached) = cache.get(&key) {
            if cached.expires_at > now {
                return Ok(ResolvedAwsCredentials {
                    access_key_id: cached.access_key_id.clone(),
                    secret_access_key: cached.secret_access_key.clone(),
                    session_token: cached.session_token.clone(),
                    region,
                    mode: mode::PROFILE.to_string(),
                    expires_at: Some(cached.expires_at),
                });
            }
        }
    }

    // Resolve from profile (would require actual AWS SDK or file parsing)
    // For now, return error indicating profile resolution is not yet fully implemented
    Err(nr_core::Error::BadRequest(
        format!(
            "Could not resolve AWS credentials for profile \"{}\": profile resolution not fully \
            implemented in this port. Use static credentials with accessKeyId and \
            secretAccessKey instead.",
            profile
        )
    ))
}
