//! Display helpers for the interactive menus. Port of
//! `cli/src/cli/utils/{format,display,comboModelId}.js`.

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use serde_json::Value;

/// Truncate with an ellipsis (`truncate` in format.js).
pub fn truncate(text: &str, max_len: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_len || max_len < 3 {
        return text.to_string();
    }
    chars[..max_len - 3].iter().collect::<String>() + "..."
}

/// Mask a key to its first and last four characters (`maskKey`).
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 8 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(chars.len() - 8))
}

/// Local `YYYY-MM-DD HH:MM:SS`, or `Invalid Date` (`formatDate`).
pub fn format_date(value: &Value) -> String {
    match parse_timestamp(value) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "Invalid Date".to_string(),
    }
}

/// `12 minutes ago` style relative time (`getRelativeTime`).
pub fn relative_time(value: &Value) -> String {
    let Some(dt) = parse_timestamp(value) else {
        return "Invalid Date".to_string();
    };
    relative_to(dt, Utc::now())
}

/// Relative time at a fixed "now" — the testable core of `relative_time`.
pub fn relative_to(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let diff_ms = (now - then).num_milliseconds();
    let diff_sec = diff_ms.div_euclid(1000);
    let diff_min = diff_sec.div_euclid(60);
    let diff_hour = diff_min.div_euclid(60);
    let diff_day = diff_hour.div_euclid(24);
    let diff_month = diff_day.div_euclid(30);
    let diff_year = diff_day.div_euclid(365);

    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n > 1 { "s" } else { "" });

    if diff_sec < 60 {
        "just now".to_string()
    } else if diff_min < 60 {
        plural(diff_min, "minute")
    } else if diff_hour < 24 {
        plural(diff_hour, "hour")
    } else if diff_day < 30 {
        plural(diff_day, "day")
    } else if diff_month < 12 {
        plural(diff_month, "month")
    } else {
        plural(diff_year, "year")
    }
}

/// Accept the timestamp shapes the JS `new Date(x)` accepts: RFC 3339
/// strings, `YYYY-MM-DD HH:MM:SS`, millisecond/second epochs, or ISO dates.
fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(n) = value.as_i64() {
        // JS treats a bare number as milliseconds.
        let (secs, nanos) = (n.div_euclid(1000), (n.rem_euclid(1000) * 1_000_000) as u32);
        return Utc.timestamp_opt(secs, nanos).single();
    }
    let s = value.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // `YYYY-MM-DD HH:MM:SS` in local time (what the JS Date constructor does
    // for an unsuffixed datetime string).
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Local.from_local_datetime(&naive).single().map(|d| d.with_timezone(&Utc));
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0)?;
        return Local.from_local_datetime(&naive).single().map(|d| d.with_timezone(&Utc));
    }
    None
}

/// Human year/month/day, used in menu headers for "member since" style lines.
pub fn date_only(value: &Value) -> String {
    match parse_timestamp(value) {
        Some(dt) => {
            let local = dt.with_timezone(&Local);
            format!("{:04}-{:02}-{:02}", local.year(), local.month(), local.day())
        }
        None => "Invalid Date".to_string(),
    }
}

/// `formatModel` from combos.js — string, object id/name, or provider/model.
pub fn format_model(model: &Value) -> String {
    match model {
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            for key in ["id", "name"] {
                if let Some(s) = model.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    return s.to_string();
                }
            }
            match (
                model.get("provider").and_then(Value::as_str),
                model.get("model").and_then(Value::as_str),
            ) {
                (Some(p), Some(m)) => format!("{p}/{m}"),
                _ => model.to_string(),
            }
        }
        other => other.to_string(),
    }
}

/// The id a combo stores for a model (`comboModelId`).
pub fn combo_model_id(model: &Value) -> Option<String> {
    if let Some(s) = model.as_str() {
        return Some(s.to_string());
    }
    if let Some(full) = model.get("fullModel").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return Some(full.to_string());
    }
    match (
        model.get("provider").and_then(Value::as_str),
        model.get("model").and_then(Value::as_str),
    ) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        _ => None,
    }
}

/// The `A → B → C` chain shown for a combo.
pub fn models_chain(models: &[Value]) -> String {
    models.iter().map(format_model).collect::<Vec<_>>().join(" → ")
}

/// `name (${maskKey(key)})` — the label used in the API-keys list.
pub fn api_key_label(key: &Value) -> String {
    let name = key.get("name").and_then(Value::as_str).unwrap_or("Unnamed");
    let masked = mask_key(key.get("key").and_then(Value::as_str).unwrap_or(""));
    format!("{name} ({masked})")
}

/// `✓` / `✗` / `?` for a connection's `testStatus`.
pub fn status_glyph(test_status: Option<&str>) -> &'static str {
    match test_status {
        Some("active") => "✓",
        Some("error") => "✗",
        _ => "?",
    }
}

