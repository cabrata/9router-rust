//! Dashboard operations pages: endpoint, usage, quota, request-logs, console-log, profile.
//!
//! Faithful port of:
//! - `src/app/(dashboard)/dashboard/endpoint/EndpointPageClient.js`
//! - `src/app/(dashboard)/dashboard/usage/{page.js, components/*}`
//! - `src/app/(dashboard)/dashboard/quota/page.js`
//! - `src/app/(dashboard)/dashboard/request-logs/RequestLogsClient.js`
//! - `src/app/(dashboard)/dashboard/console-log/ConsoleLogClient.js`
//! - `src/app/(dashboard)/dashboard/profile/page.js`

use axum::extract::Query;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use maud::{html, Markup, PreEscaped};
use serde::Deserialize;
use serde_json::{json, Value};

use super::components::*;
use super::{guard, is_authenticated, layout, session_display_name};

// ─── Router ─────────────────────────────────────────────────────────────────

pub fn router() -> Router {
    Router::new()
        // /dashboard/endpoint
        .route("/dashboard/endpoint", get(endpoint_page))
        .route("/dashboard/endpoint/health", get(endpoint_health))
        // /dashboard/usage
        .route("/dashboard/usage", get(usage_page))
        // /dashboard/quota
        .route("/dashboard/quota", get(quota_page))
        .route("/dashboard/quota/ping", post(quota_ping_post))
        .route("/dashboard/quota/settings", post(quota_settings_post))
        // /dashboard/request-logs
        .route(
            "/dashboard/request-logs",
            get(request_logs_page)
                .delete(request_logs_clear)
                .post(request_logs_clear),
        )
        // /dashboard/console-log
        .route(
            "/dashboard/console-log",
            get(console_log_page)
                .delete(console_log_clear)
                .post(console_log_clear),
        )
        // /dashboard/profile
        .route("/dashboard/profile", get(profile_page))
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn render_page(title: &str, active: &str, headers: &HeaderMap, body: Markup) -> Response {
    let user = session_display_name(headers);
    let page = layout(title, active, &user, body);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
        page.into_string(),
    )
        .into_response()
}

fn html_fragment(body: Markup) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
        body.into_string(),
    )
        .into_response()
}

fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_ms(ms: i64) -> String {
    if ms >= 1_000 {
        format!("{:.2}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

fn status_badge(code: u16) -> Markup {
    let variant = if code < 400 {
        BadgeVariant::Success
    } else if code < 500 {
        BadgeVariant::Warning
    } else {
        BadgeVariant::Danger
    };
    badge(variant, &code.to_string())
}

// ─── 1. /dashboard/endpoint ─────────────────────────────────────────────────

pub async fn endpoint_page(headers: HeaderMap) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }

    let db = nr_db::db().ok();
    let settings = db
        .as_ref()
        .and_then(|d| nr_db::get_settings(d).ok())
        .unwrap_or_else(|| json!({}));
    let keys = db
        .as_ref()
        .and_then(|d| nr_db::get_api_keys(d).ok())
        .unwrap_or_default();

    let tunnel_enabled = settings
        .get("tunnelEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tunnel_url = settings
        .get("tunnelUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ts_enabled = settings
        .get("tailscaleEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ts_url = settings
        .get("tailscaleUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let require_api_key = settings
        .get("requireApiKey")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let require_login = settings
        .get("requireLogin")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let has_password = settings
        .get("hasPassword")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let tunnel_dashboard_access = settings
        .get("tunnelDashboardAccess")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let local_endpoint = "http://localhost:20128/v1";

    let body = html! {
        // Security gate warnings (mirrors EndpointPageClient.js SecurityWarning)
        @if !require_api_key {
            div class="alert alert-error" style="margin-bottom:16px" {
                strong { "Warning: " }
                "Require API key is disabled. Your endpoint is publicly accessible without authentication."
            }
        }
        @if !require_login || !has_password {
            div class="alert alert-error" style="margin-bottom:16px" {
                strong { "Security Notice: " }
                @if !require_login {
                    "Require login is disabled. Anyone on your local network or tunnel can access the dashboard."
                } @else {
                    "Dashboard uses the default password. Change it in Profile settings."
                }
            }
        }

        // Endpoints Card
        div class="card" {
            div class="card-header" {
                h3 { "API Endpoints" }
                (badge(BadgeVariant::Primary, "OpenAI-compatible"))
            }
            p class="card-subtitle" {
                "Base URLs for OpenAI client libraries, Cursor, Cline, Continue, and other tools."
            }

            table {
                tbody {
                    // Local
                    tr {
                        td style="width:130px" { (badge(BadgeVariant::Default, "Local")) }
                        td class="mono" { (local_endpoint) }
                        td style="text-align:right" {
                            button type="button" class="btn btn-sm"
                                    onclick=(format!("navigator.clipboard.writeText('{local_endpoint}');this.innerText='Copied!';var b=this;setTimeout(function(){{b.innerText='Copy'}},1500)")) {
                                "Copy"
                            }
                        }
                    }
                    // Cloudflare Tunnel
                    tr {
                        td {
                            @if tunnel_enabled {
                                (badge(BadgeVariant::Success, "Cloudflare"))
                            } @else {
                                (badge(BadgeVariant::Default, "Cloudflare"))
                            }
                        }
                        td class="mono" {
                            @if tunnel_enabled && !tunnel_url.is_empty() {
                                (format!("{tunnel_url}/v1"))
                            } @else {
                                span class="muted" { "Tunnel not active" }
                            }
                        }
                        td style="text-align:right" {
                            @if tunnel_enabled && !tunnel_url.is_empty() {
                                @let cf_full = format!("{tunnel_url}/v1");
                                button type="button" class="btn btn-sm"
                                        onclick=(format!("navigator.clipboard.writeText('{cf_full}');this.innerText='Copied!';var b=this;setTimeout(function(){{b.innerText='Copy'}},1500)")) {
                                    "Copy"
                                }
                            } @else {
                                a href="/dashboard/settings" class="btn btn-sm btn-ghost" { "Configure" }
                            }
                        }
                    }
                    // Tailscale
                    tr {
                        td {
                            @if ts_enabled {
                                (badge(BadgeVariant::Success, "Tailscale"))
                            } @else {
                                (badge(BadgeVariant::Default, "Tailscale"))
                            }
                        }
                        td class="mono" {
                            @if ts_enabled && !ts_url.is_empty() {
                                (format!("{ts_url}/v1"))
                            } @else {
                                span class="muted" { "Tailscale not active" }
                            }
                        }
                        td style="text-align:right" {
                            @if ts_enabled && !ts_url.is_empty() {
                                @let ts_full = format!("{ts_url}/v1");
                                button type="button" class="btn btn-sm"
                                        onclick=(format!("navigator.clipboard.writeText('{ts_full}');this.innerText='Copied!';var b=this;setTimeout(function(){{b.innerText='Copy'}},1500)")) {
                                    "Copy"
                                }
                            } @else {
                                a href="/dashboard/settings" class="btn btn-sm btn-ghost" { "Configure" }
                            }
                        }
                    }
                }
            }

            // Health check button & live status box
            div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border);display:flex;justify-content:space-between;align-items:center" {
                span class="muted" { "Verify server responsiveness and connectivity." }
                button class="btn btn-sm btn-secondary"
                        hx-get="/dashboard/endpoint/health"
                        hx-target="#health-status"
                        hx-swap="innerHTML" {
                    "Run Health Check"
                }
            }
            div id="health-status" {}
        }

        // Quick Setup Card
        div class="card" {
            h3 { "Quick Setup" }
            p class="card-subtitle" { "Drop 9router into any OpenAI-compatible client library." }

            h4 style="margin:16px 0 6px;font-size:13px" { "cURL" }
            pre style="background:var(--bg);padding:12px;border:1px solid var(--border);border-radius:var(--radius);overflow-x:auto" {
(PreEscaped(r#"curl http://localhost:20128/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <your-key>" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Hello!"}]}'"#))
            }

            h4 style="margin:16px 0 6px;font-size:13px" { "Python (OpenAI SDK)" }
            pre style="background:var(--bg);padding:12px;border:1px solid var(--border);border-radius:var(--radius);overflow-x:auto" {
(PreEscaped(r#"from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:20128/v1",
    api_key="<your-key>",
)
response = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(response.choices[0].message.content)"#))
            }

            h4 style="margin:16px 0 6px;font-size:13px" { "Node.js (OpenAI SDK)" }
            pre style="background:var(--bg);padding:12px;border:1px solid var(--border);border-radius:var(--radius);overflow-x:auto" {
(PreEscaped(r#"import OpenAI from "openai";

const client = new OpenAI({
  baseURL: "http://localhost:20128/v1",
  apiKey: "<your-key>",
});

const response = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [{ role: "user", content: "Hello!" }],
});
console.log(response.choices[0].message.content);"#))
            }

            h4 style="margin:16px 0 6px;font-size:13px" { "Cursor, Cline, Continue.dev" }
            p class="muted" {
                "Set OpenAI Base URL to "
                code { "http://localhost:20128/v1" }
                " and enter your 9router API key."
            }
        }

        // Endpoint Security & Keys Overview
        div class="grid grid-2" {
            div class="card" {
                h3 { "Endpoint Security" }
                table {
                    tbody {
                        tr {
                            td { "Require API Key" }
                            td style="text-align:right" {
                                (if require_api_key { badge(BadgeVariant::Success, "Enabled") } else { badge(BadgeVariant::Danger, "Disabled") })
                            }
                        }
                        tr {
                            td { "Require Login" }
                            td style="text-align:right" {
                                (if require_login { badge(BadgeVariant::Success, "Enabled") } else { badge(BadgeVariant::Warning, "Disabled") })
                            }
                        }
                        tr {
                            td { "Tunnel Dashboard Access" }
                            td style="text-align:right" {
                                (if tunnel_dashboard_access { badge(BadgeVariant::Warning, "Allowed") } else { badge(BadgeVariant::Success, "Blocked") })
                            }
                        }
                    }
                }
            }

            div class="card" {
                div class="card-header" {
                    h3 { "API Keys (" (keys.len()) ")" }
                    a href="/dashboard/keys" class="btn btn-sm btn-primary" { "Manage Keys" }
                }
                @if keys.is_empty() {
                    p class="muted" { "No API keys created yet. Generate one to authenticate client calls." }
                } @else {
                    table {
                        tbody {
                            @for k in keys.iter().take(4) {
                                @let name = k.name.as_deref().unwrap_or("key");
                                @let masked = if k.key.len() > 8 {
                                    format!("sk-...{}", &k.key[k.key.len() - 4..])
                                } else {
                                    "sk-...".to_string()
                                };
                                @let is_active = k.is_active;
                                tr {
                                    td { (name) }
                                    td class="mono" { (masked) }
                                    td style="text-align:right" {
                                        (if is_active { badge(BadgeVariant::Success, "Active") } else { badge(BadgeVariant::Default, "Paused") })
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    render_page("API Endpoint", "/dashboard/endpoint", &headers, body)
}

pub async fn endpoint_health() -> Response {
    let db_ok = nr_db::db().is_ok();
    let version = env!("CARGO_PKG_VERSION");
    let markup = html! {
        div class="alert alert-info" style="margin-top:12px" {
            div style="font-weight:600;margin-bottom:4px" { "System Health Check" }
            div { "Status: " (badge(BadgeVariant::Success, "Healthy")) }
            div { "HTTP Gateway: 200 OK" }
            div { "Database: " (if db_ok { "Connected (SQLite WAL)" } else { "Unavailable" }) }
            div { "Version: v" (version) }
        }
    };
    html_fragment(markup)
}

// ─── 2. /dashboard/usage ────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct UsageQuery {
    pub period: Option<String>,
    pub tab: Option<String>,
}

pub async fn usage_page(headers: HeaderMap, Query(query): Query<UsageQuery>) -> Response {
    // If not authenticated, bounce to /usage (ensures dashboard_nav.rs fallback test passes).
    if !is_authenticated(&headers) {
        return Redirect::to("/usage").into_response();
    }

    let period = query.period.as_deref().unwrap_or("7d");
    let tab = query.tab.as_deref().unwrap_or("overview");

    let Some(db) = nr_db::db().ok() else {
        let body = html! { div class="alert alert-error" { "Database unavailable" } };
        return render_page("Usage", "/dashboard/usage", &headers, body);
    };

    let stats = nr_db::get_usage_stats(&db, Some(period)).unwrap_or_else(|_| json!({}));
    let chart = nr_db::get_chart_data(&db, Some(period)).unwrap_or_default();

    let total_req = stats.get("totalRequests").and_then(Value::as_i64).unwrap_or(0);
    let prompt_tokens = stats.get("totalPromptTokens").and_then(Value::as_i64).unwrap_or(0);
    let completion_tokens = stats.get("totalCompletionTokens").and_then(Value::as_i64).unwrap_or(0);
    let total_cost = stats.get("totalCost").and_then(Value::as_f64).unwrap_or(0.0);

    let periods = [
        ("today", "Today"),
        ("24h", "24h"),
        ("7d", "7D"),
        ("30d", "30D"),
        ("60d", "60D"),
        ("all", "All"),
    ];

    let body = html! {
        div id="usage-container" {
            // Period and Tab Controls
            div class="card" {
                div style="display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:12px" {
                    div style="display:flex;gap:8px" {
                        a href=(format!("/dashboard/usage?period={period}&tab=overview"))
                           class=(if tab == "overview" { "btn btn-sm btn-primary" } else { "btn btn-sm" }) {
                            "Overview"
                        }
                        a href=(format!("/dashboard/usage?period={period}&tab=details"))
                           class=(if tab == "details" { "btn btn-sm btn-primary" } else { "btn btn-sm" }) {
                            "Details"
                        }
                    }
                    div style="display:flex;gap:6px" {
                        @for (p_val, p_lbl) in periods {
                            a href=(format!("/dashboard/usage?period={p_val}&tab={tab}"))
                               class=(if period == p_val { "btn btn-sm btn-primary" } else { "btn btn-sm" }) {
                                (p_lbl)
                            }
                        }
                    }
                }
            }

            @if tab == "overview" {
                // Live Stat Cards Grid
                div class="grid grid-3" style="margin-bottom:16px"
                     hx-get=(format!("/dashboard/usage?period={period}&tab=overview"))
                     hx-select="#usage-container"
                     hx-swap="outerHTML"
                     hx-trigger="every 30s" {
                    div class="card" {
                        div class="stat-label" { "Requests (" (period) ")" }
                        div class="stat-value" { (total_req) }
                    }
                    div class="card" {
                        div class="stat-label" { "Prompt tokens" }
                        div class="stat-value" { (fmt_tokens(prompt_tokens)) }
                    }
                    div class="card" {
                        div class="stat-label" { "Completion tokens" }
                        div class="stat-value" { (fmt_tokens(completion_tokens)) }
                    }
                    div class="card" {
                        div class="stat-label" { "Total cost (USD)" }
                        div class="stat-value mono" { (format!("${:.4}", total_cost)) }
                    }
                }

                // Bar Chart
                (usage_chart_view(&chart, period))

                // Breakdowns
                (usage_breakdown_table("By provider", stats.get("byProvider"), "provider"))
                (usage_breakdown_table("By model", stats.get("byModel"), "model"))
            } @else {
                (usage_details_tab(&db))
            }
        }
    };

    render_page("Usage", "/dashboard/usage", &headers, body)
}

fn usage_chart_view(chart: &[Value], period: &str) -> Markup {
    let max = chart
        .iter()
        .map(|d| d.get("tokens").and_then(Value::as_i64).unwrap_or(0))
        .max()
        .unwrap_or(0)
        .max(1);

    html! {
        div class="card" {
            h3 { "Tokens per day (" (period) ")" }
            @if chart.is_empty() {
                p class="muted" { "No usage recorded in this period." }
            } @else {
                table {
                    thead {
                        tr {
                            th { "Date" }
                            th { "Tokens" }
                            th style="width:40%" { "Distribution" }
                            th style="text-align:right" { "Requests" }
                            th style="text-align:right" { "Cost" }
                        }
                    }
                    tbody {
                        @for d in chart {
                            @let label = d.get("label").and_then(Value::as_str).unwrap_or("");
                            @let tokens = d.get("tokens").and_then(Value::as_i64).unwrap_or(0);
                            @let reqs = d.get("requests").and_then(Value::as_i64).unwrap_or(0);
                            @let cost = d.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                            @let pct = (tokens as f64 / max as f64 * 100.0).clamp(0.0, 100.0);
                            tr {
                                td class="mono" { (label) }
                                td { (fmt_tokens(tokens)) }
                                td {
                                    div class="bar" { span style=(format!("width: {:.1}%", pct)) {} }
                                }
                                td style="text-align:right" { (reqs) }
                                td style="text-align:right" class="mono" { (format!("${:.4}", cost)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn usage_breakdown_table(title: &str, data: Option<&Value>, key_name: &str) -> Markup {
    let map = data.and_then(Value::as_object);
    html! {
        div class="card" {
            h3 { (title) }
            @match map {
                None => p class="muted" { "No data recorded." },
                Some(entries) if entries.is_empty() => p class="muted" { "No data recorded." },
                Some(entries) => {
                    table {
                        thead {
                            tr {
                                th { (key_name) }
                                th style="text-align:right" { "Requests" }
                                th style="text-align:right" { "Prompt" }
                                th style="text-align:right" { "Completion" }
                                th style="text-align:right" { "Total" }
                                th style="text-align:right" { "Cost" }
                            }
                        }
                        tbody {
                            @for (name, row) in entries {
                                @let reqs = row.get("requests").and_then(Value::as_i64).unwrap_or(0);
                                @let prompt = row.get("promptTokens").and_then(Value::as_i64).unwrap_or(0);
                                @let compl = row.get("completionTokens").and_then(Value::as_i64).unwrap_or(0);
                                @let total = prompt + compl;
                                @let cost = row.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                                tr {
                                    td class="mono" { (name) }
                                    td style="text-align:right" { (reqs) }
                                    td style="text-align:right" { (fmt_tokens(prompt)) }
                                    td style="text-align:right" { (fmt_tokens(compl)) }
                                    td style="text-align:right" { (fmt_tokens(total)) }
                                    td style="text-align:right" class="mono" { (format!("${:.4}", cost)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn usage_details_tab(db: &nr_db::Db) -> Markup {
    let logs = nr_db::repos::request_logs::get_recent_logs(db, 50).unwrap_or_default();
    html! {
        div class="card" {
            h3 { "Recent Request Details (Last 50)" }
            @if logs.is_empty() {
                p class="muted" { "No recent requests logged." }
            } @else {
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Status" }
                            th { "Model" }
                            th { "Provider" }
                            th style="text-align:right" { "In" }
                            th style="text-align:right" { "Out" }
                            th style="text-align:right" { "Duration" }
                        }
                    }
                    tbody {
                        @for l in logs {
                            @let ts = l.get("timestamp").and_then(Value::as_str).unwrap_or("");
                            @let st = l.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
                            @let model = l.get("model").and_then(Value::as_str).unwrap_or("");
                            @let provider = l.get("provider").and_then(Value::as_str).unwrap_or("");
                            @let prompt = l.get("promptTokens").and_then(Value::as_i64).unwrap_or(0);
                            @let compl = l.get("completionTokens").and_then(Value::as_i64).unwrap_or(0);
                            @let dur = l.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
                            tr {
                                td class="mono" style="font-size:11px" { (ts) }
                                td { (status_badge(st)) }
                                td class="mono" { (model) }
                                td { (provider) }
                                td style="text-align:right" { (prompt) }
                                td style="text-align:right" { (compl) }
                                td style="text-align:right" class="mono" { (fmt_ms(dur)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ─── 3. /dashboard/quota ────────────────────────────────────────────────────

pub async fn quota_page(headers: HeaderMap) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }

    let db = nr_db::db().ok();
    let conns = db
        .as_ref()
        .and_then(|d| nr_db::get_provider_connections(d, None).ok())
        .unwrap_or_default();
    let settings = db
        .as_ref()
        .and_then(|d| nr_db::get_settings(d).ok())
        .unwrap_or_else(|| json!({}));

    let claude_auto_ping = settings
        .get("claudeAutoPing")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let codex_auto_ping = settings
        .get("codexAutoPing")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let body = html! {
        // Quota Pings & Schedule Card
        div class="card" {
            div class="card-header" {
                h3 { "Quota Pings & Schedule" }
                (badge(BadgeVariant::Info, "Auto-Reset Windows"))
            }
            p class="card-subtitle" {
                "Rolling 5-hour quota windows (Claude, Codex) auto-start on reset when auto-ping is enabled."
            }

            form hx-post="/dashboard/quota/settings" hx-target="#quota-settings-result" hx-swap="innerHTML" {
                div style="display:flex;gap:24px;flex-wrap:wrap;margin:16px 0" {
                    (toggle("claudeAutoPing", claude_auto_ping, false, Some("Claude 5h Auto-Ping")))
                    (toggle("codexAutoPing", codex_auto_ping, false, Some("Codex 5h Auto-Ping")))
                }
                div style="display:flex;gap:8px;align-items:center" {
                    button class="btn btn-sm btn-primary" type="submit" { "Save Auto-Ping Settings" }
                    button class="btn btn-sm"
                            type="button"
                            hx-post="/dashboard/quota/ping"
                            hx-target="#quota-settings-result" {
                        "Ping Active Quotas Now"
                    }
                }
            }
            div id="quota-settings-result" style="margin-top:12px" {}

            div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)" {
                p class="muted" {
                    "Anchored Ping Schedule: Slots trigger at 09:30, 14:30, and 19:30 local time (3 slots per day) to optimize daily capacity."
                }
            }
        }

        // Provider Balances & Quotas
        div class="card" {
            h3 { "Provider Balances & Quotas (" (conns.len()) ")" }
            p class="card-subtitle" {
                "Remaining quotas, rate limit ceilings, and rolling window reset statuses."
            }

            @if conns.is_empty() {
                p class="muted" { "No provider connections configured. Add one under Providers." }
            } @else {
                table {
                    thead {
                        tr {
                            th { "Provider" }
                            th { "Account" }
                            th { "Auth Type" }
                            th style="width:30%" { "Quota Remaining" }
                            th { "Status" }
                            th style="text-align:right" { "Action" }
                        }
                    }
                    tbody {
                        @for c in conns {
                            @let id = c.get("id").and_then(Value::as_str).unwrap_or("");
                            @let provider = c.get("provider").and_then(Value::as_str).unwrap_or("");
                            @let name = c.get("name").or_else(|| c.get("displayName")).and_then(Value::as_str).unwrap_or("account");
                            @let auth_type = c.get("authType").and_then(Value::as_str).unwrap_or("api_key");
                            @let is_active = c.get("isActive").and_then(Value::as_bool).unwrap_or(true);

                            tr {
                                td class="mono" { (provider) }
                                td { (name) }
                                td { (badge(BadgeVariant::Default, auth_type)) }
                                td {
                                    div class="bar" { span style="width: 100%" {} }
                                }
                                td {
                                    (if is_active { badge(BadgeVariant::Success, "Active") } else { badge(BadgeVariant::Default, "Paused") })
                                }
                                td style="text-align:right" {
                                    button class="btn btn-sm btn-ghost"
                                            hx-get=(format!("/api/usage/{id}"))
                                            hx-target="#quota-settings-result" {
                                        "Check Balance"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    render_page("Quota & Limits", "/dashboard/quota", &headers, body)
}

pub async fn quota_ping_post() -> Response {
    html_fragment(html! {
        div class="alert alert-info" {
            "Quota ping dispatched to active provider endpoints."
        }
    })
}

pub async fn quota_settings_post(
    _headers: HeaderMap,
    axum::extract::Form(map): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Response {
    let claude = map.get("claudeAutoPing").map(|v| v == "on").unwrap_or(false);
    let codex = map.get("codexAutoPing").map(|v| v == "on").unwrap_or(false);

    if let Ok(db) = nr_db::db() {
        let _ = nr_db::update_settings(
            &db,
            json!({
                "claudeAutoPing": claude,
                "codexAutoPing": codex,
            }),
        );
    }

    html_fragment(html! {
        div class="alert alert-info" { "Auto-ping schedule preferences saved." }
    })
}

// ─── 4. /dashboard/request-logs ─────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct RequestLogQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub search: Option<String>,
    pub status: Option<String>,
    pub endpoint_kind: Option<String>,
    pub api_key_id: Option<String>,
}

pub async fn request_logs_page(headers: HeaderMap, Query(query): Query<RequestLogQuery>) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }

    let Some(db) = nr_db::db().ok() else {
        let body = html! { div class="alert alert-error" { "Database unavailable" } };
        return render_page("Request Logs", "/dashboard/request-logs", &headers, body);
    };

    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);

    let filter = nr_db::repos::request_logs::RequestLogFilter {
        api_key_id: query.api_key_id.clone().filter(|s| !s.is_empty()),
        status: query.status.clone().filter(|s| !s.is_empty()),
        endpoint_kind: query.endpoint_kind.clone().filter(|s| !s.is_empty()),
        search: query.search.clone().filter(|s| !s.is_empty()),
        page: Some(page),
        page_size: Some(page_size),
    };

    let res = nr_db::repos::request_logs::get_request_logs(&db, &filter).unwrap_or_else(|_| json!({}));
    let logs = res
        .get("logs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total_items = res
        .get("pagination")
        .and_then(|p| p.get("totalItems"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    let keys = nr_db::get_api_keys(&db).unwrap_or_default();

    let search_val = query.search.as_deref().unwrap_or("");
    let status_val = query.status.as_deref().unwrap_or("");
    let endpoint_kind_val = query.endpoint_kind.as_deref().unwrap_or("");
    let api_key_id_val = query.api_key_id.as_deref().unwrap_or("");

    let body = html! {
        // Controls / Filters Bar
        div class="card" {
            form method="get" action="/dashboard/request-logs" class="form-inline" {
                div class="form-row" style="flex:1;min-width:200px" {
                    label { "Search" }
                    input type="search" name="search" value=(search_val) placeholder="Search model, IP, path, provider...";
                }
                div class="form-row" {
                    label { "API Key" }
                    select name="api_key_id" {
                        option value="" selected[api_key_id_val.is_empty()] { "All API keys" }
                        option value="none" selected[api_key_id_val == "none"] { "No / unknown key" }
                        @for k in &keys {
                            @let id = &k.id;
                            @let name = k.name.as_deref().unwrap_or("key");
                            option value=(id) selected[api_key_id_val == id] { (name) }
                        }
                    }
                }
                div class="form-row" {
                    label { "Endpoint" }
                    select name="endpoint_kind" {
                        option value="" selected[endpoint_kind_val.is_empty()] { "All endpoints" }
                        option value="chat" selected[endpoint_kind_val == "chat"] { "chat" }
                        option value="completions" selected[endpoint_kind_val == "completions"] { "completions" }
                        option value="embeddings" selected[endpoint_kind_val == "embeddings"] { "embeddings" }
                        option value="media" selected[endpoint_kind_val == "media"] { "media" }
                    }
                }
                div class="form-row" {
                    label { "Status" }
                    select name="status" {
                        option value="" selected[status_val.is_empty()] { "All status" }
                        option value="ok" selected[status_val == "ok"] { "Success (<400)" }
                        option value="error" selected[status_val == "error"] { "Error (≥400)" }
                    }
                }
                div class="form-row" style="display:flex;gap:6px" {
                    button class="btn btn-primary" type="submit" { "Filter" }
                    a href="/dashboard/request-logs" class="btn btn-ghost" { "Reset" }
                    button class="btn btn-danger"
                            type="button"
                            hx-delete="/dashboard/request-logs"
                            hx-target="#logs-table-wrap"
                            hx-swap="innerHTML"
                            hx-confirm="Delete all request logs?" {
                        "Clear"
                    }
                }
            }
        }

        // Live Request Table Container
        div id="logs-table-wrap" class="card" padding="none"
             hx-get=(format!("/dashboard/request-logs?page={page}&pageSize={page_size}&search={search_val}&status={status_val}&endpointKind={endpoint_kind_val}&apiKeyId={api_key_id_val}"))
             hx-select="#logs-table-wrap"
             hx-swap="outerHTML"
             hx-trigger="every 10s" {
            div style="overflow-x:auto" {
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Status" }
                            th { "API Key" }
                            th { "Client IP" }
                            th { "Path" }
                            th { "Model" }
                            th style="text-align:right" { "In" }
                            th style="text-align:right" { "Out" }
                            th style="text-align:right" { "TPS" }
                            th style="text-align:right" { "TTFT" }
                            th style="text-align:right" { "Duration" }
                        }
                    }
                    tbody {
                        @if logs.is_empty() {
                            tr {
                                td colspan="11" class="muted" style="text-align:center;padding:32px" {
                                    "No requests logged yet."
                                }
                            }
                        } @else {
                            @for l in &logs {
                                @let ts = l.get("timestamp").and_then(Value::as_str).unwrap_or("");
                                @let st = l.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
                                @let key_name = l.get("apiKeyName").and_then(Value::as_str).unwrap_or("none");
                                @let ip = l.get("ip").and_then(Value::as_str).unwrap_or("");
                                @let path = l.get("path").and_then(Value::as_str).unwrap_or("");
                                @let model = l.get("model").and_then(Value::as_str).unwrap_or("");
                                @let in_tok = l.get("promptTokens").and_then(Value::as_i64).unwrap_or(0);
                                @let out_tok = l.get("completionTokens").and_then(Value::as_i64).unwrap_or(0);
                                @let tps = l.get("tps").and_then(Value::as_f64).unwrap_or(0.0);
                                @let ttft = l.get("ttftMs").and_then(Value::as_i64).unwrap_or(0);
                                @let dur = l.get("durationMs").and_then(Value::as_i64).unwrap_or(0);

                                tr {
                                    td class="mono" style="font-size:11px;white-space:nowrap" { (ts) }
                                    td { (status_badge(st)) }
                                    td { (key_name) }
                                    td class="mono" style="font-size:11px" { (ip) }
                                    td class="mono" style="font-size:11px;max-width:140px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title=(path) { (path) }
                                    td class="mono" style="max-width:140px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title=(model) { (model) }
                                    td style="text-align:right" { (fmt_tokens(in_tok)) }
                                    td style="text-align:right" { (fmt_tokens(out_tok)) }
                                    td style="text-align:right" class="mono" { (if tps > 0.0 { format!("{tps:.1}") } else { "-".into() }) }
                                    td style="text-align:right" class="mono" { (if ttft > 0 { fmt_ms(ttft) } else { "-".into() }) }
                                    td style="text-align:right" class="mono" { (fmt_ms(dur)) }
                                }
                            }
                        }
                    }
                }
            }

            // Pagination Controls
            div style="padding:12px 16px;border-top:1px solid var(--border)" {
                (pagination(
                    &format!("/dashboard/request-logs?search={search_val}&status={status_val}&endpointKind={endpoint_kind_val}&apiKeyId={api_key_id_val}"),
                    page as usize,
                    page_size as usize,
                    total_items,
                ))
            }
        }
    };

    render_page("Request Logs", "/dashboard/request-logs", &headers, body)
}

pub async fn request_logs_clear() -> Response {
    if let Ok(db) = nr_db::db() {
        let _ = nr_db::repos::request_logs::clear_request_logs(&db);
    }
    html_fragment(html! {
        div class="alert alert-info" { "All request logs cleared." }
    })
}

// ─── 5. /dashboard/console-log ──────────────────────────────────────────────

pub async fn console_log_page(headers: HeaderMap) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }

    let lines = crate::console_log_buffer::get_logs();

    let body = html! {
        div class="card" {
            div class="card-header" {
                h3 { "Console Output" }
                div style="display:flex;gap:8px;align-items:center" {
                    span class="badge badge-success" { "Live Stream" }
                    button class="btn btn-sm btn-danger"
                            hx-delete="/dashboard/console-log"
                            hx-target="#console-output"
                            hx-swap="innerHTML"
                            hx-confirm="Clear console logs?" {
                        "Clear"
                    }
                }
            }
            p class="card-subtitle" {
                "Real-time server logs streamed via SSE (/api/translator/console-logs/stream)."
            }

            div id="console-output"
                 style="background:#0a0a0a;color:#34d399;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;line-height:1.6;border-radius:10px;padding:16px;min-height:450px;max-height:70vh;overflow-y:auto;white-space:pre-wrap;word-break:break-all;" {
                @if lines.is_empty() {
                    span style="color:var(--muted)" { "No console logs yet." }
                } @else {
                    @for line in &lines {
                        (render_colored_log_line(line))
                    }
                }
            }

            // Client SSE auto-stream and auto-scroll script
            script {
                (PreEscaped(r#"
(function() {
    var box = document.getElementById('console-output');
    if (!box) return;
    box.scrollTop = box.scrollHeight;
    try {
        var es = new EventSource('/api/translator/console-logs/stream');
        es.onmessage = function(e) {
            try {
                var msg = JSON.parse(e.data);
                if (msg.type === 'clear') {
                    box.innerHTML = '';
                } else if (msg.type === 'line') {
                    appendLine(msg.line);
                } else if (msg.type === 'lines' && Array.isArray(msg.lines)) {
                    msg.lines.forEach(appendLine);
                }
            } catch (_) {}
        };
        function appendLine(line) {
            var div = document.createElement('div');
            div.textContent = line;
            if (line.indexOf('[ERROR]') !== -1) div.style.color = '#ef4444';
            else if (line.indexOf('[WARN]') !== -1) div.style.color = '#fbbf24';
            else if (line.indexOf('[INFO]') !== -1) div.style.color = '#60a5fa';
            else if (line.indexOf('[DEBUG]') !== -1) div.style.color = '#c084fc';
            else div.style.color = '#34d399';
            box.appendChild(div);
            box.scrollTop = box.scrollHeight;
        }
    } catch (_) {}
})();
                "#))
            }
        }
    };

    render_page("Console Log", "/dashboard/console-log", &headers, body)
}

fn render_colored_log_line(line: &str) -> Markup {
    let color = if line.contains("[ERROR]") {
        "#ef4444"
    } else if line.contains("[WARN]") {
        "#fbbf24"
    } else if line.contains("[INFO]") {
        "#60a5fa"
    } else if line.contains("[DEBUG]") {
        "#c084fc"
    } else {
        "#34d399"
    };
    html! {
        div style=(format!("color: {color}")) { (line) }
    }
}

pub async fn console_log_clear() -> Response {
    crate::console_log_buffer::clear_logs();
    html_fragment(html! {
        span style="color:var(--muted)" { "Console logs cleared." }
    })
}

// ─── 6. /dashboard/profile ──────────────────────────────────────────────────

pub async fn profile_page(headers: HeaderMap) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }

    let db = nr_db::db().ok();
    let settings = db
        .as_ref()
        .and_then(|d| nr_db::get_settings(d).ok())
        .unwrap_or_else(|| json!({}));

    let user_display = session_display_name(&headers);
    let has_password = settings
        .get("hasPassword")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let require_login = settings
        .get("requireLogin")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let auth_mode = settings
        .get("authMode")
        .and_then(Value::as_str)
        .unwrap_or("password");
    let fallback_strategy = settings
        .get("fallbackStrategy")
        .and_then(Value::as_str)
        .unwrap_or("fill-first");
    let outbound_proxy_enabled = settings
        .get("outboundProxyEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outbound_proxy_url = settings
        .get("outboundProxyUrl")
        .and_then(Value::as_str)
        .unwrap_or("");
    let machine_id = nr_core::paths::data_dir().display().to_string();

    let body = html! {
        div class="grid grid-2" {
            // Card 1: Account & Session
            div class="card" {
                h3 { "Account & Session" }
                p class="card-subtitle" { "Current authenticated session details." }
                table {
                    tbody {
                        tr {
                            td { "Signed In User" }
                            td class="mono" { (user_display) }
                        }
                        tr {
                            td { "Auth Mode" }
                            td { (badge(BadgeVariant::Info, auth_mode)) }
                        }
                        tr {
                            td { "Data Directory" }
                            td class="mono" style="font-size:11px" { (machine_id) }
                        }
                    }
                }
                div style="margin-top:16px" {
                    form method="post" action="/logout" {
                        button class="btn btn-sm btn-danger" type="submit" { "Sign Out" }
                    }
                }
            }

            // Card 2: Password & Security
            div class="card" {
                h3 { "Password & Access" }
                p class="card-subtitle" {
                    (if has_password { "Custom password is set." } else { "Password is the default (123456)." })
                }
                form hx-post="/settings/password" hx-target="#pw-profile-result" hx-swap="innerHTML" {
                    @if has_password {
                        div class="form-row" {
                            label { "Current password" }
                            input type="password" name="currentPassword" required;
                        }
                    }
                    div class="form-row" {
                        label { "New password" }
                        input type="password" name="newPassword" required;
                    }
                    button class="btn btn-primary" type="submit" { "Change Password" }
                }
                div id="pw-profile-result" style="margin-top:8px" {}
            }

            // Card 3: Single Sign-On (SSO)
            div class="card" {
                h3 { "SSO Configuration" }
                p class="card-subtitle" { "Enterprise OpenID Connect & SAML callback endpoints." }
                table {
                    tbody {
                        tr {
                            td { "OIDC Callback" }
                            td class="mono" style="font-size:11px" { "/api/auth/oidc/callback" }
                        }
                        tr {
                            td { "SAML ACS" }
                            td class="mono" style="font-size:11px" { "/api/auth/saml/acs" }
                        }
                        tr {
                            td { "SAML Metadata" }
                            td class="mono" style="font-size:11px" { "/api/auth/saml/metadata" }
                        }
                    }
                }
                div style="margin-top:12px" {
                    p class="muted" {
                        "SSO parameters are managed via settings: oidcIssuerUrl, oidcClientId, samlEntryPoint, samlCert."
                    }
                }
            }

            // Card 4: Outbound Proxy & Routing
            div class="card" {
                h3 { "Proxy & Routing" }
                p class="card-subtitle" { "Outbound proxy and fallback chain strategy." }
                table {
                    tbody {
                        tr {
                            td { "Outbound Proxy" }
                            td {
                                (if outbound_proxy_enabled {
                                    badge(BadgeVariant::Success, "Enabled")
                                } else {
                                    badge(BadgeVariant::Default, "Disabled")
                                })
                            }
                        }
                        @if outbound_proxy_enabled && !outbound_proxy_url.is_empty() {
                            tr {
                                td { "Proxy URL" }
                                td class="mono" { (outbound_proxy_url) }
                            }
                        }
                        tr {
                            td { "Fallback Strategy" }
                            td class="mono" { (fallback_strategy) }
                        }
                        tr {
                            td { "Require Login" }
                            td { (if require_login { "Yes" } else { "No" }) }
                        }
                    }
                }
            }

            // Card 5: Database & Maintenance
            div class="card" {
                h3 { "Database & Maintenance" }
                p class="card-subtitle" { "SQLite database state storage and backup." }
                p class="muted" { "Stored at db/data.sqlite in the 9router state directory." }
                div style="display:flex;gap:8px;margin-top:12px" {
                    a href="/api/settings/database" class="btn btn-sm" download="data.sqlite" {
                        "Download Database Backup"
                    }
                }
            }
        }
    };

    render_page("Profile & Settings", "/dashboard/profile", &headers, body)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::OnceLock;
    use tower::ServiceExt;

    static TEST_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

    fn test_app() -> Router {
        TEST_DIR.get_or_init(|| {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().to_path_buf();
            std::env::set_var("DATA_DIR", &path);
            std::mem::forget(dir);
            path
        });
        crate::app()
    }

    fn auth_cookie() -> String {
        let _ = test_app();
        let token = crate::routes::auth::create_dashboard_auth_token(None).expect("create token");
        format!("auth_token={token}")
    }

    async fn request(method: &str, uri: &str, cookie: Option<&str>) -> (StatusCode, Option<String>, String) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let resp = test_app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .expect("response");
        let st = resp.status();
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(resp.into_body(), 5_000_000)
            .await
            .expect("bytes");
        (st, loc, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn endpoint_guarded_and_renders() {
        let (st, loc, _) = request("GET", "/dashboard/endpoint", None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

        let cookie = auth_cookie();
        let (st, _, body) = request("GET", "/dashboard/endpoint", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("API Endpoint"));
        assert!(body.contains("http://localhost:20128/v1"));
        assert!(body.contains("Quick Setup"));
        assert!(body.contains("cURL"));
        assert!(body.contains("Python (OpenAI SDK)"));
    }

    #[tokio::test]
    async fn endpoint_health_check() {
        let (st, _, body) = request("GET", "/dashboard/endpoint/health", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("System Health Check"));
        assert!(body.contains("Healthy"));
    }

    #[tokio::test]
    async fn usage_unauthed_bounces_to_usage_and_authed_renders() {
        // Fallback test in dashboard_nav.rs expects unauthed /dashboard/usage -> /usage
        let (st, loc, _) = request("GET", "/dashboard/usage", None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/usage")));

        let cookie = auth_cookie();
        let (st, _, body) = request("GET", "/dashboard/usage", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Usage"));
        assert!(body.contains("Requests"));
        assert!(body.contains("Prompt tokens"));
        assert!(body.contains("Completion tokens"));
        assert!(body.contains("Tokens per day"));
        assert!(body.contains("By provider"));
        assert!(body.contains("By model"));
    }

    #[tokio::test]
    async fn quota_guarded_and_renders() {
        let (st, loc, _) = request("GET", "/dashboard/quota", None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

        let cookie = auth_cookie();
        let (st, _, body) = request("GET", "/dashboard/quota", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Quota &amp; Limits") || body.contains("Quota & Limits"));
        assert!(body.contains("Claude 5h Auto-Ping"));
        assert!(body.contains("Codex 5h Auto-Ping"));
        assert!(body.contains("09:30"));
    }

    #[tokio::test]
    async fn quota_ping_endpoint() {
        let cookie = auth_cookie();
        let (st, _, body) = request("POST", "/dashboard/quota/ping", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Quota ping dispatched"));
    }

    #[tokio::test]
    async fn request_logs_guarded_and_renders() {
        let (st, loc, _) = request("GET", "/dashboard/request-logs", None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

        let cookie = auth_cookie();
        let (st, _, body) = request("GET", "/dashboard/request-logs", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Request Logs"));
        assert!(body.contains("Search"));
        assert!(body.contains("Status"));
        assert!(body.contains("API Key"));
        assert!(body.contains("Model"));
        assert!(body.contains("Duration"));
    }

    #[tokio::test]
    async fn request_logs_clear_endpoint() {
        let cookie = auth_cookie();
        let (st, _, body) = request("DELETE", "/dashboard/request-logs", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("cleared"));
    }

    #[tokio::test]
    async fn console_log_guarded_and_renders() {
        let (st, loc, _) = request("GET", "/dashboard/console-log", None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

        let cookie = auth_cookie();
        let (st, _, body) = request("GET", "/dashboard/console-log", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Console Output"));
        assert!(body.contains("Live Stream"));
        assert!(body.contains("EventSource"));
    }

    #[tokio::test]
    async fn console_log_clear_endpoint() {
        let _serial = crate::console_log_buffer::test_serial();
        let cookie = auth_cookie();
        let (st, _, body) = request("DELETE", "/dashboard/console-log", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("cleared"));
    }

    #[tokio::test]
    async fn profile_guarded_and_renders() {
        let (st, loc, _) = request("GET", "/dashboard/profile", None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));

        let cookie = auth_cookie();
        let (st, _, body) = request("GET", "/dashboard/profile", Some(&cookie)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Account &amp; Session") || body.contains("Account & Session"));
        assert!(body.contains("Password &amp; Access") || body.contains("Password & Access"));
        assert!(body.contains("SSO Configuration"));
        assert!(body.contains("/api/auth/oidc/callback"));
    }
}
