//! Port of open-sse/shared/qoder/cosy.js — Qoder COSY (RSA+AES+MD5) signing.
//!
//! Every signed request carries:
//!   - an AES-128-CBC payload of the user info, the AES key wrapped in RSA
//!   - an MD5 signature over `payload || cosyKey || timestamp || body || sigPath`
//!   - the body's MD5 hash + length so the server can validate integrity
//!   - 17 Cosy-* / X-* headers fingerprinting the client.
//!
//! On-the-wire header keys keep qodercli's casing (`Cosy-Machineid`, not `Cosy-MachineID`).

use aes::cipher::{BlockEncryptMut, KeyIvInit};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use md5::{Digest as Md5Digest, Md5};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha2::Sha256;

use super::constants::{
    QODER_CLIENT_TYPE, QODER_DATA_POLICY, QODER_IDE_VERSION, QODER_LOGIN_VERSION,
    QODER_MACHINE_OS, QODER_MACHINE_TYPE, QODER_RSA_PUBLIC_KEY,
};

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Random UUIDv4-shaped machine id.
pub fn generate_machine_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Deterministic machine id from the Qoder user id.
///
/// qodercli generates one machine id per install and reuses it for every request;
/// the server tracks per-client queue state by it. PAT connections have no stored
/// machine id, so a random uuid made every request look like a new device and
/// restarted at the back of the queue. Deriving it keeps it stable.
pub fn derive_machine_id(seed: &str) -> String {
    if seed.is_empty() {
        return generate_machine_id();
    }
    let mut hasher = Sha256::default();
    hasher.update(format!("qoder-machine\0{seed}").as_bytes());
    let h = hex::encode(hasher.finalize());
    // Formatted as a UUID so it is indistinguishable from a generated machine id.
    format!(
        "{}-{}-5{}-a{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

/// AES-128 key: first 16 chars of a fresh UUID's canonical string (hyphens included).
fn generate_aes_key() -> String {
    uuid::Uuid::new_v4().to_string()[..16].to_string()
}

fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(padding as u8).take(padding));
    out
}

fn aes_encrypt_cbc_base64(plaintext: &str, key: &str) -> Option<String> {
    let key_bytes = key.as_bytes();
    if key_bytes.len() != 16 {
        return None;
    }
    let iv = &key_bytes[..16];
    let mut cipher = Aes128CbcEnc::new(key_bytes.into(), iv.into());
    let padded = pkcs7_pad(plaintext.as_bytes(), 16);
    let mut buf = padded;
    // Manual pkcs7 + NoPadding (JS sets setAutoPadding(false)).
    for chunk in buf.chunks_mut(16) {
        cipher.encrypt_block_mut(aes::Block::from_mut_slice(chunk));
    }
    Some(B64.encode(&buf))
}

fn rsa_encrypt_base64(data: &str) -> Option<String> {
    let pub_key = RsaPublicKey::from_public_key_pem(QODER_RSA_PUBLIC_KEY).ok()?;
    // JS uses RSA_PKCS1_PADDING.
    let encrypted = pub_key.encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, data.as_bytes()).ok()?;
    Some(B64.encode(encrypted))
}

fn encrypt_user_info(user_info: &serde_json::Value) -> Option<(String, String)> {
    let aes_key = generate_aes_key();
    let plaintext = user_info.to_string();
    let info_b64 = aes_encrypt_cbc_base64(&plaintext, &aes_key)?;
    let cosy_key_b64 = rsa_encrypt_base64(&aes_key)?;
    Some((cosy_key_b64, info_b64))
}

fn md5_hex(input: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(input);
    hex::encode(h.finalize())
}

/// Strip the leading `/algo` prefix from the request path (qodercli convention).
pub fn compute_sig_path(request_url: &str) -> String {
    let pathname = match url::Url::parse(request_url) {
        Ok(u) => u.path().to_string(),
        Err(_) => return String::new(),
    };
    pathname.strip_prefix("/algo").map(String::from).unwrap_or(pathname)
}

