//! Dashboard page renderers (GET handlers).
//!
//! Each `*_page` handler renders the full shell. Edit/detail fragments are the
//! inner `.card` blocks — htmx swaps those in/out without a full reload.

use super::{guard, layout, session_display_name};
use axum::extract::Path;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use maud::{html, Markup, PreEscaped};
use serde_json::{json, Value};

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

// ─── Data accessors (reuse /api JSON shapes via nr_db) ──────────────────────

fn provider_connections() -> Vec<Value> {
    let Some(db) = nr_db::db().ok() else { return vec![] };
    nr_db::get_provider_connections(&db, None)
        .unwrap_or_default()
        .into_iter()
        .map(crate::routes::api::providers::to_provider_connection_response)
        .collect()
}

fn api_keys() -> Vec<Value> {
    let Some(db) = nr_db::db().ok() else { return vec![] };
    let keys = nr_db::get_api_keys(&db).unwrap_or_default();
    keys.iter()
        .map(|k| {
            let mut v = json!({
                "id": k.id, "key": k.key, "name": k.name, "machineId": k.machine_id,
                "isActive": k.is_active, "expiredAt": k.expired_at,
                "limitConcurrent": k.limit_concurrent, "rpm": k.rpm, "createdAt": k.created_at,
                "allowedModels": k.allowed_models, "allowedEndpoints": k.allowed_endpoints,
            });
            if let Ok(mon) = nr_db::get_api_key_monitoring(&db, &k.id) {
                v["monitoring"] = mon;
            }
            v
        })
        .collect()
}

fn combos() -> Vec<Value> {
    let Some(db) = nr_db::db().ok() else { return vec![] };
    let combos = nr_db::get_combos(&db).unwrap_or_default();
    combos.iter().map(|c| json!({
        "id": c.id, "name": c.name, "kind": c.kind,
        "models": c.models, "createdAt": c.created_at, "updatedAt": c.updated_at,
    })).collect()
}

fn settings() -> Value {
    let Some(db) = nr_db::db().ok() else { return json!({}) };
    let s = nr_db::get_settings(&db).unwrap_or_else(|_| json!({}));
    let has_password = s.get("password").and_then(Value::as_str).map(|v| !v.is_empty()).unwrap_or(false);
    let mut safe = s;
    if let Some(obj) = safe.as_object_mut() {
        obj.remove("password");
        obj.remove("oidcClientSecret");
        obj.insert("hasPassword".into(), Value::Bool(has_password));
    }
    safe
}

fn provider_catalog() -> Vec<(String, String)> {
    let Some(list) = nr_data::export("src/shared/constants/providers", "AI_PROVIDERS").and_then(Value::as_object) else {
        return vec![];
    };
    let mut items: Vec<(String, String)> = list
        .iter()
        .map(|(id, v)| (id.clone(), v.get("name").and_then(Value::as_str).unwrap_or(id).to_string()))
        .collect();
    items.sort_by(|a, b| a.1.cmp(&b.1));
    items
}

fn badge(active: bool) -> Markup {
    if active { html! { span class="badge on" { "active" } } }
    else { html! { span class="badge off" { "inactive" } } }
}

// ─── /login ─────────────────────────────────────────────────────────────────

