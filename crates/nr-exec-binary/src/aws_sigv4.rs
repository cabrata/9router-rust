//! Port of open-sse/utils/awsSigv4.js
//! AWS Signature Version 4 request signing.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::BTreeMap;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

/// AWS SigV4 constants
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const TERMINATOR: &str = "aws4_request";
const KEY_PREFIX: &str = "AWS4";
const DATE_HEADER: &str = "X-Amz-Date";
const SECURITY_TOKEN_HEADER: &str = "X-Amz-Security-Token";

/// Percent-encode per RFC 3986, leaving only the unreserved set alone.
/// This is stricter than standard URL encoding.
pub fn escape_uri(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn sha256_hex(payload: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(payload);
    format!("{:x}", hasher.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key size valid");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Format date as "20150830T123600Z" and "20150830" for SigV4.
fn format_signing_date(date: chrono::DateTime<chrono::Utc>) -> (String, String) {
    let amz_date = date
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let date_stamp = amz_date[0..8].to_string();
    (amz_date, date_stamp)
}

/// Canonical URI path. Most services except S3 expect each path segment escaped a second time.
fn canonical_path(pathname: Option<&str>, double_encode_path: bool) -> String {
    let path = pathname.unwrap_or("/");
    if !double_encode_path {
        return path.to_string();
    }
    let segments: Vec<String> = path
        .split('/')
        .map(|segment| escape_uri(segment))
        .collect();
    segments.join("/")
}

/// Canonicalize headers: lowercase names, trim values, collapse whitespace.
fn canonicalize_headers(headers: &BTreeMap<String, String>) -> (String, String) {
    let mut entries: Vec<(String, String)> = headers
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(name, value)| (name.to_lowercase(), value.trim().split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers = entries
        .iter()
        .map(|(name, value)| format!("{}:{}\n", name, value))
        .collect::<String>();

    let signed_headers = entries
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>()
        .join(";");

    (canonical_headers, signed_headers)
}

/// AWS SigV4 signing request options
pub struct SigningOptions {
    pub method: String,
    pub url: String,
    pub headers: Option<BTreeMap<String, String>>,
    pub body: Option<Vec<u8>>,
    pub region: String,
    pub service: String,
    pub credentials: AwsCredentials,
    pub double_encode_path: bool,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
}

/// AWS credentials for signing
#[derive(Clone, Debug)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Sign an AWS request and return headers to send with it.
pub fn sign_aws_request(opts: SigningOptions) -> Result<BTreeMap<String, String>, String> {
    let SigningOptions {
        method,
        url,
        headers,
        body,
        region,
        service,
        credentials,
        double_encode_path,
        date,
    } = opts;

    if credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty() {
        return Err("AWS SigV4 requires both accessKeyId and secretAccessKey".to_string());
    }

    let parsed = Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;

    // Query canonicalisation is deliberately NOT implemented. Every request here is query-less POST.
    if !parsed.query().unwrap_or("").is_empty() {
        return Err(
            format!(
                "AWS SigV4 signing does not support query strings (got {}). Move parameters into the body.",
                parsed.query().unwrap_or("")
            )
        );
    }

    let date_time = date.unwrap_or_else(chrono::Utc::now);
    let (amz_date, date_stamp) = format_signing_date(date_time);

    let body_bytes = body.unwrap_or_default();
    let payload_hash = sha256_hex(&body_bytes);

    let mut headers_to_sign = headers.unwrap_or_default();
    headers_to_sign.insert("host".to_string(), parsed.host_str().unwrap_or("").to_string());
    headers_to_sign.insert(DATE_HEADER.to_string(), amz_date.clone());

    if let Some(session_token) = &credentials.session_token {
        headers_to_sign.insert(SECURITY_TOKEN_HEADER.to_string(), session_token.clone());
    }

    let (canonical_headers, signed_headers) = canonicalize_headers(&headers_to_sign);

    let pathname = parsed.path();
    let canonical_path_str = canonical_path(Some(pathname), double_encode_path);

    let canonical_request = format!(
        "{}\n{}\n\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_path_str,
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let credential_scope = format!(
        "{}/{}/{}/{}",
        date_stamp, region, service, TERMINATOR
    );

    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        ALGORITHM,
        amz_date,
        credential_scope,
        sha256_hex(canonical_request.as_bytes())
    );

    // Derive the signing key
    let key_prefix = format!("{}{}", KEY_PREFIX, credentials.secret_access_key);
    let key1 = hmac_sha256(key_prefix.as_bytes(), date_stamp.as_bytes());
    let key2 = hmac_sha256(&key1, region.as_bytes());
    let key3 = hmac_sha256(&key2, service.as_bytes());
    let signing_key = hmac_sha256(&key3, TERMINATOR.as_bytes());

    let signature = {
        let mut mac = HmacSha256::new_from_slice(&signing_key).expect("HMAC key size valid");
        mac.update(string_to_sign.as_bytes());
        format!("{:x}", mac.finalize())
    };

    headers_to_sign.insert(
        "Authorization".to_string(),
        format!(
            "{} Credential={}/{}, SignedHeaders={}, Signature={}",
            ALGORITHM, credentials.access_key_id, credential_scope, signed_headers, signature
        ),
    );

    Ok(headers_to_sign)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_uri_test() {
        assert_eq!(escape_uri("hello-world_123"), "hello-world_123");
        assert_eq!(escape_uri("hello world"), "hello%20world");
        assert_eq!(escape_uri("hello:world"), "hello%3Aworld");
        assert_eq!(escape_uri("!"), "%21");
    }

    #[test]
    fn canonical_path_test() {
        assert_eq!(canonical_path(None, false), "/");
        assert_eq!(canonical_path(Some("/v1/models"), false), "/v1/models");

        // Double encoding test
        let path = canonical_path(Some("/model/claude%3Av1"), true);
        assert_eq!(path, "/model/claude%253Av1");
    }
}
