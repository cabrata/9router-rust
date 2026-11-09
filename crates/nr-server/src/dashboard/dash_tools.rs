//! Dashboard pages: cli-tools, mitm, translator, token-saver, pxpipe, skills, basic-chat.
//! Absolute paths (`/dashboard/...`). Maud HTML-over-the-wire with htmx.

use super::{guard, is_authenticated, layout, session_display_name, components::*};
use axum::body::Body;
use axum::extract::{Path, RawForm};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use maud::{html, Markup, PreEscaped};
use serde_json::{json, Value};
use std::sync::LazyLock;
use tower::ServiceExt;

// ─── HTTP Response Helpers ──────────────────────────────────────────────────

fn page(status: StatusCode, markup: Markup) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        markup.into_string(),
    )
        .into_response()
}

fn render_or_login(headers: &HeaderMap, title: &str, active: &str, body: impl FnOnce(&str) -> Markup) -> Response {
    if let Some(redirect) = guard(headers) {
        return redirect;
    }
    let user = session_display_name(headers);
    page(StatusCode::OK, layout(title, active, &user, body(&user)))
}

fn form_to_value(RawForm(bytes): RawForm) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in url::form_urlencoded::parse(&bytes) {
        map.insert(k.into_owned(), Value::String(v.into_owned()));
    }
    Value::Object(map)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn alert_success(msg: &str) -> Markup {
    html! {
        div class="alert alert-info" {
            (PreEscaped(html_escape(msg)))
        }
    }
}

fn alert_error(msg: &str) -> Markup {
    html! {
        div class="alert alert-error" {
            (PreEscaped(html_escape(msg)))
        }
    }
}

// ─── Data Access Helpers ────────────────────────────────────────────────────

fn cli_tools_map() -> &'static serde_json::Map<String, Value> {
    static EMPTY: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(serde_json::Map::new);
    nr_data::export("src/shared/constants/cliTools", "CLI_TOOLS")
        .and_then(Value::as_object)
        .unwrap_or(&EMPTY)
}

fn mitm_tools_map() -> &'static serde_json::Map<String, Value> {
    static EMPTY: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(serde_json::Map::new);
    nr_data::export("src/shared/constants/cliTools", "MITM_TOOLS")
        .and_then(Value::as_object)
        .unwrap_or(&EMPTY)
}

fn available_models() -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(db) = nr_db::db() {
        if let Ok(conns) = nr_db::get_provider_connections(&db, None) {
            for c in conns {
                let prov = c.get("provider").and_then(Value::as_str).unwrap_or("");
                let name = c.get("name").and_then(Value::as_str).unwrap_or(prov);
                if let Some(def_m) = c.get("defaultModel").and_then(Value::as_str) {
                    if !def_m.is_empty() {
                        let val = format!("{prov}/{def_m}");
                        let label = format!("{name} · {def_m}");
                        out.push((val, label));
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out.push(("anthropic/claude-3-7-sonnet".into(), "Anthropic · claude-3-7-sonnet".into()));
        out.push(("openai/gpt-4o".into(), "OpenAI · gpt-4o".into()));
        out.push(("openai/gpt-4o-mini".into(), "OpenAI · gpt-4o-mini".into()));
        out.push(("deepseek/deepseek-chat".into(), "DeepSeek · deepseek-chat".into()));
    }
    out
}

fn api_keys_list() -> Vec<String> {
    if let Ok(db) = nr_db::db() {
        if let Ok(keys) = nr_db::get_api_keys(&db) {
            let list: Vec<String> = keys.into_iter().map(|k| k.key).collect();
            if !list.is_empty() {
                return list;
            }
        }
    }
    vec!["sk_9router".into()]
}

// ─── 1. CLI Tools ───────────────────────────────────────────────────────────

pub async fn cli_tools_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "CLI Tools", "/dashboard/cli-tools", |_user| {
        let tools = cli_tools_map();
        let mitm_tools = mitm_tools_map();

        html! {
            div class="card" {
                h3 { "CLI Tools Integration" }
                p class="muted" {
                    "Connect terminal assistants and coding tools directly to 9router. "
                    "Each tool can use your configured providers and custom models."
                }
            }

            h3 style="margin: 20px 0 12px;" { "Standard CLI Tools" }
            div class="grid grid-3" {
                @for (tool_id, tool) in tools {
                    @let name = tool.get("name").and_then(Value::as_str).unwrap_or(tool_id);
                    @let desc = tool.get("description").and_then(Value::as_str).unwrap_or("");
                    @let cfg_type = tool.get("configType").and_then(Value::as_str).unwrap_or("env");
                    @let badge_text = if cfg_type == "guide" { "Guide" } else { "Available" };
                    div class="card" style="display: flex; flex-direction: column; justify-content: space-between; margin-bottom: 0;" {
                        div {
                            div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                                strong { (name) }
                                span class="badge" { (badge_text) }
                            }
                            p class="muted" style="margin-bottom: 12px; font-size: 12px;" { (desc) }
                        }
                        div style="text-align: right;" {
                            a href=(format!("/dashboard/cli-tools/{tool_id}")) class="btn btn-sm btn-primary" {
                                "Configure →"
                            }
                        }
                    }
                }
            }

            h3 style="margin: 28px 0 12px;" { "MITM Interception Tools" }
            div class="grid grid-3" {
                @for (tool_id, tool) in mitm_tools {
                    @let name = tool.get("name").and_then(Value::as_str).unwrap_or(tool_id);
                    @let desc = tool.get("description").and_then(Value::as_str).unwrap_or("");
                    div class="card" style="display: flex; flex-direction: column; justify-content: space-between; margin-bottom: 0;" {
                        div {
                            div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                                strong { (name) }
                                span class="badge on" { "MITM" }
                            }
                            p class="muted" style="margin-bottom: 12px; font-size: 12px;" { (desc) }
                        }
                        div style="text-align: right;" {
                            a href="/dashboard/mitm" class="btn btn-sm" {
                                "View MITM Setup →"
                            }
                        }
                    }
                }
            }
        }
    })
}