/// Display name for a connection (`name || email || displayName || "Unnamed"`).
pub fn connection_name(conn: &Value) -> String {
    for key in ["name", "email", "displayName"] {
        if let Some(s) = conn.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return s.to_string();
        }
    }
    "Unnamed".to_string()
}

/// `name (✓)` label for a provider connection row.
pub fn connection_label(conn: &Value) -> String {
    format!(
        "{} ({})",
        connection_name(conn),
        status_glyph(conn.get("testStatus").and_then(Value::as_str))
    )
}

/// `name: models…` row for a combo, truncated to `max_len` (`formatComboLabel`).
pub fn combo_label(combo: &Value, max_len: usize) -> String {
    let name = combo.get("name").and_then(Value::as_str).unwrap_or("Unnamed");
    let models = combo.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
    truncate(&format!("{name}: {}", models_chain(&models)), max_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_adds_ellipsis_only_when_needed() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("abcdefghijkl", 10), "abcdefg...");
        // Multibyte input must not split a char boundary.
        assert_eq!(truncate("ключключключ", 5), "клю...");
    }

    #[test]
    fn mask_key_keeps_first_and_last_four() {
        assert_eq!(mask_key(""), "***");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key("12345678"), "1234****5678");
        assert_eq!(mask_key("sk-abcdef1234567890"), "sk-a************7890");
    }

    #[test]
    fn format_date_accepts_rfc3339_and_garbage() {
        let formatted = format_date(&json!("2024-05-01T10:20:30Z"));
        assert!(formatted.starts_with("2024-05-01 "), "got {formatted}");
        assert_eq!(format_date(&json!("not a date")), "Invalid Date");
        assert_eq!(format_date(&json!(null)), "Invalid Date");
    }

    #[test]
    fn relative_time_buckets_match_the_js() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(relative_to(ago(5), now), "just now");
        assert_eq!(relative_to(ago(60), now), "1 minute ago");
        assert_eq!(relative_to(ago(120), now), "2 minutes ago");
        assert_eq!(relative_to(ago(3600), now), "1 hour ago");
        assert_eq!(relative_to(ago(7200), now), "2 hours ago");
        assert_eq!(relative_to(ago(86_400), now), "1 day ago");
        assert_eq!(relative_to(ago(86_400 * 2), now), "2 days ago");
        assert_eq!(relative_to(ago(86_400 * 30), now), "1 month ago");
        assert_eq!(relative_to(ago(86_400 * 31), now), "1 month ago");
        assert_eq!(relative_to(ago(86_400 * 365), now), "1 year ago");
        assert_eq!(relative_to(ago(86_400 * 400), now), "1 year ago");
        assert_eq!(relative_to(ago(86_400 * 730), now), "2 years ago");
    }

    #[test]
    fn format_model_handles_strings_objects_and_fallbacks() {
        assert_eq!(format_model(&json!("ag/model")), "ag/model");
        assert_eq!(format_model(&json!({"id": "m1"})), "m1");
        assert_eq!(format_model(&json!({"name": "Nice"})), "Nice");
        assert_eq!(format_model(&json!({"provider": "cc", "model": "opus"})), "cc/opus");
    }

    #[test]
    fn combo_model_id_prefers_string_then_full_model_then_pair() {
        assert_eq!(combo_model_id(&json!("cc/opus")).as_deref(), Some("cc/opus"));
        assert_eq!(
            combo_model_id(&json!({"fullModel": "cc/x", "provider": "cc", "model": "y"})).as_deref(),
            Some("cc/x")
        );
        assert_eq!(
            combo_model_id(&json!({"provider": "cc", "model": "y"})).as_deref(),
            Some("cc/y")
        );
        assert_eq!(combo_model_id(&json!({"name": "n"})), None);
    }

    #[test]
    fn connection_labels_fall_back_through_name_fields() {
        assert_eq!(connection_name(&json!({"name": "Work"})), "Work");
        assert_eq!(connection_name(&json!({"email": "a@b.c"})), "a@b.c");
        assert_eq!(connection_name(&json!({"displayName": "D"})), "D");
        assert_eq!(connection_name(&json!({})), "Unnamed");
        assert_eq!(connection_label(&json!({"name": "W", "testStatus": "active"})), "W (✓)");
        assert_eq!(connection_label(&json!({"name": "W", "testStatus": "error"})), "W (✗)");
        assert_eq!(connection_label(&json!({"name": "W"})), "W (?)");
    }

    #[test]
    fn combo_label_truncates_the_chain() {
        let combo = json!({"name": "fast", "models": ["a/x", "b/y"]});
        assert_eq!(combo_label(&combo, 40), "fast: a/x → b/y");
        let long = json!({"name": "fast", "models": ["aaaaaaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbb"]});
        assert!(combo_label(&long, 20).ends_with("..."), "{}", combo_label(&long, 20));
    }

    #[test]
    fn api_key_label_masks_the_secret() {
        assert_eq!(
            api_key_label(&json!({"name": "laptop", "key": "sk-1234567890abcd"})),
            "laptop (sk-1**********abcd)"
        );
    }
}
