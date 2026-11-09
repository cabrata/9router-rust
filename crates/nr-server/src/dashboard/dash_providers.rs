//! Dashboard pages: providers, media-providers, proxy-pools. Absolute paths (`/dashboard/...`).
//!
//! Faithful Rust + Maud port of:
//! - `src/app/(dashboard)/dashboard/providers/**`
//! - `src/app/(dashboard)/dashboard/providers/new/**`
//! - `src/app/(dashboard)/dashboard/providers/[id]/**`
//! - `src/app/(dashboard)/dashboard/media-providers/**`
//! - `src/app/(dashboard)/dashboard/proxy-pools/**`

use axum::extract::{Path, Query, RawForm};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

use super::components::*;
use super::{guard, is_authenticated, layout, session_display_name};

// ─── Shared Utilities and Responses ──────────────────────────────────────────

fn db_conn() -> Result<nr_db::Db, String> {
    nr_db::db().map_err(|e| format!("Database error: {e}"))
}

fn html_response(markup: Markup) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        markup.into_string(),
    )
        .into_response()
}

fn err_alert(status: StatusCode, message: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        html! {
            div class="alert alert-error" { (message) }
        }
        .into_string(),
    )
        .into_response()
}

fn ok_alert(message: &str) -> Response {
    html_response(html! {
        div class="alert alert-info" { (message) }
    })
}

fn redirect_or_hx(headers: &HeaderMap, target: &str) -> Response {
    if headers.get("hx-request").is_some() {
        return (
            StatusCode::OK,
            [
                (
                    axum::http::HeaderName::from_static("hx-redirect"),
                    HeaderValue::from_str(target).unwrap_or(HeaderValue::from_static("/dashboard/providers")),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            "",
        )
            .into_response();
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, target.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
    )
        .into_response()
}

fn deleted_or_hx(headers: &HeaderMap, target: &str) -> Response {
    if headers.get("hx-request").is_some() {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            "",
        )
            .into_response();
    }
    redirect_or_hx(headers, target)
}

fn form_to_value(RawForm(bytes): RawForm) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in url::form_urlencoded::parse(&bytes) {
        map.insert(k.into_owned(), Value::String(v.into_owned()));
    }
    Value::Object(map)
}

async fn err_from(resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, 1_000_000)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let msg = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("Request failed ({})", parts.status.as_u16()));
    err_alert(parts.status, &msg)
}

fn render_page(headers: &HeaderMap, title: &str, active: &str, body: Markup) -> Response {
    if let Some(redirect) = guard(headers) {
        return redirect;
    }
    let user = session_display_name(headers);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        layout(title, active, &user, body).into_string(),
    )
        .into_response()
}

// ─── Data Access & Catalogs ──────────────────────────────────────────────────

