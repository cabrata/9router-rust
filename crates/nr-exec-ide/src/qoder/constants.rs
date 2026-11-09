//! Qoder API constants ported from CLIProxyAPIPlus qoder-provider branch.
//!
//! Qoder runs two regional sites with parallel endpoint shapes:
//!   intl (qoder)       CN (qoder-cn)
//!   openapi.qoder.sh   openapi.qoder.com.cn   - device flow + userinfo + quota usage
//!   center.qoder.sh    gateway.qoder.com.cn   - token refresh (best-effort, 403 for device tokens)
//!   api3.qoder.sh      gateway.qoder.com.cn   - inference (chat) + model list, requires COSY signing
//!   qoder.com/device   qoder.com.cn/device    - browser landing page for device authorization

use indexmap::IndexMap;
use serde_json::json;
use std::sync::OnceLock;

pub const QODER_REGION_INTL: &str = "intl";
pub const QODER_REGION_CN: &str = "cn";

// Per-region base URLs
pub struct QoderRegionBases {
    pub chat: &'static str,
    pub chat_alt: &'static str,
    pub open_api: &'static str,
    pub center: &'static str,
    pub login: &'static str,
    pub website: &'static str,
}

pub const QODER_INTL_BASES: QoderRegionBases = QoderRegionBases {
    chat: "https://api3.qoder.sh",
    chat_alt: "https://api2.qoder.sh",
    open_api: "https://openapi.qoder.sh",
    center: "https://center.qoder.sh",
    login: "https://qoder.com/device/selectAccounts",
    website: "https://qoder.com",
};

pub const QODER_CN_BASES: QoderRegionBases = QoderRegionBases {
    chat: "https://gateway.qoder.com.cn",
    chat_alt: "https://gateway.qoder.com.cn",
    open_api: "https://openapi.qoder.com.cn",
    center: "https://gateway.qoder.com.cn",
    login: "https://qoder.com.cn/device/selectAccounts",
    website: "https://qoder.com.cn",
};

pub fn qoder_region_bases(region: &str) -> &'static QoderRegionBases {
    if region == QODER_REGION_CN {
        &QODER_CN_BASES
    } else {
        &QODER_INTL_BASES
    }
}

pub fn qoder_region_of(provider_id: &str) -> &'static str {
    if provider_id == "qoder-cn" {
        QODER_REGION_CN
    } else {
        QODER_REGION_INTL
    }
}

// Default (intl) URLs for backward compatibility
pub const QODER_OPENAPI_BASE: &str = "https://openapi.qoder.sh";
pub const QODER_CENTER_BASE: &str = "https://center.qoder.sh";
pub const QODER_CHAT_BASE: &str = "https://api3.qoder.sh";
pub const QODER_CHAT_BASE_ALT: &str = "https://api2.qoder.sh";
pub const QODER_LOGIN_URL: &str = "https://qoder.com/device/selectAccounts";

pub fn qoder_open_api_base(region: &str) -> String {
    qoder_region_bases(region).open_api.to_string()
}

pub fn qoder_device_token_url(region: &str) -> String {
    format!("{}/api/v1/deviceToken/poll", qoder_open_api_base(region))
}

pub fn qoder_user_info_url(region: &str) -> String {
    format!("{}/api/v1/userinfo", qoder_open_api_base(region))
}

pub fn qoder_quota_usage_url(region: &str) -> String {
    format!("{}/api/v2/quota/usage", qoder_open_api_base(region))
}

pub fn qoder_refresh_token_url(region: &str) -> String {
    format!(
        "{}/algo/api/v3/user/refresh_token",
        qoder_region_bases(region).center
    )
}

pub fn qoder_login_url(region: &str) -> String {
    qoder_region_bases(region).login.to_string()
}

pub fn qoder_website_url(region: &str) -> String {
    qoder_region_bases(region).website.to_string()
}

pub fn qoder_job_token_exchange_url(region: &str) -> String {
    format!(
        "{}/api/v1/jobToken/exchange",
        qoder_open_api_base(region)
    )
}

// Inference endpoints (under /algo on the chat host, all COSY-signed)
pub const QODER_CHAT_SIG_PATH: &str = "/api/v2/service/pro/sse/agent_chat_generation";

pub fn qoder_model_list_url(region: &str) -> String {
    format!(
        "{}/algo/api/v2/model/list",
        qoder_region_bases(region).chat
    )
}

pub const QODER_IMAGE_UPLOAD_SIG_PATH: &str = "/api/v2/image/upload";

// Drop remaining inlined binaries if the Qoder JSON body would still exceed this.
// 30MB+ payloads are what blow past Claude-Code's ~200k context on the wire.
pub const QODER_MAX_PAYLOAD_BYTES: usize = 6 * 1024 * 1024;
// If OSS upload fails, keep tiny data-URIs; anything larger is stubbed.
pub const QODER_INLINE_FALLBACK_MAX_BYTES: usize = 512 * 1024;