/// Credentials needed to sign a Qoder request.
pub struct CosyCreds<'a> {
    pub user_id: &'a str,
    pub auth_token: &'a str,
    pub name: &'a str,
    pub email: &'a str,
    pub machine_id: Option<&'a str>,
}

/// Build the full Cosy-* header set for one Qoder request.
///
/// `body` must be the exact bytes that will be sent; pass `&[]` for GET.
pub fn build_cosy_headers(
    body: &[u8],
    request_url: &str,
    creds: &CosyCreds<'_>,
) -> Option<Vec<(String, String)>> {
    if creds.user_id.is_empty() || creds.auth_token.is_empty() {
        return None;
    }

    let (cosy_key, info) = encrypt_user_info(&serde_json::json!({
        "uid": creds.user_id,
        "security_oauth_token": creds.auth_token,
        "name": creds.name,
        "aid": "",
        "email": creds.email,
    }))?;

    let timestamp = (chrono::Utc::now().timestamp()).to_string();
    let request_id = uuid::Uuid::new_v4().to_string();

    let payload_json = serde_json::json!({
        "version": "v1",
        "requestId": request_id,
        "info": info,
        "cosyVersion": QODER_IDE_VERSION,
        "ideVersion": "",
    })
    .to_string();
    let payload_b64 = B64.encode(payload_json.as_bytes());

    let sig_path = compute_sig_path(request_url);
    // Signed over latin1 bytes: build the byte buffer directly.
    let mut sig_input = Vec::new();
    sig_input.extend_from_slice(payload_b64.as_bytes());
    sig_input.push(b'\n');
    sig_input.extend_from_slice(cosy_key.as_bytes());
    sig_input.push(b'\n');
    sig_input.extend_from_slice(timestamp.as_bytes());
    sig_input.push(b'\n');
    sig_input.extend_from_slice(body);
    sig_input.push(b'\n');
    sig_input.extend_from_slice(sig_path.as_bytes());
    let sig = md5_hex(&sig_input);

    let machine_id = creds
        .machine_id
        .filter(|m| !m.is_empty())
        .map(String::from)
        .unwrap_or_else(generate_machine_id);
    let body_hash = md5_hex(body);
    let body_length = body.len().to_string();

    let mut h: Vec<(String, String)> = Vec::with_capacity(20);
    let mut push = |k: &str, v: String| h.push((k.to_string(), v));
    push("Authorization", format!("Bearer COSY.{payload_b64}.{sig}"));
    push("Cosy-Key", cosy_key);
    push("Cosy-User", creds.user_id.to_string());
    push("Cosy-Date", timestamp);
    push("Cosy-Version", QODER_IDE_VERSION.to_string());
    push("Cosy-Machineid", machine_id.clone());
    push("Cosy-Machinetoken", machine_id);
    push("Cosy-Machinetype", QODER_MACHINE_TYPE.to_string());
    push("Cosy-Machineos", QODER_MACHINE_OS.to_string());
    push("Cosy-Clienttype", QODER_CLIENT_TYPE.to_string());
    push("Cosy-Clientip", "127.0.0.1".to_string());
    push("Cosy-Bodyhash", body_hash);
    push("Cosy-Bodylength", body_length);
    push("Cosy-Sigpath", sig_path);
    push("Cosy-Data-Policy", QODER_DATA_POLICY.to_string());
    push("Cosy-Organization-Id", String::new());
    push("Cosy-Organization-Tags", String::new());
    push("Login-Version", QODER_LOGIN_VERSION.to_string());
    push("X-Request-Id", uuid::Uuid::new_v4().to_string());

    Some(h)
}

/// Port of shared/qoder/encoding.js — the WAF-bypass body encoding.
///
/// 1. base64-encode the plaintext.
/// 2. Rearrange: split into thirds, reorder as [tail][mid][head].
/// 3. Substitute each character via a custom alphabet mapping.
///
/// The encoded body must be sent with `&Encode=1` so the server reverses it.
const QODER_STD_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const QODER_CUSTOM_ALPHABET: &[u8; 64] =
    b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";