fn ai_providers_map() -> Map<String, Value> {
    nr_data::export("src/shared/constants/providers", "AI_PROVIDERS")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn media_provider_kinds() -> Vec<Value> {
    nr_data::export("src/shared/constants/providers", "MEDIA_PROVIDER_KINDS")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn all_connections() -> Vec<Value> {
    let Ok(db) = db_conn() else { return Vec::new() };
    nr_db::get_provider_connections(&db, None).unwrap_or_default()
}

fn all_provider_nodes() -> Vec<Value> {
    let Ok(db) = db_conn() else { return Vec::new() };
    nr_db::get_provider_nodes(&db, None).unwrap_or_default()
}

fn all_proxy_pools() -> Vec<Value> {
    let Ok(db) = db_conn() else { return Vec::new() };
    nr_db::get_proxy_pools(&db, None).unwrap_or_default()
}

#[derive(Default, Clone, Debug)]
struct ProviderStats {
    connected: usize,
    error: usize,
    total: usize,
    all_disabled: bool,
}

fn calculate_provider_stats(conns: &[Value], provider_id: &str) -> ProviderStats {
    let mut stats = ProviderStats::default();
    let mut any_enabled = false;

    for c in conns {
        let p = c.get("provider").and_then(Value::as_str).unwrap_or("");
        if p == provider_id {
            stats.total += 1;
            let is_active = c.get("isActive").and_then(Value::as_bool) != Some(false);
            if is_active {
                any_enabled = true;
            }
            let test_status = c.get("testStatus").and_then(Value::as_str).unwrap_or("untested");
            if test_status == "active" || test_status == "success" {
                stats.connected += 1;
            } else if test_status == "error" || test_status == "expired" || test_status == "unavailable" {
                stats.error += 1;
            }
        }
    }

    stats.all_disabled = stats.total > 0 && !any_enabled;
    stats
}

fn provider_avatar(name: &str, color: Option<&str>) -> Markup {
    let initial = name.chars().next().unwrap_or('P');
    let bg = color.filter(|c| !c.is_empty()).unwrap_or("#6B7280");
    html! {
        span class="logo" style=(format!("width:28px;height:28px;font-size:12px;background:{bg};margin-right:8px;display:inline-flex;align-items:center;justify-content:center;border-radius:6px;color:#fff;font-weight:600;flex-shrink:0;")) {
            (initial)
        }
    }
}

fn render_status_badge(stats: &ProviderStats, no_auth: bool) -> Markup {
    if no_auth {
        return badge(BadgeVariant::Success, "Ready");
    }
    if stats.total == 0 {
        return html! { span class="muted" { "No connections" } };
    }
    if stats.all_disabled {
        return badge(BadgeVariant::Default, "Disabled");
    }
    html! {
        @if stats.connected > 0 {
            (badge(BadgeVariant::Success, &format!("{} Connected", stats.connected)))
        }
        @if stats.error > 0 {
            (badge(BadgeVariant::Danger, &format!("{} Error", stats.error)))
        }
        @if stats.connected == 0 && stats.error == 0 {
            (badge(BadgeVariant::Default, &format!("{} Added", stats.total)))
        }
    }
}

// ─── Query Parameters ────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct FilterQuery {
    pub q: Option<String>,
    pub status: Option<String>,
}

// ─── /dashboard/providers (List & Cards) ─────────────────────────────────────

pub async fn providers_list_page(headers: HeaderMap, uri: Uri, Query(query): Query<FilterQuery>) -> Response {
    // Unauthenticated fallback redirects to /providers preserving query if any, matching tests.
    if !is_authenticated(&headers) {
        let mut target = "/providers".to_string();
        if let Some(q) = uri.query() {
            target.push('?');
            target.push_str(q);
        }
        return (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, target),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
        )
            .into_response();
    }

    let search = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let filter_status = query.status.as_deref().unwrap_or("all");

    let conns = all_connections();
    let nodes = all_provider_nodes();
    let catalog = ai_providers_map();

    let oauth_keys: HashSet<String> = nr_data::export("src/shared/constants/providers", "OAUTH_PROVIDERS")
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();
    let web_cookie_keys: HashSet<String> = nr_data::export("src/shared/constants/providers", "WEB_COOKIE_PROVIDERS")
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();

    let body = html! {
        div class="providers-view" {
            // Header Controls
            div class="card" {
                div class="card-header" {
                    div {
                        h3 { "AI Provider Connections" }
                        p class="card-subtitle" { "Connect, monitor, and balance models across official and custom AI backends." }
                    }
                    div class="row-actions" {
                        a href="/dashboard/providers/new" class="btn btn-primary btn-sm" { "+ Add Provider" }
                        (modal_trigger("modal-add-openai-compatible", "btn btn-secondary btn-sm", "+ OpenAI Compatible"))
                        (modal_trigger("modal-add-anthropic-compatible", "btn btn-secondary btn-sm", "+ Anthropic Compatible"))
                    }
                }
                form method="get" action="/dashboard/providers" class="form-inline" style="margin-top:8px;" {
                    div class="form-row" style="flex:1;min-width:200px;" {
                        input type="search" name="q" value=(search) placeholder="Search providers by name or id...";
                    }
                    div class="form-row" style="min-width:140px;" {
                        select name="status" onchange="this.form.submit()" {
                            option value="all" selected[filter_status == "all"] { "All Statuses" }
                            option value="connected" selected[filter_status == "connected"] { "Connected Only" }
                            option value="error" selected[filter_status == "error"] { "Error Only" }
                            option value="noconn" selected[filter_status == "noconn"] { "No Connections" }
                        }
                    }
                    div class="form-row" {
                        button class="btn btn-secondary btn-sm" type="submit" { "Filter" }
                        @if !search.is_empty() || filter_status != "all" {
                            a href="/dashboard/providers" class="btn btn-ghost btn-sm" { "Clear" }
                        }
                    }
                }
            }

            // Custom Providers (Compatible Endpoints)
            div class="card" {
                div class="card-header" {
                    h3 { "Custom Providers (OpenAI / Anthropic Compatible)" }
                    span class="muted" { (nodes.len()) " configured" }
                }
                @if nodes.is_empty() {
                    div style="padding:16px;text-align:center;border:1px dashed var(--border);border-radius:var(--radius);" {
                        p class="muted" { "No custom providers yet. Use the buttons above to register local or third-party compatible endpoints." }
                    }
                } @else {
                    div class="grid grid-2" {
                        @for node in &nodes {
                            @let node_id = node.get("id").and_then(Value::as_str).unwrap_or("");
                            @let node_name = node.get("name").and_then(Value::as_str).unwrap_or(node_id);
                            @let node_type = node.get("type").and_then(Value::as_str).unwrap_or("custom");
                            @let stats = calculate_provider_stats(&conns, node_id);
                            div class="card" style="margin-bottom:0;" {
                                div style="display:flex;justify-content:space-between;align-items:flex-start;" {
                                    div style="display:flex;align-items:center;" {
                                        (provider_avatar(node_name, Some("#8B5CF6")))
                                        div {
                                            strong { a href=(format!("/dashboard/providers/{node_id}")) { (node_name) } }
                                            div class="muted" style="font-size:11px;" { (node_id) }
                                        }
                                    }
                                    (badge(BadgeVariant::Primary, node_type))
                                }
                                div style="margin-top:12px;display:flex;justify-content:space-between;align-items:center;" {
                                    (render_status_badge(&stats, false))
                                    div class="row-actions" {
                                        a href=(format!("/dashboard/providers/{node_id}")) class="btn btn-sm btn-secondary" { "Configure" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Configured Connections Summary Table
            @if !conns.is_empty() {
                div class="card" id="configured-connections-card" {
                    div class="card-header" {
                        h3 { "Active Connections" }
                        span class="muted" { (conns.len()) " active credentials" }
                    }
                    div style="overflow-x:auto;" {
                        table {
                            thead {
                                tr {
                                    th { "Provider" }
                                    th { "Name" }
                                    th { "Auth" }
                                    th { "Status" }
                                    th { "Default Model" }
                                    th { "Priority" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                @for c in &conns {
                                    @let c_id = c.get("id").and_then(Value::as_str).unwrap_or("");
                                    @let p_id = c.get("provider").and_then(Value::as_str).unwrap_or("");
                                    @let c_name = c.get("name").and_then(Value::as_str).unwrap_or("");
                                    @let auth_type = c.get("authType").and_then(Value::as_str).unwrap_or("apikey");
                                    @let is_act = c.get("isActive").and_then(Value::as_bool) != Some(false);
                                    @let def_model = c.get("defaultModel").and_then(Value::as_str).unwrap_or("-");
                                    @let prio = c.get("priority").and_then(Value::as_i64).unwrap_or(1);
                                    @let status = c.get("testStatus").and_then(Value::as_str).unwrap_or("untested");
                                    tr id=(format!("conn-row-{c_id}")) {
                                        td class="mono" {
                                            a href=(format!("/dashboard/providers/{p_id}")) { (p_id) }
                                        }
                                        td { (if c_name.is_empty() { p_id } else { c_name }) }
                                        td { (badge(BadgeVariant::Default, auth_type)) }
                                        td id=(format!("conn-status-{c_id}")) {
                                            @if !is_act {
                                                (badge(BadgeVariant::Default, "Disabled"))
                                            } @else if status == "active" || status == "success" {
                                                (badge(BadgeVariant::Success, "Active"))
                                            } @else if status == "error" || status == "unavailable" {
                                                (badge(BadgeVariant::Danger, "Error"))
                                            } @else {
                                                (badge(BadgeVariant::Warning, "Untested"))
                                            }
                                        }
                                        td class="mono" { (def_model) }
                                        td { (prio) }
                                        td {
                                            div class="row-actions" {
                                                button class="btn btn-sm btn-secondary" type="button"
                                                    hx-post=(format!("/dashboard/providers/conn/{c_id}/test"))
                                                    hx-target=(format!("#conn-status-{c_id}"))
                                                    hx-swap="outerHTML" {
                                                    "Test"
                                                }
                                                button class="btn btn-sm btn-ghost" type="button"
                                                    hx-post=(format!("/dashboard/providers/conn/{c_id}/toggle"))
                                                    hx-target=(format!("#conn-row-{c_id}"))
                                                    hx-swap="outerHTML" {
                                                    (if is_act { "Disable" } else { "Enable" })
                                                }
                                                button class="btn btn-sm btn-danger" type="button"
                                                    hx-delete=(format!("/dashboard/providers/conn/{c_id}"))
                                                    hx-target=(format!("#conn-row-{c_id}"))
                                                    hx-swap="outerHTML" {
                                                    "Delete"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Provider Catalog Grid
            div class="card" {
                div class="card-header" {
                    h3 { "Supported Providers Catalog" }
                    span class="muted" { (catalog.len()) " integrated providers" }
                }
                div class="grid grid-3" {
                    @for (p_id, info) in &catalog {
                        @let name = info.get("name").and_then(Value::as_str).unwrap_or(p_id);
                        @let color = info.get("color").and_then(Value::as_str);
                        @let no_auth = info.get("noAuth").and_then(Value::as_bool).unwrap_or(false);
                        @let is_hidden = info.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                        @if !is_hidden {
                            @let stats = calculate_provider_stats(&conns, p_id);
                            @let matches_search = search.is_empty() || p_id.contains(&search) || name.to_lowercase().contains(&search);
                            @let matches_filter = match filter_status {
                                "connected" => stats.connected > 0,
                                "error" => stats.error > 0,
                                "noconn" => stats.total == 0 && !no_auth,
                                _ => true,
                            };
                            @if matches_search && matches_filter {
                                @let is_oauth = oauth_keys.contains(p_id.as_str());
                                @let is_cookie = web_cookie_keys.contains(p_id.as_str());
                                div class="card" style="margin-bottom:0;display:flex;flex-direction:column;justify-content:space-between;" {
                                    div {
                                        div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:8px;" {
                                            div style="display:flex;align-items:center;min-width:0;" {
                                                (provider_avatar(name, color))
                                                div style="min-width:0;" {
                                                    div style="font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;" {
                                                        a href=(format!("/dashboard/providers/{p_id}")) { (name) }
                                                    }
                                                    div class="muted" style="font-size:11px;" { (p_id) }
                                                }
                                            }
                                            @if is_oauth {
                                                (badge(BadgeVariant::Info, "OAuth"))
                                            } @else if is_cookie {
                                                (badge(BadgeVariant::Warning, "Cookie"))
                                            }
                                        }
                                    }
                                    div style="margin-top:12px;display:flex;justify-content:space-between;align-items:center;" {
                                        (render_status_badge(&stats, no_auth))
                                        a href=(format!("/dashboard/providers/{p_id}")) class="btn btn-sm btn-secondary" { "Configure" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Modal: Add OpenAI Compatible
            (modal(
                "modal-add-openai-compatible",
                "Add OpenAI Compatible Provider",
                html! {
                    form method="post" action="/dashboard/providers/compatible" class="form" {
                        input type="hidden" name="type" value="openai-compatible";
                        div class="form-row" {
                            label for="compat-name" { "Provider Name" }
                            input type="text" id="compat-name" name="name" required placeholder="e.g. Together AI, Groq, Ollama";
                        }
                        div class="form-row" {
                            label for="compat-prefix" { "Model Prefix" }
                            input type="text" id="compat-prefix" name="prefix" required placeholder="e.g. together, local-ollama";
                        }
                        div class="form-row" {
                            label for="compat-url" { "Base URL" }
                            input type="url" id="compat-url" name="baseUrl" required placeholder="https://api.together.xyz/v1";
                        }
                        div class="form-row" {
                            label for="compat-key" { "API Key (optional)" }
                            input type="password" id="compat-key" name="apiKey" placeholder="Bearer token or API key";
                        }
                        div class="form-row" {
                            label for="compat-models" { "Model IDs (comma-separated)" }
                            input type="text" id="compat-models" name="models" placeholder="meta-llama/Llama-3-70b-chat-hf";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Create Node" }
                        }
                    }
                },
                None,
            ))

            // Modal: Add Anthropic Compatible
            (modal(
                "modal-add-anthropic-compatible",
                "Add Anthropic Compatible Provider",
                html! {
                    form method="post" action="/dashboard/providers/compatible" class="form" {
                        input type="hidden" name="type" value="anthropic-compatible";
                        div class="form-row" {
                            label for="anth-compat-name" { "Provider Name" }
                            input type="text" id="anth-compat-name" name="name" required placeholder="e.g. Bedrock Anthropic Proxy";
                        }
                        div class="form-row" {
                            label for="anth-compat-prefix" { "Model Prefix" }
                            input type="text" id="anth-compat-prefix" name="prefix" required placeholder="e.g. bedrock-claude";
                        }
                        div class="form-row" {
                            label for="anth-compat-url" { "Base URL" }
                            input type="url" id="anth-compat-url" name="baseUrl" required placeholder="https://api.anthropic.com";
                        }
                        div class="form-row" {
                            label for="anth-compat-key" { "API Key / Auth Header" }
                            input type="password" id="anth-compat-key" name="apiKey" placeholder="x-api-key or Authorization header";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Create Node" }
                        }
                    }
                },
                None,
            ))
        }
    };

    render_page(&headers, "Providers", "/dashboard/providers", body)
}

// ─── /dashboard/providers/new (Create Page) ──────────────────────────────────

pub async fn providers_new_page(headers: HeaderMap) -> Response {
    let catalog = ai_providers_map();
    let mut sorted_providers: Vec<(String, String)> = catalog
        .into_iter()
        .map(|(k, v)| {
            let name = v.get("name").and_then(Value::as_str).unwrap_or(&k).to_string();
            (k, name)
        })
        .collect();
    sorted_providers.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

    let body = html! {
        div style="max-width:680px;margin:0 auto;" {
            div style="margin-bottom:16px;" {
                a href="/dashboard/providers" class="muted" { "← Back to Providers" }
                h2 style="margin:8px 0 4px;" { "Add New Provider Connection" }
                p class="muted" { "Configure a new API key, OAuth session, or cookie-based connection." }
            }
            div class="card" {
                form method="post" action="/dashboard/providers" {
                    div class="form-row" {
                        label for="new-provider" { "AI Provider" }
                        select id="new-provider" name="provider" required {
                            @for (id, name) in &sorted_providers {
                                option value=(id) { (name) " (" (id) ")" }
                            }
                        }
                    }
                    div class="form-row" {
                        label for="new-auth-type" { "Authentication Type" }
                        select id="new-auth-type" name="authMethod" {
                            option value="api_key" selected { "API Key / Token" }
                            option value="oauth" { "OAuth Session" }
                            option value="cookie" { "Browser Session Cookie" }
                        }
                    }
                    div class="form-row" {
                        label for="new-name" { "Connection Name (optional)" }
                        input type="text" id="new-name" name="name" placeholder="e.g. Work Key, Personal Account";
                    }
                    div class="form-row" {
                        label for="new-api-key" { "API Key / Access Token / Cookie" }
                        input type="password" id="new-api-key" name="apiKey" placeholder="sk-... or session value";
                    }
                    div class="form-row" {
                        label for="new-default-model" { "Default Model (optional)" }
                        input type="text" id="new-default-model" name="defaultModel" placeholder="e.g. gpt-4o, claude-3-5-sonnet";
                    }
                    div class="form-row" {
                        label for="new-priority" { "Priority (lower numbers take precedence)" }
                        input type="number" id="new-priority" name="priority" value="1" min="1";
                    }
                    div class="form-row" style="margin-top:16px;" {
                        (toggle("isActive", true, false, Some("Enable connection immediately")))
                    }
                    div class="row-actions" style="margin-top:20px;justify-content:flex-end;" {
                        a href="/dashboard/providers" class="btn btn-secondary" { "Cancel" }
                        button type="submit" class="btn btn-primary" { "Add Provider" }
                    }
                }
            }
        }
    };

    render_page(&headers, "Add Provider", "/dashboard/providers", body)
}

// ─── /dashboard/providers/{id} (Detail & Edit Page) ──────────────────────────

pub async fn provider_detail_page(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if id == "new" {
        return providers_new_page(headers).await;
    }

    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database unavailable");
    };

    let catalog = ai_providers_map();
    let provider_info = catalog.get(&id);
    let nodes = all_provider_nodes();
    let custom_node = nodes.iter().find(|n| n.get("id").and_then(Value::as_str) == Some(&id));

    // Display title
    let provider_name = if let Some(n) = custom_node {
        n.get("name").and_then(Value::as_str).unwrap_or(&id).to_string()
    } else if let Some(p) = provider_info {
        p.get("name").and_then(Value::as_str).unwrap_or(&id).to_string()
    } else {
        id.clone()
    };

    // Connections for this provider
    let all_conns = nr_db::get_provider_connections(&db, None).unwrap_or_default();
    let conns: Vec<Value> = all_conns
        .into_iter()
        .filter(|c| {
            let p = c.get("provider").and_then(Value::as_str).unwrap_or("");
            p == id || c.get("id").and_then(Value::as_str) == Some(&id)
        })
        .collect();

    // Models catalog for this provider
    let builtin_models = nr_data::get_models_by_provider_id(&id);
    let custom_models = nr_db::get_custom_models(&db).unwrap_or_default();
    let disabled_models: HashSet<String> = nr_db::get_disabled_by_provider(&db, &id)
        .unwrap_or_default()
        .into_iter()
        .collect();

    let body = html! {
        div class="provider-detail-view" {
            // Header
            div style="margin-bottom:16px;display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:8px;" {
                div {
                    a href="/dashboard/providers" class="muted" { "← Back to Providers" }
                    h2 style="margin:4px 0 0;display:flex;align-items:center;" {
                        (provider_avatar(&provider_name, provider_info.and_then(|p| p.get("color")).and_then(Value::as_str)))
                        (provider_name)
                    }
                    div class="muted" style="font-size:12px;margin-top:2px;" {
                        "Provider ID: " code { (id) }
                        @if let Some(n) = custom_node {
                            " | Type: " (n.get("type").and_then(Value::as_str).unwrap_or("custom"))
                            " | Base URL: " (n.get("baseUrl").and_then(Value::as_str).unwrap_or("-"))
                        }
                    }
                }
                div class="row-actions" {
                    (modal_trigger("modal-add-connection", "btn btn-primary btn-sm", "+ Add Connection"))
                    (modal_trigger("modal-add-custom-model", "btn btn-secondary btn-sm", "+ Add Model"))
                }
            }

            // Connection List Card
            div class="card" {
                div class="card-header" {
                    h3 { "Connections (" (conns.len()) ")" }
                    span class="muted" { "Manage active keys, priorities and model failover routes" }
                }
                @if conns.is_empty() {
                    div style="padding:16px;text-align:center;border:1px dashed var(--border);border-radius:var(--radius);" {
                        p class="muted" { "No connections configured for this provider yet." }
                        (modal_trigger("modal-add-connection", "btn btn-primary btn-sm", "Add First Connection"))
                    }
                } @else {
                    div style="overflow-x:auto;" {
                        table {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "Auth Type" }
                                    th { "Status" }
                                    th { "Default Model" }
                                    th { "Priority" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                @for c in &conns {
                                    @let c_id = c.get("id").and_then(Value::as_str).unwrap_or("");
                                    @let c_name = c.get("name").and_then(Value::as_str).unwrap_or("");
                                    @let auth_type = c.get("authType").and_then(Value::as_str).unwrap_or("apikey");
                                    @let is_act = c.get("isActive").and_then(Value::as_bool) != Some(false);
                                    @let def_model = c.get("defaultModel").and_then(Value::as_str).unwrap_or("-");
                                    @let prio = c.get("priority").and_then(Value::as_i64).unwrap_or(1);
                                    @let status = c.get("testStatus").and_then(Value::as_str).unwrap_or("untested");
                                    tr id=(format!("conn-row-{c_id}")) {
                                        td {
                                            strong { (if c_name.is_empty() { c_id } else { c_name }) }
                                            div class="muted" style="font-size:11px;" { (c_id) }
                                        }
                                        td { (badge(BadgeVariant::Default, auth_type)) }
                                        td id=(format!("conn-status-{c_id}")) {
                                            @if !is_act {
                                                (badge(BadgeVariant::Default, "Disabled"))
                                            } @else if status == "active" || status == "success" {
                                                (badge(BadgeVariant::Success, "Active"))
                                            } @else if status == "error" || status == "unavailable" {
                                                (badge(BadgeVariant::Danger, "Error"))
                                            } @else {
                                                (badge(BadgeVariant::Warning, "Untested"))
                                            }
                                        }
                                        td class="mono" { (def_model) }
                                        td { (prio) }
                                        td {
                                            div class="row-actions" {
                                                button class="btn btn-sm btn-secondary" type="button"
                                                    hx-post=(format!("/dashboard/providers/conn/{c_id}/test"))
                                                    hx-target=(format!("#conn-status-{c_id}"))
                                                    hx-swap="outerHTML" {
                                                    "Test"
                                                }
                                                button class="btn btn-sm btn-ghost" type="button"
                                                    hx-post=(format!("/dashboard/providers/conn/{c_id}/toggle"))
                                                    hx-target=(format!("#conn-row-{c_id}"))
                                                    hx-swap="outerHTML" {
                                                    (if is_act { "Disable" } else { "Enable" })
                                                }
                                                button class="btn btn-sm btn-danger" type="button"
                                                    hx-delete=(format!("/dashboard/providers/conn/{c_id}"))
                                                    hx-target=(format!("#conn-row-{c_id}"))
                                                    hx-swap="outerHTML" {
                                                    "Delete"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Model Configuration & Catalog
            div class="card" {
                div class="card-header" {
                    h3 { "Model Catalog & Overrides" }
                    span class="muted" { (builtin_models.len() + custom_models.len()) " models registered" }
                }
                div style="overflow-x:auto;" {
                    table {
                        thead {
                            tr {
                                th { "Model ID" }
                                th { "Display Name" }
                                th { "Type" }
                                th { "Status" }
                                th { "Actions" }
                            }
                        }
                        tbody {
                            @for m in &builtin_models {
                                @let m_id = m.get("id").and_then(Value::as_str).unwrap_or("");
                                @let m_name = m.get("name").and_then(Value::as_str).unwrap_or(m_id);
                                @let is_disabled = disabled_models.contains(m_id);
                                tr id=(format!("model-row-{}", m_id.replace('/', "-"))) {
                                    td class="mono" { (m_id) }
                                    td { (m_name) }
                                    td { (badge(BadgeVariant::Default, "Built-in")) }
                                    td {
                                        @if is_disabled {
                                            (badge(BadgeVariant::Danger, "Disabled"))
                                        } @else {
                                            (badge(BadgeVariant::Success, "Active"))
                                        }
                                    }
                                    td {
                                        form method="post" action=(format!("/dashboard/providers/{id}/models/toggle")) style="display:inline;" {
                                            input type="hidden" name="modelId" value=(m_id);
                                            button type="submit" class="btn btn-sm btn-ghost" {
                                                (if is_disabled { "Enable" } else { "Disable" })
                                            }
                                        }
                                    }
                                }
                            }
                            @for m in &custom_models {
                                @let m_prov = m.get("provider").and_then(Value::as_str).unwrap_or("");
                                @if m_prov == id {
                                    @let m_id = m.get("id").and_then(Value::as_str).unwrap_or("");
                                    @let m_name = m.get("name").and_then(Value::as_str).unwrap_or(m_id);
                                    @let is_disabled = disabled_models.contains(m_id);
                                    tr id=(format!("model-row-{}", m_id.replace('/', "-"))) {
                                        td class="mono" { (m_id) }
                                        td { (m_name) }
                                        td { (badge(BadgeVariant::Primary, "Custom")) }
                                        td {
                                            @if is_disabled {
                                                (badge(BadgeVariant::Danger, "Disabled"))
                                            } @else {
                                                (badge(BadgeVariant::Success, "Active"))
                                            }
                                        }
                                        td {
                                            div class="row-actions" {
                                                form method="post" action=(format!("/dashboard/providers/{id}/models/toggle")) style="display:inline;" {
                                                    input type="hidden" name="modelId" value=(m_id);
                                                    button type="submit" class="btn btn-sm btn-ghost" {
                                                        (if is_disabled { "Enable" } else { "Disable" })
                                                    }
                                                }
                                                button class="btn btn-sm btn-danger" type="button"
                                                    hx-delete=(format!("/dashboard/providers/{id}/models/custom"))
                                                    hx-vals=(format!(r#"{{"modelId":"{m_id}"}}"#)) {
                                                    "Remove"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Modal: Add Connection
            (modal(
                "modal-add-connection",
                &format!("Add Connection for {provider_name}"),
                html! {
                    form method="post" action="/dashboard/providers" class="form" {
                        input type="hidden" name="provider" value=(id);
                        div class="form-row" {
                            label for="modal-name" { "Display Name (optional)" }
                            input type="text" id="modal-name" name="name" placeholder="e.g. Primary Account";
                        }
                        div class="form-row" {
                            label for="modal-auth-type" { "Auth Method" }
                            select id="modal-auth-type" name="authMethod" {
                                option value="api_key" selected { "API Key / Token" }
                                option value="oauth" { "OAuth Session" }
                                option value="cookie" { "Session Cookie" }
                            }
                        }
                        div class="form-row" {
                            label for="modal-key" { "API Key / Credentials" }
                            input type="password" id="modal-key" name="apiKey" required placeholder="sk-...";
                        }
                        div class="form-row" {
                            label for="modal-default-model" { "Default Model (optional)" }
                            input type="text" id="modal-default-model" name="defaultModel" placeholder="e.g. gpt-4o";
                        }
                        div class="form-row" {
                            label for="modal-priority" { "Priority" }
                            input type="number" id="modal-priority" name="priority" value="1" min="1";
                        }
                        div class="form-row" style="margin-top:16px;" {
                            (toggle("isActive", true, false, Some("Enable connection immediately")))
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Save Connection" }
                        }
                    }
                },
                None,
            ))

            // Modal: Add Custom Model
            (modal(
                "modal-add-custom-model",
                &format!("Register Custom Model for {provider_name}"),
                html! {
                    form method="post" action=(format!("/dashboard/providers/{id}/models/custom")) class="form" {
                        div class="form-row" {
                            label for="custom-model-id" { "Model ID" }
                            input type="text" id="custom-model-id" name="modelId" required placeholder="e.g. llama-3.3-70b-instruct";
                        }
                        div class="form-row" {
                            label for="custom-model-name" { "Display Name" }
                            input type="text" id="custom-model-name" name="name" required placeholder="e.g. Llama 3.3 70B Instruct";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Add Model" }
                        }
                    }
                },
                None,
            ))
        }
    };

    render_page(&headers, &provider_name, "/dashboard/providers", body)
}

// ─── /dashboard/media-providers (Hub & Kind Pages) ───────────────────────────

pub async fn media_providers_hub_page(headers: HeaderMap) -> Response {
    let kinds = media_provider_kinds();
    let conns = all_connections();
    let catalog = ai_providers_map();

    let body = html! {
        div class="media-providers-view" {
            div class="card" {
                div class="card-header" {
                    div {
                        h3 { "Media Providers" }
                        p class="card-subtitle" { "Manage text-to-speech, transcription, image generation, video and embedding services." }
                    }
                }
            }
            div class="grid grid-2" {
                @for k in &kinds {
                    @let kind_id = k.get("id").and_then(Value::as_str).unwrap_or("");
                    @let kind_label = k.get("label").and_then(Value::as_str).unwrap_or(kind_id);
                    @let endpoint = k.get("endpoint").and_then(|e| e.get("path")).and_then(Value::as_str).unwrap_or("/v1/media");
                    @let method = k.get("endpoint").and_then(|e| e.get("method")).and_then(Value::as_str).unwrap_or("POST");

                    // Count matching providers
                    @let matching_providers: Vec<_> = catalog.iter().filter(|(_, p)| {
                        let service_kinds = p.get("serviceKinds").and_then(Value::as_array);
                        if let Some(arr) = service_kinds {
                            arr.iter().any(|s| s.as_str() == Some(kind_id))
                        } else {
                            false
                        }
                    }).collect();
                    @let connected_count = matching_providers.iter().filter(|(p_id, _)| {
                        calculate_provider_stats(&conns, p_id).connected > 0
                    }).count();

                    div class="card" style="margin-bottom:0;display:flex;flex-direction:column;justify-content:space-between;" {
                        div {
                            div style="display:flex;justify-content:space-between;align-items:flex-start;" {
                                div {
                                    h3 style="margin:0 0 4px;" {
                                        a href=(format!("/dashboard/media-providers/{kind_id}")) { (kind_label) }
                                    }
                                    span class="badge badge-info mono" { (method) " " (endpoint) }
                                }
                            }
                            p class="muted" style="margin-top:8px;" {
                                (matching_providers.len()) " providers supported (" (connected_count) " connected)"
                            }
                        }
                        div style="margin-top:16px;display:flex;justify-content:flex-end;" {
                            a href=(format!("/dashboard/media-providers/{kind_id}")) class="btn btn-secondary btn-sm" {
                                "Configure " (kind_label) " →"
                            }
                        }
                    }
                }
            }
        }
    };

    render_page(&headers, "Media Providers", "/dashboard/media-providers", body)
}

pub async fn media_kind_page(headers: HeaderMap, Path(kind): Path<String>) -> Response {
    let kinds = media_provider_kinds();
    let kind_info = kinds.iter().find(|k| k.get("id").and_then(Value::as_str) == Some(&kind));

    let kind_label = kind_info
        .and_then(|k| k.get("label").and_then(Value::as_str))
        .unwrap_or(&kind);
    let endpoint = kind_info
        .and_then(|k| k.get("endpoint"))
        .and_then(|e| e.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("/v1/media");

    let conns = all_connections();
    let catalog = ai_providers_map();

    let matching_providers: Vec<_> = catalog
        .iter()
        .filter(|(_, p)| {
            let service_kinds = p.get("serviceKinds").and_then(Value::as_array);
            if let Some(arr) = service_kinds {
                arr.iter().any(|s| s.as_str() == Some(&kind))
            } else {
                false
            }
        })
        .collect();

    let body = html! {
        div class="media-kind-view" {
            div style="margin-bottom:16px;" {
                a href="/dashboard/media-providers" class="muted" { "← Back to Media Providers" }
                div style="display:flex;justify-content:space-between;align-items:center;margin-top:4px;" {
                    h2 style="margin:0;" { (kind_label) " Providers" }
                    span class="badge badge-info mono" { "POST " (endpoint) }
                }
            }
            div class="card" {
                div class="card-header" {
                    h3 { "Supported Providers (" (matching_providers.len()) ")" }
                    span class="muted" { "Choose a provider to configure keys and models" }
                }
                @if matching_providers.is_empty() {
                    p class="muted" { "No providers configured for this media kind yet." }
                } @else {
                    div class="grid grid-3" {
                        @for (p_id, p_val) in matching_providers {
                            @let p_name = p_val.get("name").and_then(Value::as_str).unwrap_or(p_id);
                            @let color = p_val.get("color").and_then(Value::as_str);
                            @let no_auth = p_val.get("noAuth").and_then(Value::as_bool).unwrap_or(false);
                            @let stats = calculate_provider_stats(&conns, p_id);
                            div class="card" style="margin-bottom:0;" {
                                div style="display:flex;align-items:center;margin-bottom:8px;" {
                                    (provider_avatar(p_name, color))
                                    div {
                                        strong { a href=(format!("/dashboard/media-providers/{kind}/{p_id}")) { (p_name) } }
                                        div class="muted" style="font-size:11px;" { (p_id) }
                                    }
                                }
                                div style="margin-top:12px;display:flex;justify-content:space-between;align-items:center;" {
                                    (render_status_badge(&stats, no_auth))
                                    a href=(format!("/dashboard/media-providers/{kind}/{p_id}")) class="btn btn-sm btn-secondary" { "Manage" }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    render_page(&headers, kind_label, "/dashboard/media-providers", body)
}

pub async fn media_provider_detail_page(headers: HeaderMap, Path((kind, id)): Path<(String, String)>) -> Response {
    let catalog = ai_providers_map();
    let provider_info = catalog.get(&id);
    let p_name = provider_info
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&id);

    let kinds = media_provider_kinds();
    let kind_info = kinds.iter().find(|k| k.get("id").and_then(Value::as_str) == Some(&kind));
    let kind_label = kind_info
        .and_then(|k| k.get("label").and_then(Value::as_str))
        .unwrap_or(&kind);
    let endpoint = kind_info
        .and_then(|k| k.get("endpoint"))
        .and_then(|e| e.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("/v1/media");

    let conns: Vec<Value> = all_connections()
        .into_iter()
        .filter(|c| c.get("provider").and_then(Value::as_str) == Some(&id))
        .collect();

    let models = nr_data::get_models_by_provider_id(&id);

    let body = html! {
        div class="media-provider-detail-view" {
            div style="margin-bottom:16px;" {
                a href=(format!("/dashboard/media-providers/{kind}")) class="muted" { "← Back to " (kind_label) }
                h2 style="margin:4px 0 0;" { (p_name) " (" (kind_label) ")" }
                div class="muted" style="margin-top:2px;" {
                    "Endpoint: " code { "POST " (endpoint) }
                }
            }

            // Connection List Card
            div class="card" {
                div class="card-header" {
                    h3 { "Active Credentials" }
                    (modal_trigger("modal-add-media-connection", "btn btn-primary btn-sm", "+ Add Credentials"))
                }
                @if conns.is_empty() {
                    p class="muted" { "No credentials configured yet for this provider. Add one to enable this service." }
                } @else {
                    table {
                        thead {
                            tr {
                                th { "Name" }
                                th { "Status" }
                                th { "Default Model" }
                                th { "Priority" }
                                th { "Actions" }
                            }
                        }
                        tbody {
                            @for c in &conns {
                                @let c_id = c.get("id").and_then(Value::as_str).unwrap_or("");
                                @let c_name = c.get("name").and_then(Value::as_str).unwrap_or(c_id);
                                @let is_act = c.get("isActive").and_then(Value::as_bool) != Some(false);
                                @let status = c.get("testStatus").and_then(Value::as_str).unwrap_or("untested");
                                tr id=(format!("media-conn-{c_id}")) {
                                    td { (c_name) }
                                    td id=(format!("media-conn-status-{c_id}")) {
                                        @if !is_act {
                                            (badge(BadgeVariant::Default, "Disabled"))
                                        } @else if status == "active" || status == "success" {
                                            (badge(BadgeVariant::Success, "Active"))
                                        } @else {
                                            (badge(BadgeVariant::Warning, status))
                                        }
                                    }
                                    td class="mono" { (c.get("defaultModel").and_then(Value::as_str).unwrap_or("-")) }
                                    td { (c.get("priority").and_then(Value::as_i64).unwrap_or(1)) }
                                    td {
                                        div class="row-actions" {
                                            button class="btn btn-sm btn-secondary" type="button"
                                                hx-post=(format!("/dashboard/providers/conn/{c_id}/test"))
                                                hx-target=(format!("#media-conn-status-{c_id}"))
                                                hx-swap="outerHTML" {
                                                "Test"
                                            }
                                            button class="btn btn-sm btn-danger" type="button"
                                                hx-delete=(format!("/dashboard/providers/conn/{c_id}"))
                                                hx-target=(format!("#media-conn-{c_id}"))
                                                hx-swap="outerHTML" {
                                                "Delete"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Available Models Card
            @if !models.is_empty() {
                div class="card" {
                    div class="card-header" {
                        h3 { "Supported Models" }
                        span class="muted" { (models.len()) " models" }
                    }
                    table {
                        thead {
                            tr {
                                th { "Model ID" }
                                th { "Name" }
                            }
                        }
                        tbody {
                            @for m in &models {
                                @let m_id = m.get("id").and_then(Value::as_str).unwrap_or("");
                                @let m_name = m.get("name").and_then(Value::as_str).unwrap_or(m_id);
                                tr {
                                    td class="mono" { (m_id) }
                                    td { (m_name) }
                                }
                            }
                        }
                    }
                }
            }

            // Usage Example Card
            div class="card" {
                div class="card-header" {
                    h3 { "API Usage Example" }
                }
                pre style="background:var(--bg-alt);padding:12px;border-radius:var(--radius);overflow-x:auto;" {
                    code {
                        "curl -X POST http://localhost:20128" (endpoint) " \\\n"
                        "  -H \"Authorization: Bearer 9router\" \\\n"
                        "  -H \"Content-Type: application/json\" \\\n"
                        "  -d '{\"model\": \"" (models.first().and_then(|m| m.get("id")).and_then(Value::as_str).unwrap_or("default")) "\"}'"
                    }
                }
            }

            // Modal: Add Connection
            (modal(
                "modal-add-media-connection",
                &format!("Add {p_name} Credentials"),
                html! {
                    form method="post" action="/dashboard/providers" class="form" {
                        input type="hidden" name="provider" value=(id);
                        div class="form-row" {
                            label for="media-name" { "Display Name (optional)" }
                            input type="text" id="media-name" name="name" placeholder="e.g. Media Production Key";
                        }
                        div class="form-row" {
                            label for="media-key" { "API Key / Credentials" }
                            input type="password" id="media-key" name="apiKey" required placeholder="sk-...";
                        }
                        div class="form-row" {
                            label for="media-default-model" { "Default Model (optional)" }
                            input type="text" id="media-default-model" name="defaultModel" placeholder="e.g. tts-1";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Save" }
                        }
                    }
                },
                None,
            ))
        }
    };

    render_page(&headers, p_name, "/dashboard/media-providers", body)
}

// ─── /dashboard/proxy-pools (Proxy List & Deploy Modals) ─────────────────────

pub async fn proxy_pools_page(headers: HeaderMap) -> Response {
    let pools = all_proxy_pools();

    let body = html! {
        div class="proxy-pools-view" {
            div class="card" {
                div class="card-header" {
                    div {
                        h3 { "Outbound Proxy Pools & Relays" }
                        p class="card-subtitle" { "Configure forward proxies and serverless relays to bypass rate limits and geographic blocks." }
                    }
                    div class="row-actions" {
                        (modal_trigger("modal-add-proxy-pool", "btn btn-primary btn-sm", "+ Add Proxy Pool"))
                        (modal_trigger("modal-deploy-vercel", "btn btn-secondary btn-sm", "Deploy Vercel"))
                        (modal_trigger("modal-deploy-cloudflare", "btn btn-secondary btn-sm", "Deploy Cloudflare"))
                        (modal_trigger("modal-deploy-deno", "btn btn-secondary btn-sm", "Deploy Deno"))
                        (modal_trigger("modal-deploy-netlify", "btn btn-secondary btn-sm", "Deploy Netlify"))
                    }
                }
            }

            div class="card" {
                div class="card-header" {
                    h3 { "Configured Proxy Pools (" (pools.len()) ")" }
                }
                @if pools.is_empty() {
                    div style="padding:16px;text-align:center;border:1px dashed var(--border);border-radius:var(--radius);" {
                        p class="muted" { "No proxy pools configured. Add a proxy or deploy a serverless relay." }
                        (modal_trigger("modal-add-proxy-pool", "btn btn-primary btn-sm", "Configure Proxy Pool"))
                    }
                } @else {
                    div style="overflow-x:auto;" {
                        table {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "Proxy URL" }
                                    th { "Bypass (No Proxy)" }
                                    th { "Type" }
                                    th { "Status" }
                                    th { "Strict" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                @for pool in &pools {
                                    @let pool_id = pool.get("id").and_then(Value::as_str).unwrap_or("");
                                    @let pool_name = pool.get("name").and_then(Value::as_str).unwrap_or(pool_id);
                                    @let proxy_url = pool.get("proxyUrl").and_then(Value::as_str).unwrap_or("");
                                    @let no_proxy = pool.get("noProxy").and_then(Value::as_str).unwrap_or("");
                                    @let p_type = pool.get("type").and_then(Value::as_str).unwrap_or("manual");
                                    @let is_act = pool.get("isActive").and_then(Value::as_bool) != Some(false);
                                    @let is_strict = pool.get("strictProxy").and_then(Value::as_bool).unwrap_or(false);
                                    tr id=(format!("pool-row-{pool_id}")) {
                                        td { strong { (pool_name) } }
                                        td class="mono" { (proxy_url) }
                                        td class="mono" { (if no_proxy.is_empty() { "-" } else { no_proxy }) }
                                        td { (badge(BadgeVariant::Default, p_type)) }
                                        td id=(format!("pool-status-{pool_id}")) {
                                            @if is_act {
                                                (badge(BadgeVariant::Success, "Active"))
                                            } @else {
                                                (badge(BadgeVariant::Default, "Disabled"))
                                            }
                                        }
                                        td { (if is_strict { "Yes" } else { "No" }) }
                                        td {
                                            div class="row-actions" {
                                                button class="btn btn-sm btn-secondary" type="button"
                                                    hx-post=(format!("/dashboard/proxy-pools/{pool_id}/test"))
                                                    hx-target=(format!("#pool-status-{pool_id}"))
                                                    hx-swap="outerHTML" {
                                                    "Test"
                                                }
                                                button class="btn btn-sm btn-danger" type="button"
                                                    hx-delete=(format!("/dashboard/proxy-pools/{pool_id}"))
                                                    hx-target=(format!("#pool-row-{pool_id}"))
                                                    hx-swap="outerHTML" {
                                                    "Delete"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Modal: Add Manual Proxy Pool
            (modal(
                "modal-add-proxy-pool",
                "Add Outbound Proxy Pool",
                html! {
                    form method="post" action="/dashboard/proxy-pools" class="form" {
                        div class="form-row" {
                            label for="pool-name" { "Pool Name" }
                            input type="text" id="pool-name" name="name" required placeholder="e.g. US Proxy Gateway";
                        }
                        div class="form-row" {
                            label for="pool-url" { "Proxy URL" }
                            input type="text" id="pool-url" name="proxyUrl" required placeholder="http://user:pass@host:port or socks5://...";
                        }
                        div class="form-row" {
                            label for="pool-no-proxy" { "No Proxy (domains to bypass, comma-separated)" }
                            input type="text" id="pool-no-proxy" name="noProxy" placeholder="localhost, 127.0.0.1, internal.domain";
                        }
                        div class="form-row" {
                            (toggle("strictProxy", false, false, Some("Strict proxy (fail request if proxy unreachable)")))
                        }
                        div class="form-row" {
                            (toggle("isActive", true, false, Some("Enable immediately")))
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Create Proxy Pool" }
                        }
                    }
                },
                None,
            ))

            // Modal: Deploy Vercel
            (modal(
                "modal-deploy-vercel",
                "Deploy Serverless Relay to Vercel",
                html! {
                    form method="post" action="/dashboard/proxy-pools/deploy/vercel" class="form" {
                        div class="form-row" {
                            label for="vercel-token" { "Vercel API Token" }
                            input type="password" id="vercel-token" name="vercelToken" required placeholder="Vercel Access Token";
                        }
                        div class="form-row" {
                            label for="vercel-project" { "Project Name (optional)" }
                            input type="text" id="vercel-project" name="projectName" placeholder="relay-vercel";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Deploy to Vercel" }
                        }
                    }
                },
                None,
            ))

            // Modal: Deploy Cloudflare
            (modal(
                "modal-deploy-cloudflare",
                "Deploy Worker Relay to Cloudflare",
                html! {
                    form method="post" action="/dashboard/proxy-pools/deploy/cloudflare" class="form" {
                        div class="form-row" {
                            label for="cf-account-id" { "Cloudflare Account ID" }
                            input type="text" id="cf-account-id" name="accountId" required placeholder="Account ID hex";
                        }
                        div class="form-row" {
                            label for="cf-api-token" { "API Token (Workers:Edit permissions)" }
                            input type="password" id="cf-api-token" name="apiToken" required placeholder="Cloudflare API Token";
                        }
                        div class="form-row" {
                            label for="cf-project" { "Worker Name (optional)" }
                            input type="text" id="cf-project" name="projectName" placeholder="relay-cloudflare";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Deploy to Cloudflare" }
                        }
                    }
                },
                None,
            ))

            // Modal: Deploy Deno
            (modal(
                "modal-deploy-deno",
                "Deploy Relay to Deno Deploy",
                html! {
                    form method="post" action="/dashboard/proxy-pools/deploy/deno" class="form" {
                        div class="form-row" {
                            label for="deno-token" { "Deno Deploy Access Token" }
                            input type="password" id="deno-token" name="denoToken" required placeholder="Deno Access Token";
                        }
                        div class="form-row" {
                            label for="deno-org" { "Organization Domain (optional)" }
                            input type="text" id="deno-org" name="orgDomain" placeholder="e.g. my-org";
                        }
                        div class="form-row" {
                            label for="deno-project" { "Project Name (optional)" }
                            input type="text" id="deno-project" name="projectName" placeholder="relay-deno";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Deploy to Deno" }
                        }
                    }
                },
                None,
            ))

            // Modal: Deploy Netlify
            (modal(
                "modal-deploy-netlify",
                "Deploy Function Relay to Netlify",
                html! {
                    form method="post" action="/dashboard/proxy-pools/deploy/netlify" class="form" {
                        div class="form-row" {
                            label for="netlify-token" { "Netlify Personal Access Token" }
                            input type="password" id="netlify-token" name="netlifyToken" required placeholder="Netlify PAT";
                        }
                        div class="form-row" {
                            label for="netlify-project" { "Site Name (optional)" }
                            input type="text" id="netlify-project" name="projectName" placeholder="relay-netlify";
                        }
                        div class="row-actions" style="margin-top:16px;justify-content:flex-end;" {
                            button type="button" class="btn btn-secondary btn-sm" data-modal-close { "Cancel" }
                            button type="submit" class="btn btn-primary btn-sm" { "Deploy to Netlify" }
                        }
                    }
                },
                None,
            ))
        }
    };

    render_page(&headers, "Proxy Pools", "/dashboard/proxy-pools", body)
}

// ─── Form Actions (POST / PUT / DELETE) ──────────────────────────────────────

pub async fn create_provider_action(headers: HeaderMap, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if let Some(prio) = obj.get("priority").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) {
            obj.insert("priority".into(), json!(prio));
        }
        if let Some(active) = obj.get("isActive").and_then(Value::as_str) {
            obj.insert("isActive".into(), Value::Bool(active == "on" || active == "true"));
        }
    }
    let resp = crate::routes::api::providers::post_providers(None, Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    redirect_or_hx(&headers, "/dashboard/providers")
}

pub async fn create_compatible_action(headers: HeaderMap, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if obj.get("apiType").is_none() || obj.get("apiType").and_then(Value::as_str) == Some("") {
            obj.insert("apiType".into(), json!("chat"));
        }
        if obj.get("prefix").is_none() || obj.get("prefix").and_then(Value::as_str) == Some("") {
            let name_slug = obj.get("name").and_then(Value::as_str).unwrap_or("custom").to_lowercase().replace(' ', "-");
            obj.insert("prefix".into(), json!(name_slug));
        }
    }
    let resp = crate::routes::api::provider_nodes::post_provider_nodes(None, Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    redirect_or_hx(&headers, "/dashboard/providers")
}

pub async fn update_provider_action(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if obj.get("apiKey").and_then(Value::as_str) == Some("") {
            obj.remove("apiKey");
        }
        if let Some(active) = obj.get("isActive").and_then(Value::as_str) {
            obj.insert("isActive".into(), Value::Bool(active == "true" || active == "on"));
        }
        if let Some(p) = obj.get("priority").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) {
            obj.insert("priority".into(), json!(p));
        }
        obj.remove("provider");
    }
    let resp = crate::routes::api::providers::put_provider_by_id(None, Path(id.clone()), Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    redirect_or_hx(&headers, &format!("/dashboard/providers/{id}"))
}

pub async fn delete_provider_action(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::providers::delete_provider_by_id(None, Path(id)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    deleted_or_hx(&headers, "/dashboard/providers")
}

pub async fn toggle_connection_action(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
    };
    if let Ok(Some(conn)) = nr_db::get_provider_connection_by_id(&db, &id) {
        let current_active = conn.get("isActive").and_then(Value::as_bool) != Some(false);
        let new_active = !current_active;
        let _ = nr_db::update_provider_connection(&db, &id, &json!({ "isActive": new_active }));
    }
    redirect_or_hx(&headers, "/dashboard/providers")
}

pub async fn toggle_provider_action(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
    };
    let conns = nr_db::get_provider_connections(&db, None).unwrap_or_default();
    let provider_conns: Vec<Value> = conns.into_iter().filter(|c| c.get("provider").and_then(Value::as_str) == Some(&id)).collect();
    let any_active = provider_conns.iter().any(|c| c.get("isActive").and_then(Value::as_bool) != Some(false));
    let new_active = !any_active;
    for c in provider_conns {
        if let Some(c_id) = c.get("id").and_then(Value::as_str) {
            let _ = nr_db::update_provider_connection(&db, c_id, &json!({ "isActive": new_active }));
        }
    }
    redirect_or_hx(&headers, &format!("/dashboard/providers/{id}"))
}

pub async fn test_connection_action(Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::providers::post_provider_test(None, Path(id)).await;
    let is_valid = resp.status() == StatusCode::OK;
    html_response(html! {
        @if is_valid {
            (badge(BadgeVariant::Success, "Active"))
        } @else {
            (badge(BadgeVariant::Danger, "Error"))
        }
    })
}

pub async fn test_provider_action(Path(id): Path<String>) -> Response {
    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
    };
    let conns = nr_db::get_provider_connections(&db, None).unwrap_or_default();
    let first_conn = conns.into_iter().find(|c| c.get("provider").and_then(Value::as_str) == Some(&id));
    if let Some(c) = first_conn {
        let conn_id = c.get("id").and_then(Value::as_str).unwrap_or("");
        let resp = crate::routes::api::providers::post_provider_test(None, Path(conn_id.to_string())).await;
        if resp.status().is_success() {
            ok_alert("Provider connection tested successfully.")
        } else {
            err_alert(StatusCode::BAD_GATEWAY, "Provider test failed.")
        }
    } else {
        ok_alert("No connection found to test for this provider.")
    }
}

pub async fn toggle_model_action(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
    };
    let body = form_to_value(form);
    let model_id = body.get("modelId").and_then(Value::as_str).unwrap_or("");
    if !model_id.is_empty() {
        let disabled: Vec<String> = nr_db::get_disabled_by_provider(&db, &id).unwrap_or_default();
        if disabled.contains(&model_id.to_string()) {
            let _ = nr_db::enable_models(&db, &id, &[model_id.to_string()]);
        } else {
            let _ = nr_db::disable_models(&db, &id, &[model_id.to_string()]);
        }
    }
    redirect_or_hx(&headers, &format!("/dashboard/providers/{id}"))
}

pub async fn add_custom_model_action(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
    };
    let body = form_to_value(form);
    let model_id = body.get("modelId").and_then(Value::as_str).unwrap_or("");
    let name = body.get("name").and_then(Value::as_str).unwrap_or(model_id);
    if !model_id.is_empty() {
        let _ = nr_db::add_custom_model(&db, &id, model_id, Some("llm"), Some(name), None, None);
    }
    redirect_or_hx(&headers, &format!("/dashboard/providers/{id}"))
}

pub async fn delete_custom_model_action(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let Ok(db) = db_conn() else {
        return err_alert(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
    };
    let body = form_to_value(form);
    let model_id = body.get("modelId").and_then(Value::as_str).unwrap_or("");
    if !model_id.is_empty() {
        let _ = nr_db::delete_custom_model(&db, &id, model_id, Some("llm"));
    }
    deleted_or_hx(&headers, &format!("/dashboard/providers/{id}"))
}

pub async fn create_proxy_pool_action(headers: HeaderMap, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if let Some(active) = obj.get("isActive").and_then(Value::as_str) {
            obj.insert("isActive".into(), Value::Bool(active == "on" || active == "true"));
        }
        if let Some(strict) = obj.get("strictProxy").and_then(Value::as_str) {
            obj.insert("strictProxy".into(), Value::Bool(strict == "on" || strict == "true"));
        }
    }
    let resp = crate::routes::api::proxy_pools::post_proxy_pools(None, Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    redirect_or_hx(&headers, "/dashboard/proxy-pools")
}

pub async fn update_proxy_pool_action(headers: HeaderMap, Path(id): Path<String>, form: RawForm) -> Response {
    let mut body = form_to_value(form);
    if let Some(obj) = body.as_object_mut() {
        if let Some(active) = obj.get("isActive").and_then(Value::as_str) {
            obj.insert("isActive".into(), Value::Bool(active == "on" || active == "true"));
        }
        if let Some(strict) = obj.get("strictProxy").and_then(Value::as_str) {
            obj.insert("strictProxy".into(), Value::Bool(strict == "on" || strict == "true"));
        }
    }
    let resp = crate::routes::api::proxy_pools::put_proxy_pool_by_id(None, Path(id), Json(body)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    redirect_or_hx(&headers, "/dashboard/proxy-pools")
}

pub async fn delete_proxy_pool_action(headers: HeaderMap, Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::proxy_pools::delete_proxy_pool_by_id(None, Path(id)).await;
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    deleted_or_hx(&headers, "/dashboard/proxy-pools")
}

pub async fn test_proxy_pool_action(Path(id): Path<String>) -> Response {
    let resp = crate::routes::api::proxy_pools::post_proxy_pool_test(None, Path(id)).await;
    let is_ok = resp.status() == StatusCode::OK;
    html_response(html! {
        @if is_ok {
            (badge(BadgeVariant::Success, "Active (200 OK)"))
        } @else {
            (badge(BadgeVariant::Danger, "Error"))
        }
    })
}

pub async fn deploy_proxy_pool_action(headers: HeaderMap, Path(target): Path<String>, form: RawForm) -> Response {
    let body = form_to_value(form);
    let resp = match target.as_str() {
        "vercel" => crate::routes::api::proxy_pools::post_proxy_pools_vercel_deploy(None, Json(body)).await,
        "cloudflare" => crate::routes::api::proxy_pools::post_proxy_pools_cloudflare_deploy(None, Json(body)).await,
        "deno" => crate::routes::api::proxy_pools::post_proxy_pools_deno_deploy(None, Json(body)).await,
        "netlify" => crate::routes::api::proxy_pools::post_proxy_pools_netlify_deploy(None, Json(body)).await,
        _ => return err_alert(StatusCode::BAD_REQUEST, "Unknown deploy provider"),
    };
    if !resp.status().is_success() {
        return err_from(resp).await;
    }
    redirect_or_hx(&headers, "/dashboard/proxy-pools")
}

// ─── Router Registration ─────────────────────────────────────────────────────

pub fn router() -> Router {
    Router::new()
        // Providers
        .route("/dashboard/providers", get(providers_list_page).post(create_provider_action))
        .route("/dashboard/providers/compatible", post(create_compatible_action))
        .route("/dashboard/providers/new", get(providers_new_page))
        .route(
            "/dashboard/providers/{id}",
            get(provider_detail_page)
                .put(update_provider_action)
                .post(update_provider_action)
                .delete(delete_provider_action),
        )
        .route("/dashboard/providers/{id}/delete", post(delete_provider_action))
        .route("/dashboard/providers/{id}/toggle", post(toggle_provider_action))
        .route("/dashboard/providers/{id}/test", post(test_provider_action))
        .route("/dashboard/providers/{id}/models/toggle", post(toggle_model_action))
        .route(
            "/dashboard/providers/{id}/models/custom",
            post(add_custom_model_action).delete(delete_custom_model_action),
        )
        .route("/dashboard/providers/conn/{id}/test", post(test_connection_action))
        .route("/dashboard/providers/conn/{id}/toggle", post(toggle_connection_action))
        .route(
            "/dashboard/providers/conn/{id}",
            delete(delete_provider_action).post(delete_provider_action),
        )
        .route("/dashboard/providers/conn/{id}/delete", post(delete_provider_action))

        // Media Providers
        .route("/dashboard/media-providers", get(media_providers_hub_page))
        .route("/dashboard/media-providers/{kind}", get(media_kind_page))
        .route("/dashboard/media-providers/{kind}/{id}", get(media_provider_detail_page))

        // Proxy Pools
        .route("/dashboard/proxy-pools", get(proxy_pools_page).post(create_proxy_pool_action))
        .route(
            "/dashboard/proxy-pools/{id}",
            put(update_proxy_pool_action)
                .post(update_proxy_pool_action)
                .delete(delete_proxy_pool_action),
        )
        .route("/dashboard/proxy-pools/{id}/delete", post(delete_proxy_pool_action))
        .route("/dashboard/proxy-pools/{id}/test", post(test_proxy_pool_action))
        .route("/dashboard/proxy-pools/deploy/{target}", post(deploy_proxy_pool_action))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();

    fn test_app() -> Router {
        DIR.get_or_init(|| {
            let td = tempfile::TempDir::new().expect("tempdir");
            std::env::set_var("DATA_DIR", td.path());
            td
        });
        super::super::router()
    }

    fn test_auth_token() -> String {
        crate::routes::auth::create_dashboard_auth_token(None).unwrap_or_else(|_| "test-token".into())
    }

    #[tokio::test]
    async fn test_unauthenticated_providers_redirects_with_query() {
        let app = test_app();
        let req = Request::builder()
            .uri("/dashboard/providers?x=1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        assert_eq!(loc, "/providers?x=1");
    }

    #[tokio::test]
    async fn test_authenticated_providers_list_renders_html() {
        let app = test_app();
        let token = test_auth_token();
        let req = Request::builder()
            .uri("/dashboard/providers")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("AI Provider Connections"));
        assert!(body.contains("Add Provider"));
        assert!(body.contains("OpenAI Compatible"));
    }

    #[tokio::test]
    async fn test_providers_new_page_renders() {
        let app = test_app();
        let token = test_auth_token();
        let req = Request::builder()
            .uri("/dashboard/providers/new")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Add New Provider Connection"));
        assert!(body.contains("AI Provider"));
    }

    #[tokio::test]
    async fn test_provider_detail_page_renders() {
        let app = test_app();
        let token = test_auth_token();
        let req = Request::builder()
            .uri("/dashboard/providers/openai")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Connections"));
        assert!(body.contains("Model Catalog &amp; Overrides"));
    }

    #[tokio::test]
    async fn test_media_providers_hub_and_kind_pages() {
        let app = test_app();
        let token = test_auth_token();

        // Hub page
        let req = Request::builder()
            .uri("/dashboard/media-providers")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Media Providers"));
        assert!(body.contains("Text To Speech"));

        // Kind page
        let app2 = test_app();
        let req2 = Request::builder()
            .uri("/dashboard/media-providers/tts")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp2 = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        let bytes2 = axum::body::to_bytes(resp2.into_body(), 1_000_000).await.unwrap();
        let body2 = String::from_utf8_lossy(&bytes2);
        assert!(body2.contains("Text To Speech"));

        // Detail page
        let app3 = test_app();
        let req3 = Request::builder()
            .uri("/dashboard/media-providers/tts/openai")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp3 = app3.oneshot(req3).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::OK);
        let bytes3 = axum::body::to_bytes(resp3.into_body(), 1_000_000).await.unwrap();
        let body3 = String::from_utf8_lossy(&bytes3);
        assert!(body3.contains("API Usage Example"));
    }

    #[tokio::test]
    async fn test_proxy_pools_page_renders() {
        let app = test_app();
        let token = test_auth_token();
        let req = Request::builder()
            .uri("/dashboard/proxy-pools")
            .header(header::COOKIE, format!("auth_token={token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Outbound Proxy Pools &amp; Relays"));
        assert!(body.contains("Deploy Vercel"));
        assert!(body.contains("Deploy Cloudflare"));
    }
}
