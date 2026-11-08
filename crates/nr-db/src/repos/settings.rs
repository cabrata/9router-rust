use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};

/// Get all settings, merged with defaults
pub fn get_settings(db: &Db) -> Result<Value> {
    let raw = read_raw(db)?;
    Ok(merge_with_defaults(&raw))
}

/// Update settings (merge with existing)
pub fn update_settings(db: &Db, updates: Value) -> Result<Value> {
    let mut current = read_raw(db)?;
    if let Some(obj) = current.as_object_mut() {
        if let Some(updates_obj) = updates.as_object() {
            for (k, v) in updates_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }

    db.execute_with_params(
        "INSERT INTO settings(id, data) VALUES(1, ?) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        [stringify_json(&current)],
    )?;

    Ok(merge_with_defaults(&current))
}

/// Check if cloud is enabled
pub fn is_cloud_enabled(db: &Db) -> Result<bool> {
    let settings = get_settings(db)?;
    Ok(settings
        .get("cloudEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

/// Get the cloud URL
pub fn get_cloud_url(db: &Db) -> Result<String> {
    let settings = get_settings(db)?;
    Ok(settings
        .get("cloudUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/// Export raw settings (without defaults)
pub fn export_settings(db: &Db) -> Result<Value> {
    read_raw(db)
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn read_raw(db: &Db) -> Result<Value> {
    let result = db.query_row(
        "SELECT data FROM settings WHERE id = 1",
        &[],
        |row| {
            let data: String = row.get(0)?;
            Ok(data)
        },
    )?;

    match result {
        Some(data) => Ok(parse_json(Some(data), json!({}))),
        None => Ok(json!({})),
    }
}

fn merge_with_defaults(raw: &Value) -> Value {
    let defaults = default_settings();
    let mut merged = defaults;

    if let Some(raw_obj) = raw.as_object() {
        if let Some(merged_obj) = merged.as_object_mut() {
            for (k, v) in raw_obj {
                merged_obj.insert(k.clone(), v.clone());
            }
        }
    }

    merged
}

fn default_settings() -> Value {
    json!({
        "cloudEnabled": false,
        "tunnelEnabled": false,
        "tunnelUrl": "",
        "tunnelProvider": "cloudflare",
        "tailscaleEnabled": false,
        "tailscaleUrl": "",
        "stickyRoundRobinLimit": 3,
        "providerStrategies": {},
        "quotaVisibility": {},
        "comboStrategy": "fallback",
        "comboStickyRoundRobinLimit": 1,
        "comboStrategies": {},
        "capacityAdapter": {
            "vision": { "enabled": true, "roundRobin": false, "models": [] },
            "pdf": { "enabled": false, "roundRobin": false, "models": [] },
            "audioInput": { "enabled": true, "roundRobin": false, "models": [] },
            "videoInput": { "enabled": false, "roundRobin": false, "models": [] },
        },
        "requireLogin": true,
        "requireApiKey": true,
        "tunnelDashboardAccess": true,
        "authMode": "password",
        "ssoType": "oidc",
        "oidcIssuerUrl": "",
        "oidcClientId": "",
        "oidcClientSecret": "",
        "oidcScopes": "openid profile email",
        "oidcLoginLabel": "Sign in with OIDC",
        "samlEntryPoint": "",
        "samlIssuer": "urn:9router:sp",
        "samlCert": "",
        "samlLoginLabel": "Sign in with SAML SSO",
        "samlAttributeEmail": "email",
        "samlAttributeName": "name",
        "enableObservability": false,
        "observabilityMaxRecords": 1000,
        "observabilityBatchSize": 20,
        "observabilityFlushIntervalMs": 5000,
        "observabilityMaxJsonSize": 5,
        "outboundProxyEnabled": false,
        "outboundProxyUrl": "",
        "outboundNoProxy": "",
        "mitmRouterBaseUrl": "http://localhost:20128",
        "dnsToolEnabled": {},
        "rtkEnabled": true,
        "headroomEnabled": false,
        "headroomUrl": "http://localhost:8787",
        "headroomCompressUserMessages": false,
        "headroomTimeoutMs": 3000,
        "cavemanEnabled": false,
        "cavemanLevel": "full",
        "ponytailEnabled": false,
        "ponytailLevel": "full",
        "customPromptEnabled": false,
        "customPrompt": "",
        "pxpipeEnabled": false,
        "pxpipeAutoInstall": true,
        "pxpipeMinChars": 25000,
        "pxpipeTimeoutMs": 15000,
        "providerOverrides": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_get_settings_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let settings = get_settings(&db).unwrap();
        assert_eq!(settings.get("requireLogin").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn test_update_settings() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let updates = json!({"customPromptEnabled": true});
        let result = update_settings(&db, updates).unwrap();
        assert_eq!(
            result.get("customPromptEnabled").and_then(|v| v.as_bool()),
            Some(true)
        );
    }
}
