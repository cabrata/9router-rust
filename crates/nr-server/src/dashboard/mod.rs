//! HTML dashboard served as HTML-over-the-wire (maud) with htmx for interactivity.
//!
//! Port of the dashboard surface of `src/app/(dashboard)/**` + `src/app/login/page.js`.
//! The pages consume exactly the JSON shapes the JS frontend reads from `/api/*`
//! (`connections`, `keys`, `combos`, settings, `usage/stats`, `usage/chart`); data is
//! read through `nr_db` (the same repos the `/api` handlers use) so the rendered rows
//! are identical to what `/api/providers`, `/api/keys`, … return.

pub mod assets;
pub mod components;
pub mod forms;
pub mod pages;
pub mod dash_providers;
pub mod dash_ops;
pub mod dash_tools;

use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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

/// Sidebar entries, in the order of the JS sidebar
/// (src/shared/components/Sidebar.js). Hrefs are `/dashboard/...`.
pub const NAV_ITEMS: &[(&str, &str)] = &[
    ("/dashboard", "Dashboard"),
    ("/dashboard/endpoint", "Endpoint"),
    ("/dashboard/providers", "Providers"),
    ("/dashboard/media-providers", "Media Providers"),
    ("/dashboard/combos", "Combos"),
    ("/dashboard/usage", "Usage"),
    ("/dashboard/quota", "Quota"),
    ("/dashboard/request-logs", "Request Logs"),
    ("/dashboard/console-log", "Console Log"),
    ("/dashboard/cli-tools", "CLI Tools"),
    ("/dashboard/mitm", "MITM"),
    ("/dashboard/translator", "Translator"),
    ("/dashboard/token-saver", "Token Saver"),
    ("/dashboard/pxpipe", "Pxpipe"),
    ("/dashboard/skills", "Skills"),
    ("/dashboard/proxy-pools", "Proxy Pools"),
    ("/dashboard/profile", "Profile"),
    ("/dashboard/basic-chat", "Basic Chat"),
];

/// Pages that only exist in the Rust build (JS keeps them inside Endpoint/Profile).
pub const EXTRA_NAV_ITEMS: &[(&str, &str)] = &[
    ("/dashboard/keys", "API Keys"),
    ("/dashboard/oauth", "OAuth"),
    ("/dashboard/settings", "Settings"),
];

/// Legacy root pages. Each one is (or will be) mirrored under `/dashboard/<page>`;
/// [`fallback`] only redirects these when no explicit route claimed the path.
const LEGACY_PAGES: &[&str] = &["providers", "keys", "combos", "settings", "usage", "oauth", "login"];

/// Accepts both legacy root paths (`/providers`, `/`) and `/dashboard/...` paths,
/// with or without a trailing slash.
fn normalize_active(active: &str) -> String {
    let trimmed = active.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    match path {
        "/" => "/dashboard".to_string(),
        p if p == "/dashboard" || p.starts_with("/dashboard/") => p.to_string(),
        p => format!("/dashboard{p}"),
    }
}

/// Active-state by prefix on a segment boundary; `/dashboard` itself matches exactly
/// (mirrors `isActive` in Sidebar.js, where `/dashboard` means the Endpoint page).
pub fn nav_is_active(href: &str, active: &str) -> bool {
    let active = normalize_active(active);
    if href == "/dashboard" {
        return active == href;
    }
    active == href || active.starts_with(&format!("{href}/"))
}

fn nav_links(items: &[(&str, &str)], active: &str) -> Markup {
    html! {
        @for (href, label) in items {
            li {
                a href=(href) class=(if nav_is_active(href, active) { "active" } else { "" })
                    aria-current=[if nav_is_active(href, active) { Some("page") } else { None }] {
                    (label)
                }
            }
        }
    }
}

/// Sidebar port of `src/shared/components/Sidebar.js`: traffic lights, gradient
/// logo, main nav, Manage section. Collapses to a drawer on narrow screens
/// (`data-nav-toggle` in the topbar, wired in `static/dashboard.js`).
fn nav(active: &str) -> Markup {
    html! {
        aside class="sidebar" id="sidebar" {
            div class="traffic" aria-hidden="true" {
                span class="dot red" {} span class="dot amber" {} span class="dot green" {}
            }
            div class="brand" {
                a href="/dashboard" class="brand-link" {
                    span class="logo" { "9" }
                    span class="brand-text" {
                        h1 { "9router" }
                        div class="ver" { (PreEscaped(concat!("v", env!("CARGO_PKG_VERSION"), " · rust"))) }
                    }
                }
            }
            nav class="side-nav" aria-label="Dashboard" {
                ul class="nav" { (nav_links(NAV_ITEMS, active)) }
                div class="nav-section" { "Manage" }
                ul class="nav" { (nav_links(EXTRA_NAV_ITEMS, active)) }
            }
            div class="sidebar-foot" {
                a href="/api/version" class="muted" { "version" }
            }
        }
    }
}