// Context-window tier selection (see shared/qoder/contextTier.js). The IDE exposes the
// model's context_config tiers (200K/400K/1M); we auto-escalate when the estimated prompt
// (+ headroom, tokenizer variance) no longer fits the current max_input_tokens.
pub const QODER_CONTEXT_TIER_HEADROOM: f64 = 0.15;
pub const QODER_CONTEXT_TIER_ENV: &str = "QODER_CONTEXT_TIER";
pub const QODER_CONTEXT_TIER_AUTO: &str = "auto";
pub const QODER_CONTEXT_TIER_MAX: &str = "max";
pub const QODER_CONTEXT_TIER_DEFAULT: &str = "default";

/// Region for a given token: job tokens (jt-...) must use api2.qoder.sh on intl.
pub fn qoder_inference_base(credentials: &serde_json::Value, region: &str) -> String {
    if region == QODER_REGION_CN {
        return qoder_region_bases(QODER_REGION_CN).chat.to_string();
    }

    let raw = credentials
        .get("apiKey")
        .and_then(|v| v.as_str())
        .or_else(|| credentials.get("accessToken").and_then(|v| v.as_str()));

    if let Some(token) = raw {
        if !token.starts_with("pt-")
            && (token.starts_with("jt-")
                || credentials
                    .get("accessToken")
                    .and_then(|v| v.as_str())
                    .map(|t| t.starts_with("jt-"))
                    .unwrap_or(false))
        {
            return QODER_CHAT_BASE_ALT.to_string();
        }
    }
    QODER_CHAT_BASE.to_string()
}

// COSY header constants
pub const QODER_IDE_VERSION: &str = "1.0.0";
pub const QODER_CLIENT_TYPE: &str = "5";
pub const QODER_DATA_POLICY: &str = "disagree";
pub const QODER_LOGIN_VERSION: &str = "v2";
pub const QODER_MACHINE_OS: &str = "x86_64_windows";
pub const QODER_MACHINE_TYPE: &str = "5";

// RSA public key for COSY encryption (extracted from Qoder IDE v0.9).
pub const QODER_RSA_PUBLIC_KEY: &str = r#"-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc
4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l
6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17
XcW+ML9FoCI6AOvOzwIDAQAB
-----END PUBLIC KEY-----"#;

// Canonical model identifiers
static QODER_MODEL_MAP: OnceLock<IndexMap<&'static str, &'static str>> = OnceLock::new();

pub fn qoder_model_map() -> &'static IndexMap<&'static str, &'static str> {
    QODER_MODEL_MAP.get_or_init(|| {
        let mut m = IndexMap::new();
        // Tier models
        m.insert("auto", "auto");
        m.insert("ultimate", "ultimate");
        m.insert("performance", "performance");
        m.insert("efficient", "efficient");
        m.insert("lite", "lite");
        // Frontier models
        m.insert("qmodel", "qmodel");
        m.insert("qfmodel", "qfmodel");
        m.insert("qmodel_latest", "qmodel_latest");
        m.insert("qmodel_38max", "qmodel_38max");
        m.insert("dmodel", "dmodel");
        m.insert("dfmodel", "dfmodel");
        m.insert("gmodel", "gmodel");
        m.insert("gfmodel", "gfmodel");
        m.insert("kmodel", "kmodel");
        m.insert("mmodel", "mmodel");
        // Frontier reasoning pair
        m.insert("smodel", "smodel");
        m.insert("cmodel", "cmodel");
        m
    })
}

// Static model_config blocks for models the per-account catalog does not reliably publish
pub fn qoder_static_model_config(key: &str) -> Option<serde_json::Value> {
    match key {
        "smodel" => Some(json!({
            "key": "smodel",
            "display_name": "Sonus",
            "model": "",
            "format": "openai",
            "source": "system",
            "url": "",
            "is_vl": true,
            "is_reasoning": true,
            "max_input_tokens": 180000,
        })),
        "cmodel" => Some(json!({
            "key": "cmodel",
            "display_name": "Cantus",
            "model": "",
            "format": "openai",
            "source": "system",
            "url": "",
            "is_vl": true,
            "is_reasoning": true,
            "max_input_tokens": 180000,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qoder_region_bases() {
        assert_eq!(qoder_region_bases("intl").chat, QODER_INTL_BASES.chat);
        assert_eq!(qoder_region_bases("cn").chat, QODER_CN_BASES.chat);
        assert_eq!(qoder_region_bases("unknown").chat, QODER_INTL_BASES.chat);
    }

    #[test]
    fn test_qoder_region_of() {
        assert_eq!(qoder_region_of("qoder-cn"), QODER_REGION_CN);
        assert_eq!(qoder_region_of("qoder"), QODER_REGION_INTL);
    }

    #[test]
    fn test_static_model_configs() {
        assert!(qoder_static_model_config("smodel").is_some());
        assert!(qoder_static_model_config("cmodel").is_some());
        assert!(qoder_static_model_config("unknown").is_none());
    }
}