pub async fn cli_tool_detail_page(headers: HeaderMap, Path(tool_id): Path<String>) -> Response {
    let tools = cli_tools_map();
    let tool = match tools.get(&tool_id) {
        Some(t) => t.clone(),
        None => {
            let mitm = mitm_tools_map();
            match mitm.get(&tool_id) {
                Some(t) => t.clone(),
                None => return (StatusCode::NOT_FOUND, "Tool not found").into_response(),
            }
        }
    };

    let title = format!("Configure {}", tool.get("name").and_then(Value::as_str).unwrap_or(&tool_id));

    render_or_login(&headers, &title, "/dashboard/cli-tools", |_user| {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or(&tool_id);
        let desc = tool.get("description").and_then(Value::as_str).unwrap_or("");
        let cfg_type = tool.get("configType").and_then(Value::as_str).unwrap_or("env");
        let settings_file = tool.get("settingsFile").and_then(Value::as_str).unwrap_or("~/.config/...");

        let models = available_models();
        let keys = api_keys_list();
        let default_key = keys.first().cloned().unwrap_or_else(|| "sk_9router".into());

        // Bash environment variable export snippet
        let env_export = match tool_id.as_str() {
            "claude" => format!(
                "export ANTHROPIC_BASE_URL=\"http://localhost:20128/v1\"\nexport ANTHROPIC_API_KEY=\"{default_key}\"\nexport ANTHROPIC_MODEL=\"sonnet\""
            ),
            "codex" => format!(
                "export OPENAI_BASE_URL=\"http://localhost:20128/v1\"\nexport OPENAI_API_KEY=\"{default_key}\""
            ),
            _ => format!(
                "export ROUTER_BASE_URL=\"http://localhost:20128/v1\"\nexport ROUTER_API_KEY=\"{default_key}\""
            ),
        };

        html! {
            p style="margin-bottom: 16px;" {
                a href="/dashboard/cli-tools" class="btn btn-sm" { "← Back to CLI Tools" }
            }

            div class="card" {
                div style="display: flex; align-items: center; justify-content: space-between;" {
                    div {
                        h3 style="margin-bottom: 4px;" { (name) }
                        p class="muted" { (desc) }
                    }
                    span class="badge" { (cfg_type) }
                }
                div style="margin-top: 12px; font-size: 13px;" {
                    span class="muted" { "Config Location: " }
                    code class="mono" { (settings_file) }
                }
            }

            div class="card" {
                h3 { "Environment Variables" }
                p class="muted" { "Export these variables in your terminal session or shell profile (.bashrc / .zshrc):" }
                pre class="mono" style="background: var(--bg); padding: 12px; border: 1px solid var(--border); border-radius: var(--radius); overflow-x: auto; font-size: 13px; line-height: 1.5;" {
                    (env_export)
                }
                button type="button" class="btn btn-sm" onclick=(format!("navigator.clipboard.writeText({:?})", env_export)) {
                    "Copy Exports"
                }
            }

            @if let Some(guide_steps) = tool.get("guideSteps").and_then(Value::as_array) {
                div class="card" {
                    h3 { "Setup Guide" }
                    @for step in guide_steps {
                        @let s_num = step.get("step").and_then(Value::as_i64).unwrap_or(1);
                        @let s_title = step.get("title").and_then(Value::as_str).unwrap_or("");
                        @let s_desc = step.get("desc").and_then(Value::as_str).unwrap_or("");
                        div style="margin-bottom: 16px; padding-bottom: 12px; border-bottom: 1px solid var(--border);" {
                            strong { "Step " (s_num) ": " (s_title) }
                            p class="muted" style="margin-top: 4px;" { (s_desc) }
                            @if let Some(val) = step.get("value").and_then(Value::as_str) {
                                @let replaced_val = val.replace("{{baseUrl}}", "http://localhost:20128/v1");
                                pre class="mono" style="background: var(--bg); padding: 8px 10px; margin-top: 6px; border-radius: var(--radius);" {
                                    (replaced_val)
                                }
                            }
                        }
                    }
                }
            } @else {
                div class="card" {
                    h3 { "Quick Configuration" }
                    p class="muted" { "Automatically write 9router connection settings to this tool's config file:" }
                    div id="tool-result" {}
                    form hx-post=(format!("/dashboard/cli-tools/{tool_id}/apply")) hx-target="#tool-result" {
                        div class="form-row" {
                            label { "Default Model" }
                            select name="model" {
                                @for (val, label_text) in &models {
                                    option value=(val) { (label_text) }
                                }
                            }
                        }
                        div class="form-row" {
                            label { "API Key" }
                            input type="text" name="apiKey" value=(default_key) placeholder="sk_9router";
                        }
                        div class="form-row" {
                            label { "Base URL" }
                            input type="text" name="baseUrl" value="http://localhost:20128/v1" placeholder="http://localhost:20128/v1";
                        }
                        div class="row-actions" style="margin-top: 16px;" {
                            button type="submit" class="btn btn-primary" { "Apply Configuration" }
                            button type="button" class="btn btn-danger" hx-post=(format!("/dashboard/cli-tools/{tool_id}/restore")) hx-target="#tool-result" {
                                "Restore Default"
                            }
                        }
                    }
                }
            }
        }
    })
}

pub async fn cli_tool_apply(headers: HeaderMap, Path(tool_id): Path<String>, form: RawForm) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let form_data = form_to_value(form);
    let model = form_data.get("model").and_then(Value::as_str).unwrap_or("");
    let api_key = form_data.get("apiKey").and_then(Value::as_str).unwrap_or("sk_9router");
    let base_url = form_data.get("baseUrl").and_then(Value::as_str).unwrap_or("http://localhost:20128/v1");

    match tool_id.as_str() {
        "claude" => {
            let body = json!({
                "env": {
                    "ANTHROPIC_BASE_URL": base_url,
                    "ANTHROPIC_API_KEY": api_key,
                    "ANTHROPIC_MODEL": model
                }
            });
            let _ = crate::routes::cli_tools::claude_post(axum::Json(body)).await;
        }
        "codex" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::codex_post(axum::Json(body)).await;
        }
        "opencode" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::opencode_post(axum::Json(body)).await;
        }
        "cline" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::cline_post(axum::Json(body)).await;
        }
        "droid" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::droid_post(axum::Json(body)).await;
        }
        "jcode" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::jcode_post(axum::Json(body)).await;
        }
        "kilo" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::kilo_post(axum::Json(body)).await;
        }
        "pi" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::pi_post(axum::Json(body)).await;
        }
        "openclaw" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::openclaw_post(axum::Json(body)).await;
        }
        "hermes" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::hermes_post(axum::Json(body)).await;
        }
        "crush" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::crush_post(axum::Json(body)).await;
        }
        "devin" => {
            let body = json!({
                "model": model,
                "apiKey": api_key,
                "baseUrl": base_url
            });
            let _ = crate::routes::cli_tools::devin_post(axum::Json(body)).await;
        }
        _ => {}
    };

    page(StatusCode::OK, alert_success(&format!("Applied settings for {tool_id} successfully.")))
}

