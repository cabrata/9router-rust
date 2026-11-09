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
    assert!(body.contains(r#"<a href="/dashboard/keys" class="active">"#));
    assert!(body.contains("dashboard.js") && body.contains("data-theme-toggle"));
    let (st, _, body) = get("/dashboard", Some(&c)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains(r#"<a href="/dashboard" class="active">"#));
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
async fn static_assets_served() {
    for (p, ct) in [("/static/dashboard.css", "text/css"), ("/static/dashboard.js", "javascript")] {
        let resp = app().oneshot(Request::builder().uri(p).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{p}");
        assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().contains(ct));
    }
}