fn s2c_table() -> [i16; 128] {
    let mut table = [-1i16; 128];
    for i in 0..64 {
        table[QODER_STD_ALPHABET[i] as usize] = QODER_CUSTOM_ALPHABET[i] as i16;
    }
    table[b'=' as usize] = b'$' as i16;
    table
}

pub fn qoder_encode_body(plaintext: &[u8]) -> String {
    let std_b64 = B64.encode(plaintext);
    let n = std_b64.len();
    let a = n / 3;
    // [tail][mid][head]
    let rearranged: Vec<u8> = std_b64.as_bytes()[n - a..]
        .iter()
        .chain(std_b64.as_bytes()[a..n - a].iter())
        .chain(std_b64.as_bytes()[..a].iter())
        .copied()
        .collect();

    let table = s2c_table();
    let out: Vec<u8> = rearranged
        .iter()
        .map(|&c| {
            if (c as usize) < 128 && table[c as usize] >= 0 {
                table[c as usize] as u8
            } else {
                c
            }
        })
        .collect();
    // JS returns a latin1 string; keep it byte-for-byte.
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_machine_id_stable() {
        let a = derive_machine_id("user-123");
        let b = derive_machine_id("user-123");
        assert_eq!(a, b);
        assert_ne!(a, derive_machine_id("user-456"));
        // UUID-shaped: 8-4-4-4-12
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert!(parts[2].starts_with('5'));
        assert!(parts[3].starts_with('a'));
        assert_eq!(parts[4].len(), 12);
    }

    #[test]
    fn test_compute_sig_path() {
        assert_eq!(
            compute_sig_path("https://api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation"),
            "/api/v2/service/pro/sse/agent_chat_generation"
        );
        assert_eq!(
            compute_sig_path("https://api3.qoder.sh/api/v1/userinfo"),
            "/api/v1/userinfo"
        );
        assert_eq!(compute_sig_path("not a url"), "");
    }

    #[test]
    fn test_encoding_roundtrip_shape() {
        let encoded = qoder_encode_body(b"{\"hello\":\"world\"}");
        // Length is preserved (base64 length), padding maps '=' to '$'.
        let b64_len = B64.encode(b"{\"hello\":\"world\"}").len();
        assert_eq!(encoded.chars().count(), b64_len);
        // Nothing from the standard alphabet survives except the mapped chars.
        assert!(!encoded.contains('+'));
    }

    #[test]
    fn test_build_cosy_headers_requires_credentials() {
        let empty = CosyCreds { user_id: "", auth_token: "", name: "", email: "", machine_id: None };
        assert!(build_cosy_headers(b"{}", "https://api3.qoder.sh/api/v1/userinfo", &empty).is_none());

        let ok = CosyCreds {
            user_id: "u1",
            auth_token: "dt-abc",
            name: "n",
            email: "e@x.y",
            machine_id: Some("11111111-2222-3333-4444-555555555555"),
        };
        let headers = build_cosy_headers(b"{}", "https://api3.qoder.sh/algo/api/v2/model/list", &ok)
            .expect("cosy headers");
        let map: std::collections::HashMap<_, _> = headers.into_iter().collect();
        assert!(map["Authorization"].starts_with("Bearer COSY."));
        assert_eq!(map["Cosy-User"], "u1");
        assert_eq!(map["Cosy-Sigpath"], "/api/v2/model/list");
        assert_eq!(map["Cosy-Bodylength"], "2");
        assert_eq!(map["Cosy-Organization-Id"], "");
        assert_eq!(map["Cosy-Machineid"], "11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn test_aes_key_length_matches_js() {
        // The key is the first 16 chars of a UUID string, hyphens included.
        let key = generate_aes_key();
        assert_eq!(key.len(), 16);
    }
}