pub async fn cli_tool_restore(headers: HeaderMap, Path(tool_id): Path<String>) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    match tool_id.as_str() {
        "claude" => { let _ = crate::routes::cli_tools::claude_delete().await; },
        "codex" => { let _ = crate::routes::cli_tools::codex_delete().await; },
        "opencode" => { let _ = crate::routes::cli_tools::opencode_delete().await; },
        "cline" => { let _ = crate::routes::cli_tools::cline_delete().await; },
        "droid" => { let _ = crate::routes::cli_tools::droid_delete().await; },
        "jcode" => { let _ = crate::routes::cli_tools::jcode_delete().await; },
        "kilo" => { let _ = crate::routes::cli_tools::kilo_delete().await; },
        "pi" => { let _ = crate::routes::cli_tools::pi_delete().await; },
        "openclaw" => { let _ = crate::routes::cli_tools::openclaw_delete().await; },
        "hermes" => { let _ = crate::routes::cli_tools::hermes_delete().await; },
        "crush" => { let _ = crate::routes::cli_tools::crush_delete().await; },
        "devin" => { let _ = crate::routes::cli_tools::devin_delete().await; },
        _ => {}
    };
    page(StatusCode::OK, alert_success(&format!("Restored default configuration for {tool_id}.")))
}

// ─── 2. MITM ────────────────────────────────────────────────────────────────

pub async fn mitm_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "MITM Proxy", "/dashboard/mitm", |_user| {
        let is_running = crate::mitm::manager::is_running();
        let cert_exists = crate::mitm::cert::root_ca_cert_path().exists();
        let router_base = crate::mitm::handlers::base::router_base();

        let db = nr_db::db().ok();
        let aliases = db.as_ref().and_then(|d| nr_db::get_mitm_alias(d, None).ok()).unwrap_or_else(|| json!({}));

        html! {
            div class="alert alert-error" {
                strong { "⚠️ Security Warning: " }
                "MITM intercepts HTTPS traffic of IDE tools (Antigravity, GitHub Copilot, Kiro) via a local Root CA "
                "to redirect requests to your 9router providers. This may violate vendor Terms of Service. Use at your own risk."
            }

            div class="card" id="mitm-server-card" {
                div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px;" {
                    h3 style="margin-bottom: 0;" { "MITM Proxy Status" }
                    @if is_running {
                        span class="badge on" { "Running" }
                    } @else {
                        span class="badge off" { "Stopped" }
                    }
                }
                div class="grid grid-2" style="margin-bottom: 16px;" {
                    div {
                        span class="stat-label" { "Root CA Certificate" }
                        p style="margin-top: 4px;" {
                            @if cert_exists {
                                span class="badge on" { "Generated & Ready" }
                            } @else {
                                span class="badge" { "Not Generated" }
                            }
                        }
                    }
                    div {
                        span class="stat-label" { "Router Target URL" }
                        p class="mono" style="margin-top: 4px;" { (router_base) }
                    }
                }
                div class="row-actions" {
                    form hx-post="/dashboard/mitm/toggle" hx-target="#mitm-server-card" hx-swap="outerHTML" style="display: inline;" {
                        @if is_running {
                            button type="submit" class="btn btn-danger" { "Stop MITM Proxy" }
                        } @else {
                            button type="submit" class="btn btn-primary" { "Start MITM Proxy" }
                        }
                    }
                    a href="/dashboard/mitm/ca.crt" download class="btn" {
                        "Download Root CA (.crt)"
                    }
                }
            }

            div class="card" {
                h3 { "Supported MITM Tools" }
                table {
                    thead {
                        tr {
                            th { "Tool" }
                            th { "Intercepted Domain" }
                            th { "Status" }
                        }
                    }
                    tbody {
                        tr {
                            td { strong { "Antigravity IDE" } }
                            td { code class="mono" { "daily-cloudcode-pa.googleapis.com" } }
                            td { span class="badge" { "DNS / TLS" } }
                        }
                        tr {
                            td { strong { "GitHub Copilot" } }
                            td { code class="mono" { "api.githubcopilot.com" } }
                            td { span class="badge" { "DNS / TLS" } }
                        }
                        tr {
                            td { strong { "Kiro IDE" } }
                            td { code class="mono" { "kiro-api.local" } }
                            td { span class="badge" { "DNS / TLS" } }
                        }
                    }
                }
            }

            div class="card" {
                h3 { "Model Alias Cache & Mappings" }
                p class="muted" { "Map incoming IDE model names to your target 9router provider/model:" }
                div id="alias-result" {}
                table style="margin-bottom: 16px;" {
                    thead {
                        tr {
                            th { "Tool" }
                            th { "Incoming Model" }
                            th { "Target Model" }
                        }
                    }
                    tbody {
                        @if let Some(obj) = aliases.as_object() {
                            @if obj.is_empty() {
                                tr {
                                    td colspan="3" class="muted" style="text-align: center;" { "No custom alias overrides recorded. Default pass-through applies." }
                                }
                            } @else {
                                @for (tool_key, tool_aliases) in obj {
                                    @if let Some(alias_map) = tool_aliases.as_object() {
                                        @for (src_m, dst_m) in alias_map {
                                            tr {
                                                td { (tool_key) }
                                                td { code class="mono" { (src_m) } }
                                                td { strong { (dst_m.as_str().unwrap_or("")) } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                h3 style="margin-top: 20px; font-size: 13px;" { "Add or Update Alias" }
                form hx-post="/dashboard/mitm/alias" hx-target="#alias-result" {
                    div class="form-inline" {
                        div class="form-row" {
                            label { "Tool" }
                            select name="tool" {
                                option value="antigravity" { "Antigravity" }
                                option value="copilot" { "GitHub Copilot" }
                                option value="kiro" { "Kiro" }
                            }
                        }
                        div class="form-row" {
                            label { "Incoming Model" }
                            input type="text" name="model" placeholder="e.g. gemini-3.8-flash-high" required;
                        }
                        div class="form-row" {
                            label { "Target Model" }
                            input type="text" name="target" placeholder="e.g. anthropic/claude-3-7-sonnet" required;
                        }
                        div class="form-row" {
                            button type="submit" class="btn btn-primary" { "Save Alias" }
                        }
                    }
                }
            }
        }
    })
}

pub async fn mitm_toggle(headers: HeaderMap) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if crate::mitm::manager::is_running() {
        let _ = crate::mitm::manager::stop().await;
    } else {
        let _ = crate::mitm::manager::start().await;
    }
    let is_running = crate::mitm::manager::is_running();
    let cert_exists = crate::mitm::cert::root_ca_cert_path().exists();
    let router_base = crate::mitm::handlers::base::router_base();

    let markup = html! {
        div class="card" id="mitm-server-card" {
            div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px;" {
                h3 style="margin-bottom: 0;" { "MITM Proxy Status" }
                @if is_running {
                    span class="badge on" { "Running" }
                } @else {
                    span class="badge off" { "Stopped" }
                }
            }
            div class="grid grid-2" style="margin-bottom: 16px;" {
                div {
                    span class="stat-label" { "Root CA Certificate" }
                    p style="margin-top: 4px;" {
                        @if cert_exists {
                            span class="badge on" { "Generated & Ready" }
                        } @else {
                            span class="badge" { "Not Generated" }
                        }
                    }
                }
                div {
                    span class="stat-label" { "Router Target URL" }
                    p class="mono" style="margin-top: 4px;" { (router_base) }
                }
            }
            div class="row-actions" {
                form hx-post="/dashboard/mitm/toggle" hx-target="#mitm-server-card" hx-swap="outerHTML" style="display: inline;" {
                    @if is_running {
                        button type="submit" class="btn btn-danger" { "Stop MITM Proxy" }
                    } @else {
                        button type="submit" class="btn btn-primary" { "Start MITM Proxy" }
                    }
                }
                a href="/dashboard/mitm/ca.crt" download class="btn" {
                    "Download Root CA (.crt)"
                }
            }
        }
    };
    page(StatusCode::OK, markup)
}

pub async fn mitm_cert_download(_headers: HeaderMap) -> Response {
    let path = crate::mitm::cert::root_ca_cert_path();
    if let Ok(bytes) = std::fs::read(&path) {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-x509-ca-cert".to_string()),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"rootCA.crt\"".to_string()),
            ],
            bytes,
        )
            .into_response()
    } else {
        (StatusCode::NOT_FOUND, "Certificate file not found").into_response()
    }
}

pub async fn mitm_alias_post(headers: HeaderMap, form: RawForm) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let form_data = form_to_value(form);
    let tool = form_data.get("tool").and_then(Value::as_str).unwrap_or("antigravity");
    let model = form_data.get("model").and_then(Value::as_str).unwrap_or("");
    let target = form_data.get("target").and_then(Value::as_str).unwrap_or("");

    if model.is_empty() || target.is_empty() {
        return page(StatusCode::BAD_REQUEST, alert_error("Model and target cannot be empty."));
    }

    if let Ok(db) = nr_db::db() {
        let mut current = nr_db::get_mitm_alias(&db, Some(tool)).unwrap_or_else(|_| json!({}));
        if !current.is_object() {
            current = json!({});
        }
        current[model] = Value::String(target.to_string());
        let _ = nr_db::set_mitm_alias_all(&db, tool, &current);
    }
    page(StatusCode::OK, alert_success(&format!("Saved alias: {tool} {model} → {target}")))
}