pub async fn login_page(headers: HeaderMap) -> Response {
    if super::is_authenticated(&headers) {
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/".to_string()), (header::CACHE_CONTROL, "no-store".to_string())],
        )
            .into_response();
    }
    let settings = settings();
    let has_password = settings.get("hasPassword").and_then(Value::as_bool).unwrap_or(false);
    let error = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("login_error=")))
        .and_then(|v| urlencoding::decode(v).ok())
        .map(|v| v.into_owned())
        .unwrap_or_default();

    let body = html! {
        div class="login-wrap" {
            div class="login-card" {
                h1 { "9router" }
                p class="sub" { "Sign in to the management dashboard." }
                @if !error.is_empty() {
                    div class="alert alert-error" { (PreEscaped(html_escape(&error))) }
                }
                form method="post" action="/login" hx-post="/login" hx-target="#login-result" hx-swap="innerHTML" {
                    div class="form-row" {
                        label for="password" { "Password" }
                        input type="password" id="password" name="password" required autofocus
                            placeholder=(if has_password { "Dashboard password" } else { "Default: 123456" });
                    }
                    button class="btn btn-primary" type="submit" { "Sign in" }
                }
                div id="login-result" {}
                p class="muted" style="margin-top:16px" {
                    "Forgot password? Reset to default via the 9Router CLI → Settings → Reset Password to Default."
                }
            }
        }
    };
    page(StatusCode::OK, super::bare_layout("Sign in", body))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ─── / (status + quick links) ───────────────────────────────────────────────

pub async fn root_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Server Status", "/", |_user| {
        let conns = provider_connections();
        let keys = api_keys();
        let combos_list = combos();
        let settings_val = settings();
        let version = env!("CARGO_PKG_VERSION");
        let data_dir = nr_core::paths::data_dir().display().to_string();

        let active_conns = conns.iter().filter(|c| c.get("isActive").and_then(Value::as_bool) != Some(false)).count();
        let active_keys = keys.iter().filter(|k| k.get("isActive").and_then(Value::as_bool) != Some(false)).count();

        html! {
            div class="grid grid-3" {
                div class="card" { div class="stat-label" { "Version" } div class="stat-value mono" { "v" (version) } }
                div class="card" { div class="stat-label" { "Providers" } div class="stat-value" { (active_conns) " / " (conns.len()) } }
                div class="card" { div class="stat-label" { "API Keys" } div class="stat-value" { (active_keys) " / " (keys.len()) } }
                div class="card" { div class="stat-label" { "Combos" } div class="stat-value" { (combos_list.len()) } }
                div class="card" { div class="stat-label" { "Login required" } div class="stat-value" {
                    (if settings_val.get("requireLogin").and_then(Value::as_bool).unwrap_or(true) { "yes" } else { "no" })
                } }
                div class="card" { div class="stat-label" { "Data dir" } div class="stat-value mono" style="font-size:13px" { (data_dir) } }
            }

            div class="card" {
                h3 { "Quick links" }
                ul {
                    li { a href="/providers" { "Providers" } " — add connections for model routing" }
                    li { a href="/keys" { "API Keys" } " — create keys clients use against /v1" }
                    li { a href="/combos" { "Combos" } " — model fallback chains" }
                    li { a href="/usage" { "Usage" } " — requests, tokens and cost" }
                    li { a href="/oauth" { "OAuth" } " — connect OAuth-enabled providers" }
                    li { a href="/settings" { "Settings" } " — password, login, and more" }
                }
            }

            div class="card" {
                h3 { "OpenAI-compatible endpoint" }
                p class="muted" { "Point any OpenAI-compatible client at:" }
                pre { (PreEscaped(html_escape(&format!("POST /v1/chat/completions\nAuthorization: Bearer <api-key>")))) }
            }
        }
    })
}

// ─── /providers ─────────────────────────────────────────────────────────────

pub async fn providers_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Providers", "/providers", |_user| {
        let conns = provider_connections();
        let catalog = provider_catalog();
        html! {
            div id="providers-list" { (providers_table(&conns)) }
            div class="card" {
                h3 { "Add provider" }
                form hx-post="/providers" hx-target="#providers-list" hx-swap="outerHTML" {
                    div class="form-inline" {
                        div class="form-row" style="min-width:220px" {
                            label { "Provider" }
                            select name="provider" required {
                                @for (id, name) in &catalog { option value=(id) { (name) } }
                            }
                        }
                        div class="form-row" style="min-width:220px" {
                            label { "Display name (optional)" }
                            input type="text" name="name" placeholder="auto";
                        }
                        div class="form-row" style="min-width:280px" {
                            label { "API Key / Cookie" }
                            input type="text" name="apiKey" placeholder="sk-…" autocomplete="off";
                        }
                        div class="form-row" {
                            button class="btn btn-primary" type="submit" { "Add" }
                        }
                    }
                }
            }
        }
    })
}

