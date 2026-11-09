//! Port of `open-sse/services/minimaxCodeUsage.js`: MiniMax Code account
//! usage — credits balance, plan tier and M Plan rate windows, read the way
//! MiniMax Code reads them (its signed account API). Protocol from
//! @magpie-community/opencode-minimax-auth 0.1.1.
//!
//! Signing (account API only): the request carries mcode query params, a `yy`
//! md5 over path+query, body and now, and `x-signature` md5(ts + secret
//! [+ body]).
//!
//! The `ponytail:` ceiling: the signed account fetch needs `proxy_fetch`
//! (blocking-free, async). The pure parsing/signing/building functions are
//! fully offline-testable; the network-facing `get_minimax_code_usage` is
//! implemented but integration is left to the nr-server usage dispatcher.

use std::time::{SystemTime, UNIX_EPOCH};

use md5::Md5;
use nr_core::http::{FetchRequest, HeaderList, proxy_fetch};
use nr_core::Json;
use serde_json::json;
use sha2::Digest;

const SIGN_SECRET: &str = "I*7Cf%WZ#S&%1RlZJ&C2";

/// `SITES`: CN uses agent.minimaxi.com; global agent.minimax.io.
pub struct MinimaxSite {
    pub agent: &'static str,
    pub platform: &'static str,
    pub lang: &'static str,
}

pub const SITES: [(&str, MinimaxSite); 2] = [
    (
        "minimax-code",
        MinimaxSite { agent: "https://agent.minimaxi.com", platform: "https://www.minimaxi.com", lang: "zh" },
    ),
    (
        "minimax-code-global",
        MinimaxSite { agent: "https://agent.minimax.io", platform: "https://platform.minimax.io", lang: "en" },
    ),
];

pub fn site_for(provider: &str) -> Option<&'static MinimaxSite> {
    SITES.iter().find(|(id, _)| *id == provider).map(|(_, s)| s)
}

fn md5_hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `signedAccountRequest(site, path, opts)`: build the signed request the
/// mcode client sends. Returns `(url, headers, body)`.
pub fn signed_account_request(
    site: &MinimaxSite,
    path: &str,
    access: &str,
    user_id: &str,
    body: Option<&Json>,
    now: u64,
    tz_offset_sec: i64,
) -> (String, HeaderList, Option<String>) {
    let mut url = url::Url::parse(&format!("{}{}", site.agent.trim_end_matches('/'), path)).expect("site agent + path is a url");
    let mut pairs: Vec<(&str, String)> = vec![
        ("device_platform", "mcode".into()),
        ("biz_id", "3".into()),
        ("app_id", "3001".into()),
        ("version_code", "22201".into()),
        ("unix", now.to_string()),
        ("timezone_offset", tz_offset_sec.to_string()),
        ("sys_language", site.lang.into()),
        ("lang", site.lang.into()),
        ("device_id", "0".into()),
        ("os_name", std::env::consts::OS.into()),
        ("browser_name", "mcode".into()),
        ("user_id", if user_id.trim().is_empty() { "0".into() } else { user_id.trim().into() }),
        ("client", "mcode".into()),
    ];
    let mut q = url.query_pairs_mut();
    for (k, v) in pairs.drain(..) {
        q.append_pair(k, &v);
    }
    drop(q);

    let at = format!("{}{}", url.path(), url.query().map(|q| format!("?{q}")).unwrap_or_default());
    let ts = now / 1000;
    let json_body = body.map(Json::to_string);
    let yy = match &json_body {
        None => {
            let tail = format!("{}ooui", md5_hex(&now.to_string()));
            md5_hex(&format!("{}_{{}}{tail}", urlencoding::encode(&at)))
        }
        Some(j) => {
            let tail = format!("{}ooui", md5_hex(&now.to_string()));
            md5_hex(&format!("{}_{j}{tail}", urlencoding::encode(&at)))
        }
    };
    let x_signature = match &json_body {
        None => md5_hex(&format!("{ts}{SIGN_SECRET}")),
        Some(j) => md5_hex(&format!("{ts}{SIGN_SECRET}{j}")),
    };

    let mut headers = HeaderList::new();
    headers.insert("Accept".into(), "application/json".into());
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("User-Agent".into(), "MiniMaxCode".into());
    headers.insert("Authorization".into(), format!("Bearer {access}"));
    headers.insert("yy".into(), yy);
    headers.insert("x-timestamp".into(), ts.to_string());
    headers.insert("x-signature".into(), x_signature);
    (url.to_string(), headers, json_body)
}