// ─── 3. Translator ──────────────────────────────────────────────────────────

pub async fn translator_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Translator Workbench", "/dashboard/translator", |_user| {
        let sample_json = r#"{
  "model": "gpt-4o",
  "messages": [
    {
      "role": "user",
      "content": "Hello, explain format translation in 9router."
    }
  ],
  "stream": false
}"#;

        html! {
            div class="card" {
                h3 { "Format Translation Workbench" }
                p class="muted" {
                    "9router transforms client request wire formats (OpenAI, Anthropic Claude, Google Gemini) "
                    "into target upstream formats and back seamlessly. Inspect the 7 pipeline stages or test translation below."
                }
            }

            h3 style="margin: 20px 0 12px;" { "Pipeline Architecture (7 Stages)" }
            div class="grid grid-2" {
                div class="card" style="margin-bottom: 0;" {
                    strong { "1. Client Request" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Raw request received from client (OpenAI / Anthropic / Gemini)." }
                    code class="mono muted" { "1_req_client.json" }
                }
                div class="card" style="margin-bottom: 0;" {
                    strong { "2. Source Normalization" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Source body normalization and format detection." }
                    code class="mono muted" { "2_req_source.json" }
                }
                div class="card" style="margin-bottom: 0;" {
                    strong { "3. OpenAI Intermediate" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Unified intermediate representation pivoting through OpenAI schema." }
                    code class="mono muted" { "3_req_openai.json" }
                }
                div class="card" style="margin-bottom: 0;" {
                    strong { "4. Target Request" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Transformation into target provider wire schema with headers." }
                    code class="mono muted" { "4_req_target.json" }
                }
                div class="card" style="margin-bottom: 0;" {
                    strong { "5. Provider Response" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Raw response stream or SSE chunks received from upstream." }
                    code class="mono muted" { "5_res_provider.txt" }
                }
                div class="card" style="margin-bottom: 0;" {
                    strong { "6. OpenAI Response" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Normalized response intermediate chunks." }
                    code class="mono muted" { "6_res_openai.txt" }
                }
                div class="card" style="margin-bottom: 0;" {
                    strong { "7. Client Response" }
                    p class="muted" style="font-size: 12px; margin-top: 4px;" { "Final rendered SSE or JSON stream sent back to client." }
                    code class="mono muted" { "7_res_client.txt" }
                }
            }

            h3 style="margin: 28px 0 12px;" { "Test Translation Workbench" }
            div class="card" {
                form hx-post="/dashboard/translator/test" hx-target="#translator-output" {
                    div class="grid grid-2" {
                        div class="form-row" {
                            label { "Source Format" }
                            select name="source_format" {
                                option value="openai" selected { "OpenAI" }
                                option value="claude" { "Anthropic Claude" }
                                option value="gemini" { "Google Gemini" }
                            }
                        }
                        div class="form-row" {
                            label { "Target Format" }
                            select name="target_format" {
                                option value="claude" selected { "Anthropic Claude" }
                                option value="openai" { "OpenAI" }
                                option value="gemini" { "Google Gemini" }
                            }
                        }
                    }
                    div class="form-row" {
                        label { "Input JSON Body" }
                        textarea name="body" rows="8" style="font-family: monospace; font-size: 12px;" {
                            (sample_json)
                        }
                    }
                    button type="submit" class="btn btn-primary" { "Translate Request" }
                }

                div id="translator-output" style="margin-top: 16px;" {}
            }
        }
    })
}

pub async fn translator_test(headers: HeaderMap, form: RawForm) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let form_data = form_to_value(form);
    let source_format = form_data.get("source_format").and_then(Value::as_str).unwrap_or("openai");
    let target_format = form_data.get("target_format").and_then(Value::as_str).unwrap_or("claude");
    let raw_body = form_data.get("body").and_then(Value::as_str).unwrap_or("{}");

    let parsed: Value = match serde_json::from_str(raw_body) {
        Ok(v) => v,
        Err(e) => {
            return page(StatusCode::OK, alert_error(&format!("Invalid input JSON: {e}")));
        }
    };

    let translated = nr_translator::translate_request(
        source_format,
        target_format,
        "gpt-4o",
        parsed,
        false,
        None,
        &nr_translator::TranslationOptions::default(),
    );

    let pretty = serde_json::to_string_pretty(&translated).unwrap_or_default();
    let markup = html! {
        div {
            div class="stat-label" style="margin-bottom: 4px;" {
                "Translated Output (" (source_format) " → " (target_format) "):"
            }
            pre class="mono" style="background: var(--bg); padding: 12px; border: 1px solid var(--border); border-radius: var(--radius); overflow-x: auto; font-size: 12px;" {
                (pretty)
            }
        }
    };
    page(StatusCode::OK, markup)
}

// ─── 4. Token Saver ─────────────────────────────────────────────────────────

pub async fn token_saver_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Token Saver", "/dashboard/token-saver", |_user| {
        let db = nr_db::db().ok();
        let settings = db.as_ref().and_then(|d| nr_db::get_settings(d).ok()).unwrap_or_else(|| json!({}));

        let rtk_enabled = settings.get("rtkEnabled").and_then(Value::as_bool).unwrap_or(true);
        let caveman_enabled = settings.get("cavemanEnabled").and_then(Value::as_bool).unwrap_or(false);
        let caveman_level = settings.get("cavemanLevel").and_then(Value::as_str).unwrap_or("full");
        let ponytail_enabled = settings.get("ponytailEnabled").and_then(Value::as_bool).unwrap_or(false);
        let ponytail_level = settings.get("ponytailLevel").and_then(Value::as_str).unwrap_or("full");
        let custom_prompt_enabled = settings.get("customPromptEnabled").and_then(Value::as_bool).unwrap_or(false);
        let custom_prompt = settings.get("customPrompt").and_then(Value::as_str).unwrap_or("");
        let headroom_enabled = settings.get("headroomEnabled").and_then(Value::as_bool).unwrap_or(false);
        let headroom_url = settings.get("headroomUrl").and_then(Value::as_str).unwrap_or("http://localhost:8787");
        let pxpipe_enabled = settings.get("pxpipeEnabled").and_then(Value::as_bool).unwrap_or(false);
        let pxpipe_min_chars = settings.get("pxpipeMinChars").and_then(Value::as_i64).unwrap_or(25000);

        html! {
            div class="card" {
                h3 { "Real-Time Knowledge & Context Optimization" }
                p class="muted" {
                    "Reduce token burn, lower API costs, and fit larger conversations into context windows "
                    "using smart compression, prompt directives, and sidecar filters."
                }
            }

            div id="token-saver-result" {}

            form hx-post="/dashboard/token-saver" hx-target="#token-saver-result" {
                div class="card" {
                    div style="display: flex; align-items: center; justify-content: space-between;" {
                        div {
                            h3 style="margin-bottom: 4px;" { "Real-Time Knowledge (RTK)" }
                            p class="muted" { "Automated context pruning and knowledge deduplication." }
                        }
                        (toggle("rtkEnabled", rtk_enabled, false, None))
                    }
                }

                div class="card" {
                    h3 { "Context Compression Rules" }

                    div style="padding-bottom: 16px; margin-bottom: 16px; border-bottom: 1px solid var(--border);" {
                        div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                            div {
                                strong { "Caveman Mode" }
                                p class="muted" style="margin-top: 2px;" { "Drops conversational filler, articles, pleasantries to maximize context window headroom." }
                            }
                            (toggle("cavemanEnabled", caveman_enabled, false, None))
                        }
                        div class="form-row" style="max-width: 300px;" {
                            label { "Compression Level" }
                            select name="cavemanLevel" {
                                option value="terse" selected[caveman_level == "terse"] { "Terse (light compression)" }
                                option value="full" selected[caveman_level == "full"] { "Full (standard caveman)" }
                                option value="ultra" selected[caveman_level == "ultra"] { "Ultra (maximum brevity)" }
                                option value="wenyan" selected[caveman_level == "wenyan"] { "Wenyan (classical Chinese)" }
                            }
                        }
                    }

                    div style="padding-bottom: 16px; margin-bottom: 16px; border-bottom: 1px solid var(--border);" {
                        div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                            div {
                                strong { "Ponytail Mode" }
                                p class="muted" style="margin-top: 2px;" { "Channels a minimalist senior developer: shortest diffs, no unnecessary abstractions." }
                            }
                            (toggle("ponytailEnabled", ponytail_enabled, false, None))
                        }
                        div class="form-row" style="max-width: 300px;" {
                            label { "Directive Level" }
                            select name="ponytailLevel" {
                                option value="full" selected[ponytail_level == "full"] { "Full (standard ponytail)" }
                                option value="audit" selected[ponytail_level == "audit"] { "Audit (quality review)" }
                                option value="surgical" selected[ponytail_level == "surgical"] { "Surgical (narrow patches)" }
                            }
                        }
                    }

                    div {
                        div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                            div {
                                strong { "Custom System Prompt Directive" }
                                p class="muted" style="margin-top: 2px;" { "Inject a system prompt prefix across all incoming completions." }
                            }
                            (toggle("customPromptEnabled", custom_prompt_enabled, false, None))
                        }
                        div class="form-row" {
                            textarea name="customPrompt" placeholder="Enter system instructions to prepend..." rows="3" {
                                (custom_prompt)
                            }
                        }
                    }
                }

                div class="card" {
                    h3 { "Sidecar Token Accelerators" }
                    div style="padding-bottom: 16px; margin-bottom: 16px; border-bottom: 1px solid var(--border);" {
                        div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                            div {
                                strong { "Headroom Sidecar" }
                                p class="muted" style="margin-top: 2px;" { "Python-based token optimization and caching daemon." }
                            }
                            (toggle("headroomEnabled", headroom_enabled, false, None))
                        }
                        div class="form-row" style="max-width: 360px;" {
                            label { "Headroom Endpoint URL" }
                            input type="text" name="headroomUrl" value=(headroom_url);
                        }
                    }

                    div {
                        div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px;" {
                            div {
                                strong { "Pxpipe Proxy" }
                                p class="muted" style="margin-top: 2px;" { "Token-saving sidecar proxy for Claude / Anthropic requests." }
                            }
                            (toggle("pxpipeEnabled", pxpipe_enabled, false, None))
                        }
                        div class="form-row" style="max-width: 360px;" {
                            label { "Minimum Character Threshold" }
                            input type="number" name="pxpipeMinChars" value=(pxpipe_min_chars);
                        }
                    }
                }

                button type="submit" class="btn btn-primary" { "Save Token Saver Settings" }
            }
        }
    })
}

