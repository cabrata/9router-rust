//! /dashboard/* aliases, fallback redirects, guard and sidebar.
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use std::sync::OnceLock;
use tower::ServiceExt;

static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

fn app() -> axum::Router {
    let dir = DIR.get_or_init(|| {
        let p = tempfile::TempDir::new().unwrap().into_path();
        std::env::set_var("DATA_DIR", &p);
        p
    });
    let _ = dir;
    nr_server::app()
}

async fn get(uri: &str, cookie: Option<&str>) -> (StatusCode, Option<String>, String) {
    let mut r = Request::builder().uri(uri);
    if let Some(c) = cookie { r = r.header(header::COOKIE, c); }
    let resp = app().oneshot(r.body(Body::empty()).unwrap()).await.unwrap();
    let st = resp.status();
    let loc = resp.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string());
    let b = axum::body::to_bytes(resp.into_body(), 10_000_000).await.unwrap();
    (st, loc, String::from_utf8_lossy(&b).into_owned())
}

/// True when the nav renders `href` with the active class, in any attribute
/// order (the markup also carries `aria-current="page"` for a11y).
fn has_active_link(body: &str, href: &str) -> bool {
    let needle = format!("href=\"{href}\"");
    body.match_indices(&needle).any(|(i, _)| {
        let tag_start = body[..i].rfind('<').unwrap_or(0);
        let tag_end = body[i..].find('>').map(|e| i + e).unwrap_or(body.len());
        let tag = &body[tag_start..tag_end];
        tag.contains(r#"class="active""#)
    })
}

fn cookie() -> String {
    app();
    format!("auth_token={}", nr_server::routes::auth::create_dashboard_auth_token(None).unwrap())
}

#[tokio::test]
async fn dashboard_pages_guarded_and_nav_rendered() {
    for p in ["/dashboard", "/dashboard/keys", "/dashboard/combos", "/dashboard/settings", "/dashboard/oauth"] {
        let (st, loc, _) = get(p, None).await;
        assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/login")), "{p}");
    }
    let c = cookie();
    let (st, _, body) = get("/dashboard/keys", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    for label in ["Endpoint", "Media Providers", "Request Logs", "Token Saver", "Pxpipe", "Basic Chat", "Proxy Pools"] {
        assert!(body.contains(label), "{label}");
    }
    assert!(has_active_link(&body, "/dashboard/keys"));
    assert!(body.contains("dashboard.js") && body.contains("data-theme-toggle"));
    let (st, _, body) = get("/dashboard", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(has_active_link(&body, "/dashboard"));
}

#[tokio::test]
async fn fallback_redirects_only_unrouted_legacy_pages() {
    let (st, loc, _) = get("/dashboard/providers?x=1", None).await;
    assert_eq!((st, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/providers?x=1")));
    let (_, loc, _) = get("/dashboard/usage", None).await;
    assert_eq!(loc.as_deref(), Some("/usage"));
    let (_, loc, _) = get("/dashboard/login", None).await;
    assert_eq!(loc.as_deref(), Some("/login"));
    let (st, loc, _) = get("/dashboard/nonexistent", None).await;
    assert_eq!((st, loc), (StatusCode::NOT_FOUND, None));
}

#[tokio::test]
async fn fallback_rejects_non_get_on_legacy_paths() {
    // /dashboard/login is unrouted below the fallback (login lives at /login), so
    // a non-GET/HEAD must be 405, never a redirect.
    let resp = app()
        .oneshot(Request::builder().method("POST").uri("/dashboard/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers().get(header::ALLOW).and_then(|v| v.to_str().ok()), Some("GET, HEAD"));
}

#[tokio::test]
async fn aliases_render_like_root_pages() {
    let c = cookie();
    for (root, alias, marker) in [
        ("/keys", "/dashboard/keys", "Keys"),
        ("/combos", "/dashboard/combos", "Combos"),
        ("/settings", "/dashboard/settings", "Settings"),
        ("/oauth", "/dashboard/oauth", "OAuth"),
    ] {
        let (st, _, root_body) = get(root, Some(&c)).await;
        assert_eq!(st, StatusCode::OK, "{root}");
        let (st, _, alias_body) = get(alias, Some(&c)).await;
        assert_eq!(st, StatusCode::OK, "{alias}");
        assert!(alias_body.contains(marker), "{alias}: missing {marker}");
        assert!(root_body.contains(marker), "{root}: missing {marker}");
    }
    // Detail pages under the alias too.
    let (st, _, body) = get("/dashboard/keys/abc", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Key not found"));
}

#[tokio::test]
async fn nav_sidebar_lists_all_js_items() {
    let c = cookie();
    let (_, _, body) = get("/dashboard/keys", Some(&c)).await;
    for label in [
        "Dashboard", "Endpoint", "Providers", "Media Providers", "Combos", "Usage", "Quota",
        "Request Logs", "Console Log", "CLI Tools", "MITM", "Translator", "Token Saver",
        "Pxpipe", "Skills", "Proxy Pools", "Profile", "Basic Chat",
    ] {
        assert!(body.contains(label), "missing nav item {label}");
    }
    assert!(body.contains("Manage"));
    for href in ["/dashboard/keys", "/dashboard/oauth", "/dashboard/settings"] {
        assert!(body.contains(&format!("href=\"{href}\"")), "missing manage link {href}");
    }
    // Active state follows prefix: a child path keeps the parent highlighted.
    let (_, _, body) = get("/dashboard/keys/abc", Some(&c)).await;
    assert!(has_active_link(&body, "/dashboard/keys"));
}

#[tokio::test]
async fn static_assets_served() {
    for (p, ct) in [("/static/dashboard.css", "text/css"), ("/static/dashboard.js", "javascript")] {
        let resp = app().oneshot(Request::builder().uri(p).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{p}");
        assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().contains(ct));
    }
}
