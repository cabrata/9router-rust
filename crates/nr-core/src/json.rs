//! Small helpers for porting JS object code onto `serde_json::Value`.

use crate::Json;

/// JS truthiness: `null`, `false`, `0`, `NaN` and `""` are falsy; every object
/// and array (even empty) is truthy.
pub fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Json::String(s) => !s.is_empty(),
        Json::Array(_) | Json::Object(_) => true,
    }
}

/// `obj?.[key]` as a non-empty string — the usual JS `x?.y || fallback` read.
pub fn str_of<'a>(v: &'a Json, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Json::as_str).filter(|s| !s.is_empty())
}

/// `obj?.a?.b?.c` for object keys.
pub fn get_path<'a>(v: &'a Json, path: &[&str]) -> Option<&'a Json> {
    path.iter().try_fold(v, |cur, key| cur.get(*key))
}

/// `Date.now()`.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `new Date().toISOString()`, e.g. `2026-11-08T12:34:56.789Z`.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Milliseconds since epoch of an ISO-8601 string or a numeric timestamp
/// (`new Date(x).getTime()`); `None` when unparseable.
pub fn parse_time_ms(v: &Json) -> Option<i64> {
    match v {
        Json::Number(n) => n.as_f64().map(|f| f as i64),
        Json::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| d.timestamp_millis())
            .ok()
            .or_else(|| s.parse::<f64>().ok().map(|f| f as i64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_matches_js() {
        for v in [json!(null), json!(false), json!(0), json!(0.0), json!("")] {
            assert!(!truthy(&v), "{v} should be falsy");
        }
        for v in [json!(true), json!(1), json!("x"), json!([]), json!({})] {
            assert!(truthy(&v), "{v} should be truthy");
        }
    }

    #[test]
    fn reads() {
        let v = json!({"a": {"b": "c"}, "e": ""});
        assert_eq!(get_path(&v, &["a", "b"]), Some(&json!("c")));
        assert_eq!(str_of(&v, "e"), None);
        assert_eq!(parse_time_ms(&json!("1970-01-01T00:00:01.000Z")), Some(1000));
        assert!(now_iso().ends_with('Z'));
    }
}