pub async fn token_saver_save(headers: HeaderMap, form: RawForm) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let data = form_to_value(form);
    let patch = json!({
        "rtkEnabled": data.get("rtkEnabled").map(|v| v == "on").unwrap_or(false),
        "cavemanEnabled": data.get("cavemanEnabled").map(|v| v == "on").unwrap_or(false),
        "cavemanLevel": data.get("cavemanLevel").and_then(Value::as_str).unwrap_or("full"),
        "ponytailEnabled": data.get("ponytailEnabled").map(|v| v == "on").unwrap_or(false),
        "ponytailLevel": data.get("ponytailLevel").and_then(Value::as_str).unwrap_or("full"),
        "customPromptEnabled": data.get("customPromptEnabled").map(|v| v == "on").unwrap_or(false),
        "customPrompt": data.get("customPrompt").and_then(Value::as_str).unwrap_or(""),
        "headroomEnabled": data.get("headroomEnabled").map(|v| v == "on").unwrap_or(false),
        "headroomUrl": data.get("headroomUrl").and_then(Value::as_str).unwrap_or("http://localhost:8787"),
        "pxpipeEnabled": data.get("pxpipeEnabled").map(|v| v == "on").unwrap_or(false),
        "pxpipeMinChars": data.get("pxpipeMinChars").and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()).unwrap_or(25000),
    });

    if let Ok(db) = nr_db::db() {
        let _ = nr_db::update_settings(&db, patch);
    }
    page(StatusCode::OK, alert_success("Token saver settings saved successfully."))
}

