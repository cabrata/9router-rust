//! Provider credentials: the JS `credentials` object passed through
//! translators, executors and token refresh.
//!
//! Kept as a dynamic JSON object on purpose: the JS code reads and writes
//! dozens of ad-hoc fields (`accessToken`, `providerSpecificData.baseUrl`,
//! `_clientSessionId`, `rawHeaders`, …), and keeping the same keys keeps the
//! DB rows, cloud sync payloads and executors byte-compatible with upstream.

use serde::{Deserialize, Serialize};

use crate::{Json, JsonMap};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Credentials(pub JsonMap);

impl Credentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Non-objects become empty credentials.
    pub fn from_value(v: Json) -> Self {
        match v {
            Json::Object(m) => Self(m),
            _ => Self::default(),
        }
    }

    pub fn into_value(self) -> Json {
        Json::Object(self.0)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.0.get(key)
    }

    /// Non-empty string field (JS truthy string).
    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Json::as_str).filter(|s| !s.is_empty())
    }

    pub fn set(&mut self, key: &str, value: impl Into<Json>) {
        self.0.insert(key.to_string(), value.into());
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        self.0.shift_remove(key)
    }

    /// `Object.assign(credentials, other)`.
    pub fn merge(&mut self, other: &JsonMap) {
        for (k, v) in other {
            self.0.insert(k.clone(), v.clone());
        }
    }

    pub fn access_token(&self) -> Option<&str> {
        self.str("accessToken")
    }

    pub fn api_key(&self) -> Option<&str> {
        self.str("apiKey")
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.str("refreshToken")
    }

    /// `credentials.accessToken || credentials.apiKey`.
    pub fn bearer(&self) -> Option<&str> {
        self.access_token().or_else(|| self.api_key())
    }

    pub fn connection_id(&self) -> Option<&str> {
        self.str("connectionId")
    }

    /// `credentials.providerSpecificData`.
    pub fn psd(&self) -> Option<&JsonMap> {
        self.0.get("providerSpecificData").and_then(Json::as_object)
    }

    /// `credentials.providerSpecificData?.[key]` as a non-empty string.
    pub fn psd_str(&self, key: &str) -> Option<&str> {
        self.psd()?.get(key).and_then(Json::as_str).filter(|s| !s.is_empty())
    }

    /// `providerSpecificData`, created when missing or not an object.
    pub fn psd_mut(&mut self) -> &mut JsonMap {
        let slot = self.0.entry("providerSpecificData").or_insert_with(|| Json::Object(JsonMap::new()));
        if !slot.is_object() {
            *slot = Json::Object(JsonMap::new());
        }
        slot.as_object_mut().expect("just ensured object")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accessors_follow_js_truthiness() {
        let mut c = Credentials::from_value(json!({"accessToken": "", "apiKey": "k", "providerSpecificData": {"baseUrl": "u"}}));
        assert_eq!(c.bearer(), Some("k"));
        assert_eq!(c.psd_str("baseUrl"), Some("u"));
        c.psd_mut().insert("x".into(), json!(1));
        c.merge(json!({"accessToken": "t"}).as_object().unwrap());
        assert_eq!(c.bearer(), Some("t"));
        assert_eq!(c.psd().unwrap()["x"], json!(1));
    }
}