/// Fragment returned to htmx after a create: the whole `#providers-list` container.
pub fn providers_list_fragment() -> String {
    html! { div id="providers-list" { (providers_table(&provider_connections())) } }
        .into_string()
}

fn providers_table(conns: &[Value]) -> Markup {
    html! {
        div class="card" {
            h3 { "Connections" }
            @if conns.is_empty() {
                p class="muted" { "No providers configured yet." }
            } @else {
                table {
                    thead { tr { th { "Provider" } th { "Name" } th { "Status" } th { "Default model" } th { "Priority" } th { "" } } }
                    tbody {
                        @for c in conns {
                            tr {
                                td class="mono" { (c.get("provider").and_then(Value::as_str).unwrap_or("")) }
                                td { (c.get("name").and_then(Value::as_str).unwrap_or("")) }
                                td { (badge(c.get("isActive").and_then(Value::as_bool) != Some(false))) }
                                td class="mono" { (c.get("defaultModel").and_then(Value::as_str).unwrap_or("—")) }
                                td { (c.get("priority").and_then(Value::as_i64).unwrap_or(1)) }
                                td class="row-actions" {
                                    a class="btn btn-sm" href=(format!("/providers/{}", c.get("id").and_then(Value::as_str).unwrap_or(""))) { "Edit" }
                                    button class="btn btn-sm btn-danger" hx-delete=(format!("/providers/{}", c.get("id").and_then(Value::as_str).unwrap_or("")))
                                        hx-confirm="Delete this provider connection?" hx-target="closest tr" hx-swap="outerHTML" { "Delete" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn provider_edit_page(headers: HeaderMap, Path(id): Path<String>) -> Response {
    render_or_login(&headers, "Edit provider", "/providers", |_user| {
        let Some(db) = nr_db::db().ok() else { return html! { div class="alert alert-error" { "Database unavailable" } } };
        let conn = nr_db::get_provider_connection_by_id(&db, &id).ok().flatten();
        match conn {
            Some(c) => {
                let safe = crate::routes::api::providers::to_provider_connection_response(c);
                html! {
                    div class="card" {
                        h3 { "Edit provider" }
                        form hx-put=(format!("/providers/{}", id)) hx-target="#edit-result" hx-swap="innerHTML" {
                            div class="form-row" { label { "Provider" } input type="text" name="provider" value=(safe.get("provider").and_then(Value::as_str).unwrap_or("")) readonly; }
                            div class="form-row" { label { "Display name" } input type="text" name="name" value=(safe.get("name").and_then(Value::as_str).unwrap_or("")); }
                            div class="form-row" { label { "New API key (leave blank to keep)" } input type="text" name="apiKey" placeholder="(unchanged)"; }
                            div class="form-row" { label { "Priority" } input type="number" name="priority" value=(safe.get("priority").and_then(Value::as_i64).unwrap_or(1)); }
                            div class="form-row" { label { "Default model" } input type="text" name="defaultModel" value=(safe.get("defaultModel").and_then(Value::as_str).unwrap_or("")); }
                            div class="form-row" {
                                label { "Active" }
                                select name="isActive" {
                                    @let active = safe.get("isActive").and_then(Value::as_bool).unwrap_or(true);
                                    option value="true" selected[active] { "Active" }
                                    option value="false" selected[!active] { "Inactive" }
                                }
                            }
                            button class="btn btn-primary" type="submit" { "Save" }
                            a class="btn" href="/providers" { "Back" }
                        }
                        div id="edit-result" {}
                    }
                }
            }
            None => html! { div class="alert alert-error" { "Provider not found" } },
        }
    })
}

// ─── /keys ──────────────────────────────────────────────────────────────────

pub async fn keys_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "API Keys", "/keys", |_user| {
        let keys = api_keys();
        html! {
            div id="keys-list" { (keys_table(&keys)) }
            div class="card" {
                h3 { "Create API key" }
                form hx-post="/keys" hx-target="#keys-list" hx-swap="outerHTML" {
                    div class="form-inline" {
                        div class="form-row" style="min-width:220px" {
                            label { "Name" }
                            input type="text" name="name" required placeholder="my-key";
                        }
                        div class="form-row" style="min-width:120px" {
                            label { "RPM limit" }
                            input type="number" name="rpm" value="0" title="0 = unlimited";
                        }
                        div class="form-row" {
                            button class="btn btn-primary" type="submit" { "Create" }
                        }
                    }
                }
            }
        }
    })
}

/// Fragment returned to htmx after a create: the whole `#keys-list` container.
pub fn keys_list_fragment() -> String {
    html! { div id="keys-list" { (keys_table(&api_keys())) } }.into_string()
}

fn keys_table(keys: &[Value]) -> Markup {
    html! {
        div class="card" {
            h3 { "Keys" }
            @if keys.is_empty() {
                p class="muted" { "No API keys yet. Create one to let clients call /v1." }
            } @else {
                table {
                    thead { tr { th { "Name" } th { "Key" } th { "Status" } th { "RPM" } th { "Last 24h" } th { "" } } }
                    tbody {
                        @for k in keys {
                            @let id = k.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            @let mon = k.get("monitoring").cloned().unwrap_or(json!({}));
                            tr {
                                td { (k.get("name").and_then(Value::as_str).unwrap_or("—")) }
                                td class="mono" { (PreEscaped(format!("{}…", &k.get("key").and_then(Value::as_str).unwrap_or("").chars().take(12).collect::<String>()))) }
                                td { (badge(k.get("isActive").and_then(Value::as_bool).unwrap_or(true))) }
                                td { (k.get("rpm").and_then(Value::as_i64).unwrap_or(0)) }
                                td {
                                    @let last24 = mon.get("requestsLast24h").and_then(Value::as_i64).unwrap_or(0);
                                    @let err24 = mon.get("errorsLast24h").and_then(Value::as_i64).unwrap_or(0);
                                    (last24) " req · " (err24) " err"
                                }
                                td class="row-actions" {
                                    a class="btn btn-sm" href=(format!("/keys/{}", id)) { "Edit" }
                                    button class="btn btn-sm btn-danger" hx-delete=(format!("/keys/{}", id))
                                        hx-confirm="Revoke this key?" hx-target="closest tr" hx-swap="outerHTML" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn key_edit_page(headers: HeaderMap, Path(id): Path<String>) -> Response {
    render_or_login(&headers, "Edit API key", "/keys", |_user| {
        let Some(db) = nr_db::db().ok() else { return html! { div class="alert alert-error" { "Database unavailable" } } };
        match nr_db::get_api_key_by_id(&db, &id).ok().flatten() {
            Some(k) => html! {
                div class="card" {
                    h3 { "Edit API key" }
                    form hx-put=(format!("/keys/{}", id)) hx-target="#edit-result" hx-swap="innerHTML" {
                        div class="form-row" { label { "Name" } input type="text" name="name" value=(k.name.unwrap_or_default()); }
                        div class="form-row" { label { "RPM limit" } input type="number" name="rpm" value=(k.rpm); }
                        div class="form-row" { label { "Concurrency limit" } input type="number" name="limitConcurrent" value=(k.limit_concurrent); }
                        div class="form-row" {
                            label { "Active" }
                            select name="isActive" {
                                option value="true" selected[k.is_active] { "Active" }
                                option value="false" selected[!k.is_active] { "Revoked" }
                            }
                        }
                        button class="btn btn-primary" type="submit" { "Save" }
                        a class="btn" href="/keys" { "Back" }
                    }
                    div id="edit-result" {}
                }
            },
            None => html! { div class="alert alert-error" { "Key not found" } },
        }
    })
}

// ─── /combos ────────────────────────────────────────────────────────────────

pub async fn combos_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Combos", "/combos", |_user| {
        let list = combos();
        html! {
            div id="combos-list" { (combos_table(&list)) }
            div class="card" {
                h3 { "Create combo" }
                p class="muted" { "A combo is an ordered list of `provider:model` entries tried in sequence." }
                form hx-post="/combos" hx-target="#combos-list" hx-swap="outerHTML" {
                    div class="form-row" {
                        label { "Name" }
                        input type="text" name="name" required placeholder="my-combo" pattern="[a-zA-Z0-9_.\\-]+";
                    }
                    div class="form-row" {
                        label { "Models (one per line: provider:model)" }
                        textarea name="models" placeholder="openai:gpt-4o\nanthropic:claude-sonnet-4-5" {}
                    }
                    button class="btn btn-primary" type="submit" { "Create" }
                }
            }
        }
    })
}

/// Fragment returned to htmx after a create: the whole `#combos-list` container.
pub fn combos_list_fragment() -> String {
    html! { div id="combos-list" { (combos_table(&combos())) } }.into_string()
}

fn combos_table(list: &[Value]) -> Markup {
    html! {
        div class="card" {
            h3 { "Combos" }
            @if list.is_empty() {
                p class="muted" { "No combos defined." }
            } @else {
                table {
                    thead { tr { th { "Name" } th { "Kind" } th { "Models" } th { "" } } }
                    tbody {
                        @for c in list {
                            @let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            @let models = c.get("models").and_then(Value::as_array).map(|a| {
                                a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" → ")
                            }).unwrap_or_default();
                            tr {
                                td class="mono" { (c.get("name").and_then(Value::as_str).unwrap_or("")) }
                                td { (c.get("kind").and_then(Value::as_str).unwrap_or("llm")) }
                                td { (PreEscaped(html_escape(&models))) }
                                td class="row-actions" {
                                    a class="btn btn-sm" href=(format!("/combos/{}", id)) { "Edit" }
                                    button class="btn btn-sm btn-danger" hx-delete=(format!("/combos/{}", id))
                                        hx-confirm="Delete this combo?" hx-target="closest tr" hx-swap="outerHTML" { "Delete" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn combo_edit_page(headers: HeaderMap, Path(id): Path<String>) -> Response {
    render_or_login(&headers, "Edit combo", "/combos", |_user| {
        let Some(db) = nr_db::db().ok() else { return html! { div class="alert alert-error" { "Database unavailable" } } };
        match nr_db::get_combo_by_id(&db, &id).ok().flatten() {
            Some(c) => {
                let models_text = c.models.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
                html! {
                    div class="card" {
                        h3 { "Edit combo" }
                        form hx-put=(format!("/combos/{}", id)) hx-target="#edit-result" hx-swap="innerHTML" {
                            div class="form-row" { label { "Name" } input type="text" name="name" value=(c.name); }
                            div class="form-row" { label { "Models (one per line)" } textarea name="models" { (PreEscaped(html_escape(&models_text))) } }
                            button class="btn btn-primary" type="submit" { "Save" }
                            a class="btn" href="/combos" { "Back" }
                        }
                        div id="edit-result" {}
                    }
                }
            }
            None => html! { div class="alert alert-error" { "Combo not found" } },
        }
    })
}

// ─── /settings ──────────────────────────────────────────────────────────────

pub async fn settings_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Settings", "/settings", |_user| {
        let s = settings();
        let get = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let get_bool = |k: &str| s.get(k).and_then(Value::as_bool).unwrap_or(false);
        let has_password = get_bool("hasPassword");
        let require_login = get_bool("requireLogin");

        html! {
            div class="grid grid-2" {
                div class="card" {
                    h3 { "Login & security" }
                    p class="muted" { "Password " (if has_password { "is set" } else { "is the default (123456)" }) "." }
                    form hx-post="/settings/password" hx-target="#pw-result" hx-swap="innerHTML" {
                        @if has_password {
                            div class="form-row" { label { "Current password" } input type="password" name="currentPassword" required; }
                        }
                        div class="form-row" { label { "New password" } input type="password" name="newPassword" required; }
                        button class="btn btn-primary" type="submit" { "Change password" }
                    }
                    div id="pw-result" {}
                    hr style="border-color:var(--border);margin:16px 0"
                    form hx-post="/settings" hx-target="#settings-result" hx-swap="innerHTML" {
                        div class="form-row" {
                            label { "Require login" }
                            select name="requireLogin" {
                                option value="true" selected[require_login] { "Yes" }
                                option value="false" selected[!require_login] { "No" }
                            }
                        }
                        button class="btn" type="submit" { "Save settings" }
                    }
                    div id="settings-result" {}
                }

                div class="card" {
                    h3 { "Preferences" }
                    form hx-post="/settings" hx-target="#settings-result" hx-swap="innerHTML" {
                        div class="form-row" { label { "Theme" } input type="text" name="theme" value=(get("theme")); }
                        div class="form-row" { label { "Language" } input type="text" name="language" value=(get("language")); }
                        button class="btn" type="submit" { "Save" }
                    }
                }
            }
        }
    })
}

// ─── /usage ─────────────────────────────────────────────────────────────────

pub async fn usage_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "Usage", "/usage", |_user| {
        let Some(db) = nr_db::db().ok() else {
            return html! { div class="alert alert-error" { "Database unavailable" } };
        };
        let stats = nr_db::get_usage_stats(&db, Some("7d")).unwrap_or_else(|_| json!({}));
        let chart = nr_db::get_chart_data(&db, Some("7d")).unwrap_or_default();

        let fmt_int = |v: &Value| v.as_i64().unwrap_or(0).to_string();
        let total_req = stats.get("totalRequests").cloned().unwrap_or(json!(0));
        let prompt = stats.get("totalPromptTokens").cloned().unwrap_or(json!(0));
        let completion = stats.get("totalCompletionTokens").cloned().unwrap_or(json!(0));
        let cost = stats.get("totalCost").and_then(Value::as_f64).unwrap_or(0.0);

        html! {
            div class="grid grid-3" {
                div class="card" { div class="stat-label" { "Requests (7d)" } div class="stat-value" { (fmt_int(&total_req)) } }
                div class="card" { div class="stat-label" { "Prompt tokens" } div class="stat-value" { (fmt_int(&prompt)) } }
                div class="card" { div class="stat-label" { "Completion tokens" } div class="stat-value" { (fmt_int(&completion)) } }
                div class="card" { div class="stat-label" { "Cost (USD)" } div class="stat-value mono" { (format!("${:.4}", cost)) } }
            }

            (usage_bar_chart(&chart))

            (usage_by_table("By provider", stats.get("byProvider"), "provider"))
            (usage_by_table("By model", stats.get("byModel"), "model"))
        }
    })
}

/// Inline horizontal bar chart of daily requests/tokens. Pure HTML — no JS chart lib.
fn usage_bar_chart(chart: &[Value]) -> Markup {
    let max = chart.iter().map(|d| d.get("tokens").and_then(Value::as_i64).unwrap_or(0)).max().unwrap_or(0).max(1);
    html! {
        div class="card" {
            h3 { "Tokens per day (7d)" }
            @if chart.is_empty() {
                p class="muted" { "No usage recorded in the last 7 days." }
            } @else {
                table {
                    @for d in chart {
                        @let label = d.get("label").and_then(Value::as_str).unwrap_or("").to_string();
                        @let tokens = d.get("tokens").and_then(Value::as_i64).unwrap_or(0);
                        @let reqs = d.get("requests").and_then(Value::as_i64).unwrap_or(0);
                        @let cost = d.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                        @let pct = (tokens as f64 / max as f64 * 100.0) as u32;
                        tr {
                            td style="width:80px" class="mono" { (label) }
                            td style="width:45%" {
                                div class="bar" { span style=(format!("width:{pct}%")) {} }
                            }
                            td class="mono" { (tokens) " tok" }
                            td class="mono muted" { (reqs) " req" }
                            td class="mono muted" { (format!("${:.4}", cost)) }
                        }
                    }
                }
            }
        }
    }
}

fn usage_by_table(title: &str, by: Option<&Value>, _key: &str) -> Markup {
    let Some(map) = by.and_then(Value::as_object) else {
        return html! { div class="card" { h3 { (title) } p class="muted" { "No data" } } };
    };
    let mut rows: Vec<(&String, i64, f64)> = map
        .iter()
        .map(|(k, v)| {
            let reqs = v.get("requests").and_then(Value::as_i64)
                .or_else(|| v.get("totalRequests").and_then(Value::as_i64))
                .unwrap_or(0);
            let cost = v.get("cost").and_then(Value::as_f64)
                .or_else(|| v.get("totalCost").and_then(Value::as_f64))
                .unwrap_or(0.0);
            (k, reqs, cost)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)));

    html! {
        div class="card" {
            h3 { (title) }
            @if rows.is_empty() {
                p class="muted" { "No data" }
            } @else {
                table {
                    thead { tr { th { "Name" } th { "Requests" } th { "Cost" } } }
                    tbody {
                        @for (name, reqs, cost) in rows {
                            tr {
                                td class="mono" { (PreEscaped(html_escape(name))) }
                                td { (reqs) }
                                td class="mono" { (format!("${:.4}", cost)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ─── /oauth ─────────────────────────────────────────────────────────────────

const OAUTH_PROVIDERS: &[&str] = &[
    "anthropic", "claude", "openai", "codex", "copilot", "gemini", "kimi",
    "minimax", "kiro", "cursor", "qoder", "windsurf", "trae", "xai",
    "perplexity", "grok", "github", "zed", "perchai", "opencode",
];

pub async fn oauth_page(headers: HeaderMap) -> Response {
    render_or_login(&headers, "OAuth", "/oauth", |_user| {
        let conns = provider_connections();
        let connected: Vec<String> = conns
            .iter()
            .filter_map(|c| c.get("provider").and_then(Value::as_str))
            .map(str::to_string)
            .collect();

        html! {
            div class="card" {
                h3 { "OAuth providers" }
                p class="muted" { "Start a device-code or browser OAuth flow. Status reflects existing provider connections." }
                table {
                    thead { tr { th { "Provider" } th { "Status" } th { "Action" } } }
                    tbody {
                        @for p in OAUTH_PROVIDERS {
                            @let is_connected = connected.iter().any(|c| c == p);
                            tr {
                                td class="mono" { (p) }
                                td { (badge(is_connected)) }
                                td class="row-actions" {
                                    button class="btn btn-sm" hx-get=(format!("/api/oauth/{}/device-code", p))
                                        hx-target=(format!("#oauth-result-{}", p)) hx-swap="innerHTML" { "Start device flow" }
                                    span id=(format!("oauth-result-{}", p)) class="mono muted" {}
                                }
                            }
                        }
                    }
                }
            }
            div class="card" {
                h3 { "Existing connections" }
                @if conns.is_empty() {
                    p class="muted" { "None" }
                } @else {
                    table {
                        thead { tr { th { "Provider" } th { "Name" } th { "Auth" } th { "Status" } } }
                        tbody {
                            @for c in conns {
                                tr {
                                    td class="mono" { (c.get("provider").and_then(Value::as_str).unwrap_or("")) }
                                    td { (c.get("name").and_then(Value::as_str).unwrap_or("")) }
                                    td { (c.get("authType").and_then(Value::as_str).unwrap_or("")) }
                                    td { (badge(c.get("isActive").and_then(Value::as_bool) != Some(false))) }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}