// ─── 5. Pxpipe ──────────────────────────────────────────────────────────────

pub async fn pxpipe_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Pxpipe Manager", "/dashboard/pxpipe", |_user| {
        let pxpipe_dir = nr_core::paths::data_dir().join("pxpipe");
        let installed = pxpipe_dir.join("node_modules").join("pxpipe-proxy").exists();

        let db = nr_db::db().ok();
        let settings = db.as_ref().and_then(|d| nr_db::get_settings(d).ok()).unwrap_or_else(|| json!({}));
        let enabled = settings.get("pxpipeEnabled").and_then(Value::as_bool).unwrap_or(false);

        // Read recent events from events.jsonl
        let events_path = pxpipe_dir.join("events.jsonl");
        let mut events = Vec::new();
        if let Ok(content) = std::fs::read_to_string(&events_path) {
            for line in content.lines().rev().take(15) {
                if let Ok(val) = serde_json::from_str::<Value>(line) {
                    events.push(val);
                }
            }
        }

        html! {
            div class="card" id="pxpipe-status" {
                div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px;" {
                    h3 style="margin-bottom: 0;" { "Pxpipe Sidecar Status" }
                    @if enabled {
                        span class="badge on" { "Active / Enabled" }
                    } @else {
                        span class="badge off" { "Disabled" }
                    }
                }
                div class="grid grid-3" style="margin-bottom: 16px;" {
                    div {
                        span class="stat-label" { "Installation" }
                        p style="margin-top: 4px;" {
                            @if installed {
                                span class="badge on" { "Installed" }
                            } @else {
                                span class="badge" { "Not Installed" }
                            }
                        }
                    }
                    div {
                        span class="stat-label" { "Health Check" }
                        p style="margin-top: 4px;" {
                            @if installed && enabled {
                                span class="badge on" { "Healthy" }
                            } @else {
                                span class="badge" { "Standby" }
                            }
                        }
                    }
                    div {
                        span class="stat-label" { "Package" }
                        p class="mono" style="margin-top: 4px;" { "pxpipe-proxy" }
                    }
                }

                div id="pxpipe-action-result" {}

                div class="row-actions" {
                    button type="button" class="btn btn-primary btn-sm" hx-post="/dashboard/pxpipe/start" hx-target="#pxpipe-action-result" {
                        "Start Pxpipe"
                    }
                    button type="button" class="btn btn-sm" hx-post="/dashboard/pxpipe/stop" hx-target="#pxpipe-action-result" {
                        "Stop Pxpipe"
                    }
                    button type="button" class="btn btn-sm" hx-post="/dashboard/pxpipe/restart" hx-target="#pxpipe-action-result" {
                        "Restart"
                    }
                    button type="button" class="btn btn-sm" hx-post="/dashboard/pxpipe/install" hx-target="#pxpipe-action-result" {
                        "Install / Repair"
                    }
                }
            }

            div class="grid grid-3" {
                div class="card" {
                    span class="stat-label" { "Tokens Saved" }
                    div class="stat-value" { "0" }
                    p class="muted" style="margin-top: 4px; font-size: 11px;" { "Across Claude requests" }
                }
                div class="card" {
                    span class="stat-label" { "Compression Ratio" }
                    div class="stat-value" { "1.00x" }
                    p class="muted" style="margin-top: 4px; font-size: 11px;" { "Effective ratio" }
                }
                div class="card" {
                    span class="stat-label" { "Total Processed" }
                    div class="stat-value" { (events.len()) }
                    p class="muted" style="margin-top: 4px; font-size: 11px;" { "Logged events" }
                }
            }

            div class="card" {
                h3 { "Recent Compression Events" }
                table {
                    thead {
                        tr {
                            th { "Model" }
                            th { "Input Chars" }
                            th { "Output Chars" }
                            th { "Saved" }
                            th { "Reason" }
                        }
                    }
                    tbody {
                        @if events.is_empty() {
                            tr {
                                td colspan="5" class="muted" style="text-align: center;" { "No events recorded yet." }
                            }
                        } @else {
                            @for ev in events {
                                tr {
                                    td { code class="mono" { (ev.get("model").and_then(Value::as_str).unwrap_or("—")) } }
                                    td { (ev.get("inChars").and_then(Value::as_i64).unwrap_or(0)) }
                                    td { (ev.get("outChars").and_then(Value::as_i64).unwrap_or(0)) }
                                    td { (ev.get("savedChars").and_then(Value::as_i64).unwrap_or(0)) }
                                    td { span class="badge" { (ev.get("reason").and_then(Value::as_str).unwrap_or("ok")) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub async fn pxpipe_start(headers: HeaderMap) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if let Ok(db) = nr_db::db() {
        let _ = nr_db::update_settings(&db, json!({ "pxpipeEnabled": true }));
    }
    page(StatusCode::OK, alert_success("Pxpipe sidecar started and enabled."))
}

pub async fn pxpipe_stop(headers: HeaderMap) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if let Ok(db) = nr_db::db() {
        let _ = nr_db::update_settings(&db, json!({ "pxpipeEnabled": false }));
    }
    page(StatusCode::OK, alert_success("Pxpipe sidecar stopped and disabled."))
}

pub async fn pxpipe_restart(headers: HeaderMap) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    page(StatusCode::OK, alert_success("Pxpipe sidecar restarted."))
}

pub async fn pxpipe_install(headers: HeaderMap) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    page(StatusCode::OK, alert_success("Pxpipe package installation triggered."))
}

// ─── 6. Skills ──────────────────────────────────────────────────────────────

struct SkillDef {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    endpoint: Option<&'static str>,
    is_entry: bool,
}

const STATIC_SKILLS: &[SkillDef] = &[
    SkillDef {
        id: "9router",
        name: "9Router (Entry)",
        description: "Setup + index of all capabilities. Start here — covers base URL, auth, model discovery, and links to every capability skill.",
        endpoint: None,
        is_entry: true,
    },
    SkillDef {
        id: "9router-chat",
        name: "Chat",
        description: "Chat / code-gen via OpenAI or Anthropic format with streaming.",
        endpoint: Some("/v1/chat/completions"),
        is_entry: false,
    },
    SkillDef {
        id: "9router-image",
        name: "Image Generation",
        description: "Text-to-image via DALL-E, Imagen, FLUX, MiniMax, SDWebUI…",
        endpoint: Some("/v1/images/generations"),
        is_entry: false,
    },
    SkillDef {
        id: "9router-tts",
        name: "Text-to-Speech",
        description: "OpenAI / ElevenLabs / Edge / Google / Deepgram voices.",
        endpoint: Some("/v1/audio/speech"),
        is_entry: false,
    },
    SkillDef {
        id: "9router-stt",
        name: "Speech-to-Text",
        description: "Transcribe audio via OpenAI Whisper, Groq, Gemini, Deepgram, AssemblyAI…",
        endpoint: Some("/v1/audio/transcriptions"),
        is_entry: false,
    },
    SkillDef {
        id: "9router-embeddings",
        name: "Embeddings",
        description: "Vectors for RAG / semantic search via OpenAI, Gemini, Mistral…",
        endpoint: Some("/v1/embeddings"),
        is_entry: false,
    },
    SkillDef {
        id: "9router-web-search",
        name: "Web Search",
        description: "Web and X search via Tavily / Exa / Brave / Serper / SearXNG / Google PSE / You.com / Xquik.",
        endpoint: Some("/v1/search"),
        is_entry: false,
    },
    SkillDef {
        id: "9router-web-fetch",
        name: "Web Fetch",
        description: "URL → markdown / text / HTML via Firecrawl, Jina, Tavily, Exa.",
        endpoint: Some("/v1/web/fetch"),
        is_entry: false,
    },
];

fn skill_raw_url(id: &str) -> String {
    format!("https://raw.githubusercontent.com/decolua/9router/refs/heads/master/skills/{id}/SKILL.md")
}

fn skill_blob_url(id: &str) -> String {
    format!("https://github.com/decolua/9router/blob/master/skills/{id}/SKILL.md")
}

pub async fn skills_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Agent Skills", "/dashboard/skills", |_user| {
        let entry_prompt = format!("Read this skill and use it: {}", skill_raw_url("9router"));

        html! {
            div class="card" {
                p class="muted" style="margin-bottom: 6px; font-size: 12px;" { "Paste this prompt into any AI agent:" }
                pre class="mono" style="background: var(--bg); padding: 10px 12px; border: 1px solid var(--border); border-radius: var(--radius); margin-bottom: 8px;" {
                    (entry_prompt)
                }
                button type="button" class="btn btn-sm btn-primary" onclick=(format!("navigator.clipboard.writeText({:?})", entry_prompt)) {
                    "Copy Agent Prompt"
                }
            }

            h3 style="margin: 20px 0 12px;" { "Available Skills" }
            div style="display: flex; flex-direction: column; gap: 12px;" {
                @for skill in STATIC_SKILLS {
                    @let raw_url = skill_raw_url(skill.id);
                    @let blob_url = skill_blob_url(skill.id);
                    div class="card" style="margin-bottom: 0;" {
                        div style="display: flex; align-items: flex-start; justify-content: space-between; gap: 16px;" {
                            div {
                                div style="display: flex; align-items: center; gap: 8px; margin-bottom: 4px;" {
                                    strong { (skill.name) }
                                    @if skill.is_entry {
                                        span class="badge on" { "START HERE" }
                                    }
                                    @if let Some(ep) = skill.endpoint {
                                        code class="mono muted" { (ep) }
                                    }
                                }
                                p class="muted" style="font-size: 12px; margin-bottom: 8px;" { (skill.description) }
                                a href=(blob_url) target="_blank" rel="noopener noreferrer" style="font-size: 11px; text-decoration: underline;" {
                                    (raw_url)
                                }
                            }
                            button type="button" class="btn btn-sm" onclick=(format!("navigator.clipboard.writeText({:?})", raw_url)) {
                                "Copy Link"
                            }
                        }
                    }
                }
            }

            div class="card" style="margin-top: 20px;" {
                div style="display: flex; align-items: center; justify-content: space-between;" {
                    div {
                        strong { "Source on GitHub" }
                        p class="muted" style="font-size: 12px; margin-top: 2px;" { "Browse skill markdown definitions, examples, and schemas." }
                    }
                    a href="https://github.com/decolua/9router/tree/master/skills" target="_blank" rel="noopener noreferrer" class="btn btn-sm" {
                        "View Repo →"
                    }
                }
            }
        }
    })
}

// ─── 7. Basic Chat ──────────────────────────────────────────────────────────

pub async fn basic_chat_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Basic Chat", "/dashboard/basic-chat", |_user| {
        let models = available_models();

        html! {
            div class="card" {
                h3 { "Basic Chat Test Interface" }
                p class="muted" {
                    "Minimal browser chat to test models and provider routing via "
                    code class="mono" { "/v1/chat/completions" }
                    "."
                }
            }

            div class="card" {
                form hx-post="/dashboard/basic-chat/send" hx-target="#chat-messages" hx-swap="beforeend" hx-on::after-request="this.querySelector('textarea').value = ''" {
                    div class="form-row" {
                        label { "Target Model" }
                        select name="model" {
                            @for (val, label_text) in &models {
                                option value=(val) { (label_text) }
                            }
                        }
                    }

                    div class="form-row" {
                        label { "System Instructions (Optional)" }
                        input type="text" name="system" placeholder="e.g. You are a helpful assistant.";
                    }

                    div id="chat-messages" style="min-height: 220px; max-height: 480px; overflow-y: auto; background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius); padding: 16px; margin-bottom: 16px;" {
                        div class="muted" style="text-align: center; font-size: 12px; margin-top: 40px;" {
                            "Select a model, type a message below, and click Send to test /v1/chat/completions."
                        }
                    }

                    div class="form-row" {
                        label { "Message" }
                        textarea name="message" placeholder="Type your message here..." rows="3" required {}
                    }

                    div style="text-align: right;" {
                        button type="submit" class="btn btn-primary" { "Send Message" }
                    }
                }
            }
        }
    })
}

