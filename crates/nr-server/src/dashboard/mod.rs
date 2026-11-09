//! HTML dashboard served as HTML-over-the-wire (maud) with htmx for interactivity.
//!
//! Port of the dashboard surface of `src/app/(dashboard)/**` + `src/app/login/page.js`.
//! The pages consume exactly the JSON shapes the JS frontend reads from `/api/*`
//! (`connections`, `keys`, `combos`, settings, `usage/stats`, `usage/chart`); data is
//! read through `nr_db` (the same repos the `/api` handlers use) so the rendered rows
//! are identical to what `/api/providers`, `/api/keys`, … return.

pub mod assets;
pub mod forms;
pub mod pages;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use maud::{html, Markup, PreEscaped};
use serde_json::Value;

// ─── Auth guard ─────────────────────────────────────────────────────────────

/// Session check shared by the dashboard guard. Port of `src/dashboardGuard.js`:
/// when `requireLogin` is set, an unauthenticated browser is redirected to `/login`.
pub fn is_authenticated(headers: &HeaderMap) -> bool {
    let db = match nr_db::db() {
        Ok(db) => db,
        Err(_) => return false,
    };
    let settings = nr_db::get_settings(&db).unwrap_or_else(|_| serde_json::json!({}));
    if settings.get("requireLogin").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("auth_token=")));
    match token {
        Some(t) => crate::routes::auth::verify_dashboard_auth_token(t).is_some(),
        None => false,
    }
}

/// `Some(redirect)` when the request must bounce to `/login`, else `None`.
fn guard(headers: &HeaderMap) -> Option<Response> {
    if is_authenticated(headers) {
        return None;
    }
    Some((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/login".to_string()), (header::CACHE_CONTROL, "no-store".to_string())],
    )
        .into_response())
}

fn session_display_name(headers: &HeaderMap) -> String {
    let db = match nr_db::db() {
        Ok(db) => db,
        Err(_) => return String::new(),
    };
    let settings = nr_db::get_settings(&db).unwrap_or_else(|_| serde_json::json!({}));
    if settings.get("requireLogin").and_then(Value::as_bool) == Some(false) {
        return "anonymous".into();
    }
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("auth_token=")))
        .and_then(crate::routes::auth::verify_dashboard_auth_token);
    match token {
        Some(claims) => claims
            .oidc_name
            .or(claims.oidc_email)
            .or(claims.saml_name)
            .or(claims.saml_email)
            .unwrap_or_else(|| "authenticated".into()),
        None => "anonymous".into(),
    }
}

// ─── Layout ─────────────────────────────────────────────────────────────────

const NAV_ITEMS: &[(&str, &str)] = &[
    ("/", "Status"),
    ("/providers", "Providers"),
    ("/keys", "API Keys"),
    ("/combos", "Combos"),
    ("/usage", "Usage"),
    ("/oauth", "OAuth"),
    ("/settings", "Settings"),
];

fn nav(active: &str) -> Markup {
    html! {
        aside class="sidebar" {
            div class="brand" {
                h1 { "9router" }
                div class="ver" { (PreEscaped(concat!("rust · v", env!("CARGO_PKG_VERSION")))) }
            }
            ul class="nav" {
                @for (href, label) in NAV_ITEMS {
                    li { a href=(href) class=(if *href == active { "active" } else { "" }) { (label) } }
                }
            }
        }
    }
}

/// Standard dashboard shell. `active` is one of `NAV_ITEMS` paths.
pub fn layout(title: &str, active: &str, user: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · 9router" }
                link rel="stylesheet" href="/static/dashboard.css";
                script src="/static/htmx.min.js" defer {}
            }
            body {
                div class="shell" {
                    (nav(active))
                    main class="main" {
                        header class="topbar" {
                            h2 { (title) }
                            div class="user-box" {
                                "Signed in as " (user)
                                form method="post" action="/logout" {
                                    button class="btn btn-sm" type="submit" { "Sign out" }
                                }
                            }
                        }
                        div class="content" { (body) }
                    }
                }
            }
        }
    }
}

/// Bare page shell (no sidebar) for the login screen.
pub fn bare_layout(title: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · 9router" }
                link rel="stylesheet" href="/static/dashboard.css";
                script src="/static/htmx.min.js" defer {};
            }
            body { (body) }
        }
    }
}

// ─── Router ─────────────────────────────────────────────────────────────────

pub fn router() -> Router {
    Router::new()
        .route("/static/{*path}", get(assets::static_handler))
        .route("/favicon.ico", get(|| async {
            ([(header::CONTENT_TYPE, "image/svg+xml")], assets::FAVICON)
        }))
        .route("/", get(pages::root_page))
        .route("/login", get(pages::login_page).post(forms::login_submit))
        .route("/logout", post_redirect("/login"))
        .route("/providers", get(pages::providers_page).post(forms::create_provider))
        .route("/providers/{id}", get(pages::provider_edit_page).put(forms::update_provider).delete(forms::delete_provider))
        .route("/keys", get(pages::keys_page).post(forms::create_key))
        .route("/keys/{id}", get(pages::key_edit_page).put(forms::update_key).delete(forms::delete_key))
        .route("/combos", get(pages::combos_page).post(forms::create_combo))
        .route("/combos/{id}", get(pages::combo_edit_page).put(forms::update_combo).delete(forms::delete_combo))
        .route("/settings", get(pages::settings_page).post(forms::update_settings))
        .route("/settings/password", post(forms::change_password))
        .route("/usage", get(pages::usage_page))
        .route("/oauth", get(pages::oauth_page))
        // AGENTS.md contract: the dashboard lives at /dashboard. The pages are
        // served at the root here, so /dashboard and /dashboard/<page> redirect.
        .route("/dashboard", get(|| async { dashboard_redirect("") }))
        .route("/dashboard/{*rest}", get(
            |axum::extract::Path(rest): axum::extract::Path<String>| async move { dashboard_redirect(&rest) },
        ))
}

fn dashboard_redirect(rest: &str) -> Response {
    let target = if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", rest.trim_start_matches("dashboard/").trim_matches('/'))
    };
    (StatusCode::SEE_OTHER, [(header::LOCATION, target), (header::CACHE_CONTROL, "no-store".to_string())])
        .into_response()
}

use axum::routing::post;

fn post_redirect(to: &str) -> axum::routing::MethodRouter {
    let to = to.to_string();
    post(move || async move {
        (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, to.clone()),
                (header::SET_COOKIE, "auth_token=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
        )
            .into_response()
    })
}