/// `parseMembership(e)`: read a membership answer (or workspace entry).
pub fn parse_membership(e: &Json) -> Json {
    let d = e.get("data");
    let pick = |k: &str| e.get(k).or_else(|| d.and_then(|d| d.get(k)));
    let s = |k: &str| pick(k).and_then(Json::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let has = pick("has_token_plan").and_then(Json::as_bool);
    let ends = pick("token_plan_expires_at").and_then(Json::as_u64);
    let sum = e.get("op_credit_summary").or_else(|| d.and_then(|d| d.get("op_credit_summary")));
    let bal = sum
        .and_then(|s| s.get("total_remaining_amount"))
        .and_then(Json::as_str)
        .map(str::to_string)
        .or_else(|| match pick("opcredit_balance") {
            Some(Json::String(v)) if !v.trim().is_empty() => Some(v.trim().to_string()),
            Some(Json::Number(n)) if n.as_f64().is_some() => Some(n.to_string()),
            _ => None,
        });

    let mut out = serde_json::Map::new();
    if let Some(h) = has {
        out.insert("hasTokenPlan".into(), json!(h));
    }
    if let Some(g) = s("op_group_id") {
        out.insert("opGroupId".into(), json!(g));
    }
    if let Some(t) = s("token_plan_tier") {
        out.insert("tier".into(), json!(t));
    }
    if ends.is_some_and(|e| e > 0) {
        out.insert("expiresAt".into(), json!(ends.unwrap_or(0)));
    }
    if let Some(b) = bal {
        out.insert("balance".into(), json!(b));
    }
    Json::Object(out)
}

/// `parsePlanWindows(body)`: parse `coding_plan/remains` into windows.
pub fn parse_plan_windows(body: &Json) -> Option<Vec<Json>> {
    let br = body.get("base_resp");
    if let Some(br) = br {
        if let Some(code) = br.get("status_code").and_then(Json::as_u64) {
            if code != 0 {
                return None;
            }
        }
    }
    if br.is_none() && !body.get("model_remains").and_then(Json::as_array).is_some() {
        return None;
    }
    let ms = |n: u64| if n < 1_000_000_000_000 { n * 1000 } else { n };
    let z = |v: Option<u64>| v == Some(0);
    let mut out = Vec::new();
    let list = body.get("model_remains").and_then(Json::as_array)?;
    for k in list {
        let name = k.get("model_name").and_then(Json::as_str).map(str::trim).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let interval_status = k.get("current_interval_status").and_then(Json::as_u64);
        let weekly_status = k.get("current_weekly_status").and_then(Json::as_u64);
        if interval_status == Some(3)
            && weekly_status == Some(3)
            && z(k.get("current_interval_total_count").and_then(Json::as_u64))
            && z(k.get("current_weekly_total_count").and_then(Json::as_u64))
        {
            continue;
        }
        let general = name.eq_ignore_ascii_case("general");
        let mut windows = Vec::new();
        for (left_key, status_key, start_key, end_key, week) in [
            ("current_interval_remaining_percent", "current_interval_status", "start_time", "end_time", false),
            ("current_weekly_remaining_percent", "current_weekly_status", "weekly_start_time", "weekly_end_time", true),
        ] {
            let left = k.get(left_key).and_then(Json::as_u64);
            let status = k.get(status_key).and_then(Json::as_u64).unwrap_or(0);
            let start = k.get(start_key).and_then(Json::as_u64).unwrap_or(0);
            let end = k.get(end_key).and_then(Json::as_u64).unwrap_or(0);
            if status == 3 || (left.is_none() && status != 2) {
                continue;
            }
            let left = if status == 2 { 0 } else { left.map(|l| l.min(100)).unwrap_or(0) };
            let mut span = 0u64;
            if start > 0 && end > start {
                span = ms(end) - ms(start);
            } else if week {
                span = 7 * 86_400_000;
            }
            let label = if week {
                "7 days".to_string()
            } else if span > 86_400_000 && span % 86_400_000 == 0 {
                format!("{} days", span / 86_400_000)
            } else if span >= 3_600_000 && span % 3_600_000 == 0 {
                format!("{} hours", span / 3_600_000)
            } else if span > 0 {
                format!("{} minutes", span / 60_000)
            } else {
                "Allowance".to_string()
            };
            let window_name = if general {
                label
            } else {
                let mut chars = name.chars();
                let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
                format!("{first}{} · {label}", chars.as_str())
            };
            let mut w = serde_json::Map::new();
            w.insert("name".into(), json!(window_name));
            w.insert("used".into(), json!(100 - left));
            w.insert("total".into(), json!(100));
            w.insert("remaining".into(), json!(left));
            w.insert("remainingPercentage".into(), json!(left));
            w.insert("resetAt".into(), if end > 0 { json!(iso_ms(ms(end))) } else { Json::Null });
            windows.push(Json::Object(w));
        }
        out.extend(windows);
    }
    Some(out)
}

fn iso_ms(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// `getMiniMaxCodeUsage(connection)` in the USAGE_HANDLERS shape: credits
/// balance, plan tier and M Plan windows. Transport failures degrade to the
/// plain "not implemented" message like the JS handler.
pub async fn get_minimax_code_usage(
    provider: &str,
    access_token: Option<&str>,
    psd: &Json,
) -> Json {
    let Some(site) = site_for(provider) else {
        return json!({ "message": format!("Usage API not implemented for {provider}") });
    };
    let Some(access) = access_token.filter(|t| !t.is_empty()) else {
        return json!({ "message": format!("Usage API not implemented for {provider}") });
    };

    let user_id = psd.get("realUserID").and_then(Json::as_str).unwrap_or("").to_string();
    let mut who_user_id = user_id.clone();
    let mut who_email: Option<String> = None;
    let mut who_name: Option<String> = None;

    if user_id.is_empty() {
        match account_call(site, "/v1/api/user/info", access, &user_id, None).await {
            Ok(env) => {
                let d = env.get("data").filter(|v| v.is_object());
                let u = d
                    .and_then(|d| d.get("userInfo").or_else(|| d.get("user_info")))
                    .or_else(|| env.get("userInfo").or_else(|| env.get("user_info")));
                let first = |ks: &[&str]| {
                    ks.iter().find_map(|k| u.and_then(|u| u.get(*k)).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                };
                if let Some(id) = first(&["realUserID", "real_user_id"]) {
                    who_user_id = id;
                }
                who_email = first(&["userEmail", "email", "userMail", "user_email"]);
                who_name = first(&["name", "userName", "user_name"]);
            }
            Err(AccountRefused::Message(msg)) => return json!({ "message": format!("MiniMax Code sign-in expired — unauthorized, please re-authorize ({msg})") }),
            Err(_) => return json!({ "message": "MiniMax Code usage API error" }),
        }
    }

    let membership = match get_membership(site, access, &who_user_id).await {
        Ok(m) => m,
        Err(AccountRefused::Message(msg)) => return json!({ "message": format!("MiniMax Code sign-in expired — unauthorized, please re-authorize ({msg})") }),
        Err(_) => return json!({ "message": "MiniMax Code usage API error" }),
    };

    let has_token_plan = membership.get("hasTokenPlan").and_then(Json::as_bool);
    let balance = membership.get("balance").and_then(Json::as_str).and_then(|b| b.parse::<f64>().ok());
    let expires_at = membership.get("expiresAt").and_then(Json::as_u64).unwrap_or(0);
    let tier = membership.get("tier").and_then(Json::as_str);
    let op_group_id = membership.get("opGroupId").and_then(Json::as_str);

    let mut quotas = serde_json::Map::new();
    if let Some(n) = balance {
        quotas.insert(
            "Credits".into(),
            json!({ "used": 0, "total": n, "isCreditBalance": true, "currency": "credits" }),
        );
    }
    if has_token_plan == Some(true) && op_group_id.is_some() {
        // plain Bearer call to the platform host
        let url = format!("{}/v1/api/openplatform/coding_plan/remains", site.platform.trim_end_matches('/'));
        let mut headers = HeaderList::new();
        headers.insert("Accept".into(), "application/json".into());
        headers.insert("Authorization".into(), format!("Bearer {access}"));
        if let Some(g) = op_group_id {
            headers.insert("X-Group-Id".into(), g.to_string());
        }
        let req = FetchRequest::get(url).with_headers(headers).with_timeout_ms(15_000);
        match proxy_fetch(req, None).await {
            Ok(res) if res.ok() => match res.json_body().await {
                Ok(body) => match parse_plan_windows(&body) {
                    Some(windows) => {
                        for w in windows {
                            if let Some(name) = w.get("name").and_then(Json::as_str) {
                                quotas.insert(name.to_string(), w);
                            }
                        }
                    }
                    None => {
                        quotas.insert("M Plan windows".into(), json!({ "name": "M Plan windows", "used": 0, "total": 0, "resetAt": Json::Null, "message": "MiniMax M Plan: no plan in the reply" }));
                    }
                },
                Err(_) => {}
            },
            Ok(res) => {
                quotas.insert("M Plan windows".into(), json!({ "name": "M Plan windows", "used": 0, "total": 0, "resetAt": Json::Null, "message": format!("MiniMax M Plan: HTTP {}", res.status) }));
            }
            Err(e) => {
                quotas.insert("M Plan windows".into(), json!({ "name": "M Plan windows", "used": 0, "total": 0, "resetAt": Json::Null, "message": format!("MiniMax M Plan: {e}") }));
            }
        }
    }
    if quotas.is_empty() {
        quotas.insert(
            "Plan".into(),
            json!({ "name": "Plan", "used": 0, "total": 0, "resetAt": Json::Null, "message": if has_token_plan == Some(false) { "Free account (no M Plan)" } else { "No usage data returned" } }),
        );
    }

    let mut out = serde_json::Map::new();
    out.insert("quotas".into(), Json::Object(quotas));
    out.insert(
        "plan".into(),
        json!(if has_token_plan == Some(true) {
            tier.map(|t| format!("M Plan {t}")).unwrap_or_else(|| "M Plan".to_string())
        } else {
            "Free".to_string()
        }),
    );
    if expires_at > 0 {
        out.insert("planExpiresAt".into(), json!(iso_ms(if expires_at < 1_000_000_000_000 { expires_at * 1000 } else { expires_at })));
    }
    out.insert(
        "user".into(),
        json!(who_email.or(who_name).or_else(|| psd.get("realUserID").and_then(Json::as_str).map(str::to_string)).unwrap_or(who_user_id)),
    );
    Json::Object(out)
}

enum AccountRefused {
    Message(String),
}

async fn account_call(
    site: &MinimaxSite,
    path: &str,
    access: &str,
    user_id: &str,
    body: Option<&Json>,
) -> std::result::Result<Json, AccountRefused> {
    let (url, headers, body_str) = signed_account_request(site, path, access, user_id, body, now_ms(), 0);
    let mut req = FetchRequest::new(
        if body_str.is_some() { http::Method::POST } else { http::Method::GET },
        url,
    );
    req.headers = headers;
    req.body = body_str.map(Into::into);
    req.header_timeout = Some(std::time::Duration::from_secs(15));
    let res = proxy_fetch(req, None).await.map_err(|_| AccountRefused::Message("network error".into()))?;
    if res.status == 401 || res.status == 403 {
        return Err(AccountRefused::Message(format!("the sign-in was refused (HTTP {})", res.status)));
    }
    if !res.ok() {
        return Err(AccountRefused::Message(format!("HTTP {}", res.status)));
    }
    let env = res.json_body().await.map_err(|_| AccountRefused::Message("not JSON".into()))?;
    check_status(&env).map_err(AccountRefused::Message)?;
    Ok(env)
}

fn check_status(env: &Json) -> std::result::Result<(), String> {
    if let Some(si) = env.get("statusInfo") {
        if let Some(code) = si.get("code").and_then(Json::as_u64) {
            if code != 0 {
                if code == 1_000_048 {
                    return Err("the sign-in was refused".into());
                }
                let fallback = format!("status {code}");
                let msg = si.get("message").or_else(|| si.get("msg")).and_then(Json::as_str).unwrap_or(&fallback);
                return Err(msg.to_string());
            }
        }
    }
    if let Some(br) = env.get("base_resp") {
        if let Some(code) = br.get("status_code").and_then(Json::as_u64) {
            if code != 0 {
                let fallback = format!("status {code}");
                let msg = br.get("status_msg").and_then(Json::as_str).unwrap_or(&fallback);
                return Err(msg.to_string());
            }
        }
    }
    Ok(())
}

async fn get_membership(site: &MinimaxSite, access: &str, user_id: &str) -> std::result::Result<Json, AccountRefused> {
    // Workspace call first: type-0 workspace carries membership info.
    let ws = match account_call(site, "/matrix/api/v1/user/get_user_extra_info", access, user_id, Some(&json!({}))).await {
        Ok(env) => {
            let list = env.get("workspaces").and_then(Json::as_array).map(|a| a.clone())
                .or_else(|| env.get("data").and_then(|d| d.get("workspaces")).and_then(Json::as_array).map(|a| a.clone()))
                .unwrap_or_default();
            list.into_iter().find(|w| w.get("workspace_type").and_then(Json::as_u64) == Some(0))
        }
        Err(AccountRefused::Message(m)) if m.contains("HTTP 4") => return Err(AccountRefused::Message(m)),
        Err(_) => None,
    };

    match ws {
        Some(w) => {
            let id = w.get("workspace_id").and_then(Json::as_u64).map(|n| n.to_string())
                .or_else(|| w.get("workspace_id").and_then(Json::as_str).map(str::to_string));
            let ws_m = parse_membership(&w);
            if let Some(id) = id {
                match account_call(site, "/matrix/api/v1/commerce/get_membership_info", access, user_id, Some(&json!({ "workspace_id": id }))).await {
                    Ok(m) => {
                        let m = parse_membership(&m);
                        let mut merged = ws_m.as_object().cloned().unwrap_or_default();
                        for (k, v) in m.as_object().cloned().unwrap_or_default() {
                            merged.insert(k, v);
                        }
                        let has = match (ws_m.get("hasTokenPlan").and_then(Json::as_bool), m.get("hasTokenPlan").and_then(Json::as_bool)) {
                            (Some(a), Some(b)) => Some(a || b),
                            (Some(a), None) => Some(a),
                            (None, Some(b)) => Some(b),
                            _ => None,
                        };
                        if let Some(h) = has {
                            merged.insert("hasTokenPlan".into(), json!(h));
                        }
                        return Ok(Json::Object(merged));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(ws_m)
        }
        None => account_call(site, "/matrix/api/v1/commerce/get_membership_info", access, user_id, Some(&json!({}))).await.map(|e| parse_membership(&e)),
    }
}

/// Small builder sugar to keep request construction terse.
trait FetchRequestExt {
    fn with_headers(self, headers: HeaderList) -> Self;
    fn with_timeout_ms(self, ms: u64) -> Self;
}

impl FetchRequestExt for FetchRequest {
    fn with_headers(mut self, headers: HeaderList) -> Self {
        self.headers = headers;
        self
    }
    fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.header_timeout = Some(std::time::Duration::from_millis(ms));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_request_matches_shape_and_headers() {
        let site = site_for("minimax-code").unwrap();
        let (url, headers, body) = signed_account_request(site, "/v1/api/user/info", "tok", "", None, 1_700_000_000_000, -420);
        assert!(url.starts_with("https://agent.minimaxi.com/v1/api/user/info?"));
        assert!(url.contains("device_platform=mcode"));
        assert!(url.contains("user_id=0"));
        assert!(url.contains("timezone_offset=-420"));
        assert!(body.is_none());
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer tok"));
        assert!(headers.get("yy").is_some());
        assert_eq!(headers.get("x-timestamp").map(String::as_str), Some("1700000000"));
        assert!(headers.get("x-signature").is_some());
    }

    #[test]
    fn membership_parse_covers_keys() {
        let m = parse_membership(&json!({
            "data": { "has_token_plan": true, "token_plan_tier": "pro", "op_group_id": "g1", "token_plan_expires_at": 5, "opcredit_balance": 12.5 }
        }));
        assert_eq!(m.get("hasTokenPlan").and_then(Json::as_bool), Some(true));
        assert_eq!(m.get("tier").and_then(Json::as_str), Some("pro"));
        assert_eq!(m.get("opGroupId").and_then(Json::as_str), Some("g1"));
        assert_eq!(m.get("balance").and_then(Json::as_str), Some("12.5"));
        assert_eq!(m.get("expiresAt").and_then(Json::as_u64), Some(5));
        assert!(parse_membership(&json!({})).as_object().map(|o| o.is_empty()).unwrap_or(false));
    }

    #[test]
    fn plan_windows_shape_and_labels() {
        let body = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 80,
                    "current_interval_status": 0,
                    "start_time": 1000,
                    "end_time": 1000 + 5*3600,
                    "current_weekly_remaining_percent": 50,
                    "current_weekly_status": 0,
                    "weekly_start_time": 0,
                    "weekly_end_time": 0
                },
                {
                    "model_name": "pei",
                    "current_interval_remaining_percent": 0,
                    "current_interval_status": 2,
                    "start_time": 0,
                    "end_time": 0,
                    "current_weekly_remaining_percent": 100,
                    "current_weekly_status": 0,
                    "weekly_start_time": 0,
                    "weekly_end_time": 0
                }
            ]
        });
        // JS pushes a window for every line whose status is not 3 and whose
        // remaining percent is present (or status is 2), so "general" and "pei"
        // each contribute an interval and a week: 4 windows.
        let windows = parse_plan_windows(&body).expect("parses");
        assert_eq!(windows.len(), 4, "general interval + general week + pei interval + pei week");
        assert_eq!(windows[0].get("name").and_then(Json::as_str), Some("5 hours"));
        assert_eq!(windows[0].get("remaining").and_then(Json::as_u64), Some(80));
        assert!(windows[0].get("resetAt").and_then(Json::as_str).is_some());
        assert_eq!(windows[1].get("name").and_then(Json::as_str), Some("7 days"));
        assert_eq!(windows[1].get("remaining").and_then(Json::as_u64), Some(50));
        // Status 2 means the interval is exhausted: remaining is forced to 0.
        // pei carries no interval timestamps, so its interval label is the
        // duration-less "Allowance" bucket the JS uses for span 0.
        assert_eq!(windows[2].get("name").and_then(Json::as_str), Some("Pei · Allowance"));
        assert_eq!(windows[2].get("remaining").and_then(Json::as_u64), Some(0));
        assert_eq!(windows[3].get("name").and_then(Json::as_str), Some("Pei · 7 days"));
        assert_eq!(windows[3].get("remaining").and_then(Json::as_u64), Some(100));
    }

    #[test]
    fn plan_windows_reject_bad_body() {
        assert!(parse_plan_windows(&json!({})).is_none());
        assert!(parse_plan_windows(&json!({ "base_resp": { "status_code": 1 } })).is_none());
    }
}