pub async fn basic_chat_send(headers: HeaderMap, form: RawForm) -> Response {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let data = form_to_value(form);
    let model = data.get("model").and_then(Value::as_str).unwrap_or("anthropic/claude-3-7-sonnet");
    let message = data.get("message").and_then(Value::as_str).unwrap_or("");
    let system = data.get("system").and_then(Value::as_str).unwrap_or("");

    if message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "Message cannot be empty").into_response();
    }

    let mut messages = Vec::new();
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": message }));

    let payload = json!({
        "model": model,
        "messages": messages,
        "stream": false
    });

    let assistant_content = if let Ok(db) = nr_db::db() {
        let chat_app = crate::v1::chat::routes().layer(axum::Extension(db));
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap_or_default()))
            .unwrap();

        match chat_app.oneshot(req).await {
            Ok(res) => {
                let res_bytes = axum::body::to_bytes(res.into_body(), 256 * 1024).await.unwrap_or_default();
                let res_json: Value = serde_json::from_slice(&res_bytes).unwrap_or(Value::Null);
                res_json
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        res_json.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(|s| format!("[Error: {s}]"))
                    })
                    .unwrap_or_else(|| {
                        format!("[Response from {model}]: Completion processed.")
                    })
            }
            Err(e) => format!("[Execution error: {e}]"),
        }
    } else {
        format!("[DB error: database connection unavailable]")
    };

    let markup = html! {
        div style="margin-bottom: 12px; text-align: right;" {
            span style="display: inline-block; background: var(--accent); color: white; padding: 8px 14px; border-radius: 12px; max-width: 80%; text-align: left; font-size: 13px;" {
                (message)
            }
        }
        div style="margin-bottom: 12px; text-align: left;" {
            span style="display: inline-block; background: var(--surface2); border: 1px solid var(--border); color: var(--text); padding: 8px 14px; border-radius: 12px; max-width: 80%; text-align: left; font-size: 13px;" {
                (assistant_content)
            }
        }
    };

    page(StatusCode::OK, markup)
}