/// Applies the saved theme before first paint (no flash). Mirrors themeStore.js:
/// localStorage key `theme`, values light | dark | system (default system).
const THEME_BOOT: &str = "(function(){try{var t=localStorage.getItem('theme')||'system';\
var d=t==='dark'||(t==='system'&&matchMedia('(prefers-color-scheme: dark)').matches);\
document.documentElement.classList.toggle('dark',d)}catch(e){}})()";

fn head(title: &str) -> Markup {
    html! {
        meta charset="utf-8";
        meta name="viewport" content="width=device-width, initial-scale=1";
        title { (title) " · 9router" }
        link rel="icon" href="/favicon.ico";
        script { (PreEscaped(THEME_BOOT)) }
        link rel="stylesheet" href="/static/dashboard.css";
        script src="/static/htmx.min.js" defer {}
        script src="/static/dashboard.js" defer {}
    }
}

/// ThemeToggle.js equivalent; behaviour lives in assets/dashboard.js.
pub fn theme_toggle() -> Markup {
    html! {
        button type="button" class="theme-toggle" data-theme-toggle aria-label="Toggle theme" title="Toggle theme" {
            span class="when-light" { "☾" } span class="when-dark" { "☀" }
        }
    }
}

/// Standard dashboard shell. `active` is a nav path (`/dashboard/x`) or a legacy root path (`/x`).
pub fn layout(title: &str, active: &str, user: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head { (head(title)) }
            body {
                div class="shell" {
                    (nav(active))
                    main class="main" {
                        header class="topbar" {
                            div class="topbar-left" {
                                button type="button" class="nav-toggle" data-nav-toggle
                                    aria-label="Toggle navigation" aria-controls="sidebar" { "☰" }
                                h2 { (title) }
                            }
                            div class="user-box" {
                                (theme_toggle())
                                span class="user-name" { (user) }
                                form method="post" action="/logout" {
                                    button class="btn btn-sm btn-ghost" type="submit" { "Sign out" }
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
            head { (head(title)) }
            body { div class="bare-theme" { (theme_toggle()) } (body) }
        }
    }
}

// ─── Router ─────────────────────────────────────────────────────────────────

/// Dashboard routes.
///
/// The legacy pages are served at both their root path and `/dashboard/<page>`:
///
/// | root          | mirrored                    | owner            |
/// |---------------|-----------------------------|------------------|
/// | `/keys`       | `/dashboard/keys`           | this module      |
/// | `/combos`     | `/dashboard/combos`         | this module      |
/// | `/settings`   | `/dashboard/settings`       | this module      |
/// | `/oauth`      | `/dashboard/oauth`          | this module      |
/// | `/login`      | `/dashboard/login`          | fallback redirect |
/// | `/providers`  | `/dashboard/providers`      | `dash_providers` |
/// | `/usage`      | `/dashboard/usage`          | `dash_ops`       |
///
/// `/providers` and `/usage` are *not* re-registered here: `dash_providers` and
/// `dash_ops` already own the `/dashboard/...` path (axum panics on overlapping
/// method routes), and their pages render the same data. The root handlers stay in
/// `pages` for clients and tests that still use them.
pub fn router() -> Router {
    let keys = get(pages::keys_page).post(forms::create_key);
    let key = get(pages::key_edit_page).put(forms::update_key).delete(forms::delete_key);
    let combos = get(pages::combos_page).post(forms::create_combo);
    let combo = get(pages::combo_edit_page).put(forms::update_combo).delete(forms::delete_combo);
    let settings = get(pages::settings_page).post(forms::update_settings);
    let login = get(pages::login_page).post(forms::login_submit);
    Router::new()
        .merge(dash_providers::router())
        .merge(dash_ops::router())
        .merge(dash_tools::router())
        .route("/static/{*path}", get(assets::static_handler))
        .route("/favicon.ico", get(|| async {
            ([(header::CONTENT_TYPE, "image/svg+xml")], assets::FAVICON)
        }))
        .route("/", get(pages::root_page))
        .route("/login", login)
        .route("/logout", post_redirect("/login"))
        .route("/providers", get(pages::providers_page).post(forms::create_provider))
        .route("/providers/{id}", get(pages::provider_edit_page).put(forms::update_provider).delete(forms::delete_provider))
        .route("/keys", keys.clone())
        .route("/dashboard/keys", keys)
        .route("/keys/{id}", key.clone())
        .route("/dashboard/keys/{id}", key)
        .route("/combos", combos.clone())
        .route("/dashboard/combos", combos)
        .route("/combos/{id}", combo.clone())
        .route("/dashboard/combos/{id}", combo)
        .route("/settings", settings.clone())
        .route("/dashboard/settings", settings)
        .route("/settings/password", post(forms::change_password))
        .route("/dashboard/settings/password", post(forms::change_password))
        .route("/usage", get(pages::usage_page))
        .route("/oauth", get(pages::oauth_page))
        .route("/dashboard/oauth", get(pages::oauth_page))
        .fallback(fallback)
}

/// Runs only when no route matched. `/dashboard[/]` is served here (not routed) so a
/// sibling lane may claim it (JS shows the Endpoint page there) without a merge
/// conflict. `/dashboard/<legacy page>[/…]` redirects (GET/HEAD) to the root page
/// that already serves it; anything else is a plain 404.
async fn fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let readable = method == Method::GET || method == Method::HEAD;
    if readable && uri.path().trim_end_matches('/') == "/dashboard" {
        return pages::root_page(headers).await;
    }
    match dashboard_redirect_target(uri.path()) {
        Some(_) if !readable => (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD".to_string())],
            "method not allowed",
        )
            .into_response(),
        Some(mut target) => {
            if let Some(q) = uri.query() {
                target.push('?');
                target.push_str(q);
            }
            (StatusCode::SEE_OTHER, [(header::LOCATION, target), (header::CACHE_CONTROL, "no-store".to_string())])
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn dashboard_redirect_target(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/dashboard")?.trim_matches('/');
    let first = rest.split('/').next().unwrap_or("");
    LEGACY_PAGES.contains(&first).then(|| format!("/{rest}"))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_by_prefix() {
        assert!(nav_is_active("/dashboard", "/"));
        assert!(nav_is_active("/dashboard", "/dashboard/"));
        assert!(!nav_is_active("/dashboard", "/providers"));
        assert!(nav_is_active("/dashboard/providers", "/providers"));
        assert!(nav_is_active("/dashboard/providers", "/dashboard/providers/abc"));
        assert!(!nav_is_active("/dashboard/providers", "/dashboard/providers-x"));
        assert!(nav_is_active("/dashboard/media-providers", "/dashboard/media-providers/web"));
        assert!(!nav_is_active("/dashboard/media-providers", "/dashboard/providers"));
        assert!(nav_is_active("/dashboard/keys", "/keys"));
        assert!(nav_is_active("/dashboard/keys", "/dashboard/keys/abc/"));
    }

    #[test]
    fn nav_mirrors_js_sidebar() {
        let labels: Vec<_> = NAV_ITEMS.iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "Dashboard", "Endpoint", "Providers", "Media Providers", "Combos", "Usage", "Quota",
                "Request Logs", "Console Log", "CLI Tools", "MITM", "Translator", "Token Saver",
                "Pxpipe", "Skills", "Proxy Pools", "Profile", "Basic Chat",
            ]
        );
        assert!(NAV_ITEMS.iter().all(|(h, _)| h.starts_with("/dashboard")));
    }

    #[test]
    fn every_nav_item_activates_for_its_own_child() {
        for (href, _) in NAV_ITEMS.iter().chain(EXTRA_NAV_ITEMS) {
            assert!(nav_is_active(href, href), "{href}");
            // The Dashboard item is special: it matches only itself (JS maps it to
            // the Endpoint page); every other item activates for its subtree.
            if *href != "/dashboard" {
                assert!(nav_is_active(href, &format!("{href}/child")), "{href}/child");
            }
            assert!(!nav_is_active(href, &format!("{href}-other")), "{href}-other");
        }
    }

    #[test]
    fn redirect_targets() {
        assert_eq!(dashboard_redirect_target("/dashboard/providers").as_deref(), Some("/providers"));
        assert_eq!(dashboard_redirect_target("/dashboard/providers/x/").as_deref(), Some("/providers/x"));
        assert_eq!(dashboard_redirect_target("/dashboard/nope"), None);
        assert_eq!(dashboard_redirect_target("/other"), None);
    }
}