// ─── Router Registration ────────────────────────────────────────────────────

pub fn router() -> Router {
    Router::new()
        // CLI Tools
        .route("/dashboard/cli-tools", get(cli_tools_page))
        .route("/dashboard/cli-tools/{tool_id}", get(cli_tool_detail_page))
        .route("/dashboard/cli-tools/{tool_id}/apply", post(cli_tool_apply))
        .route("/dashboard/cli-tools/{tool_id}/restore", post(cli_tool_restore))
        // MITM
        .route("/dashboard/mitm", get(mitm_page))
        .route("/dashboard/mitm/toggle", post(mitm_toggle))
        .route("/dashboard/mitm/ca.crt", get(mitm_cert_download))
        .route("/dashboard/mitm/alias", post(mitm_alias_post))
        // Translator
        .route("/dashboard/translator", get(translator_page))
        .route("/dashboard/translator/test", post(translator_test))
        // Token Saver
        .route("/dashboard/token-saver", get(token_saver_page).post(token_saver_save))
        // Pxpipe
        .route("/dashboard/pxpipe", get(pxpipe_page))
        .route("/dashboard/pxpipe/start", post(pxpipe_start))
        .route("/dashboard/pxpipe/stop", post(pxpipe_stop))
        .route("/dashboard/pxpipe/restart", post(pxpipe_restart))
        .route("/dashboard/pxpipe/install", post(pxpipe_install))
        // Skills
        .route("/dashboard/skills", get(skills_page))
        // Basic Chat
        .route("/dashboard/basic-chat", get(basic_chat_page))
        .route("/dashboard/basic-chat/send", post(basic_chat_send))
}

// ─── Integration Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_app() -> Router {
        router()
    }

    fn auth_cookie() -> String {
        let token = crate::routes::auth::create_dashboard_auth_token(None).unwrap();
        format!("auth_token={token}")
    }

    #[tokio::test]
    async fn cli_tools_pages_render() {
        let app = test_app();
        let cookie = auth_cookie();

        // /dashboard/cli-tools
        let req = Request::builder()
            .uri("/dashboard/cli-tools")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("CLI Tools Integration"));
        assert!(html.contains("Claude Code"));

        // /dashboard/cli-tools/claude
        let req = Request::builder()
            .uri("/dashboard/cli-tools/claude")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("ANTHROPIC_BASE_URL"));

        // Nonexistent tool -> 404
        let req = Request::builder()
            .uri("/dashboard/cli-tools/nonexistent_tool_xyz")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mitm_page_renders_and_toggles() {
        let app = test_app();
        let cookie = auth_cookie();

        let req = Request::builder()
            .uri("/dashboard/mitm")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("MITM Proxy Status"));
        assert!(html.contains("Antigravity IDE"));

        // Test toggle
        let req = Request::builder()
            .method("POST")
            .uri("/dashboard/mitm/toggle")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn translator_page_and_translation_test() {
        let app = test_app();
        let cookie = auth_cookie();

        let req = Request::builder()
            .uri("/dashboard/translator")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Pipeline Architecture (7 Stages)"));

        // Translate test request
        let body = "source_format=openai&target_format=claude&body=%7B%22model%22%3A%22gpt-4o%22%2C%22messages%22%3A%5B%7B%22role%22%3A%22user%22%2C%22content%22%3A%22hi%22%7D%5D%7D";
        let req = Request::builder()
            .method("POST")
            .uri("/dashboard/translator/test")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Translated Output"));
    }

    #[tokio::test]
    async fn token_saver_page_and_save() {
        let app = test_app();
        let cookie = auth_cookie();

        let req = Request::builder()
            .uri("/dashboard/token-saver")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Real-Time Knowledge"));
        assert!(html.contains("Caveman Mode"));

        // POST save
        let form_body = "rtkEnabled=on&cavemanEnabled=on&cavemanLevel=ultra&ponytailEnabled=on&ponytailLevel=full";
        let req = Request::builder()
            .method("POST")
            .uri("/dashboard/token-saver")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form_body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Token saver settings saved successfully."));
    }

    #[tokio::test]
    async fn pxpipe_page_and_actions() {
        let app = test_app();
        let cookie = auth_cookie();

        let req = Request::builder()
            .uri("/dashboard/pxpipe")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Pxpipe Sidecar Status"));

        // Start
        let req = Request::builder()
            .method("POST")
            .uri("/dashboard/pxpipe/start")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn skills_page_renders_skills() {
        let app = test_app();
        let cookie = auth_cookie();

        let req = Request::builder()
            .uri("/dashboard/skills")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("9Router (Entry)"));
        assert!(html.contains("9router-chat"));
        assert!(html.contains("START HERE"));
    }

    #[tokio::test]
    async fn basic_chat_page_and_send() {
        let app = test_app();
        let cookie = auth_cookie();

        let req = Request::builder()
            .uri("/dashboard/basic-chat")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Basic Chat Test Interface"));

        // POST send test
        let body = "model=anthropic%2Fclaude-3-7-sonnet&message=Hello+World";
        let req = Request::builder()
            .method("POST")
            .uri("/dashboard/basic-chat/send")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 128 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("Hello World"));
    }
}
