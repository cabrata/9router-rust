//! The local HTTPS interception proxy. Port of src/mitm/server.js.
//!
//! Listens on 443 with a cert signed by our Root CA (generated per SNI), then for
//! each request:
//!   * `/…/:generateContent` etc. from an intercepted IDE → handler → 9Router
//!   * everything else → passthrough to the real upstream (TLS, upstream verification off)
//!
//! Why `danger_accept_invalid_certs` on the upstream leg: the hosts file sends the
//! tool hostnames to 127.0.0.1, so a normal verified client would loop back into us.
//! We dial the real IP with SNI = hostname and skip verification, exactly like the
//! JS `rejectUnauthorized: false` request.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use axum::Router;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::json;

use crate::mitm::cert::CertResolver;
use crate::mitm::handlers::{self, base, InterceptCtx};
use crate::mitm::{antigravity_ide_version, config, logger};

/// Default interception port. 443 is required: the IDE dials the real hostname.
pub const LOCAL_PORT: u16 = 443;

/// Marks our own traffic so the MITM never intercepts a request that 9Router
/// itself sent (anti-loop).
const INTERNAL_REQUEST_HEADER: &str = "x-request-source";
const INTERNAL_REQUEST_VALUE: &str = "local";

/// PROD cloudcode-pa is rate-limited (429); daily-cloudcode-pa (dev endpoint)
/// accepts the same body + token. Same workaround as open-sse.
const HOST_REWRITE: &[(&str, &str)] = &[(
    "cloudcode-pa.googleapis.com",
    "daily-cloudcode-pa.googleapis.com",
)];

fn rewrite_host_for_chat(original_host: &str, path_and_query: &str) -> String {
    let is_chat_endpoint =
        path_and_query.contains(":generateContent") || path_and_query.contains(":streamGenerateContent");
    if !is_chat_endpoint {
        return original_host.to_string();
    }
    HOST_REWRITE
        .iter()
        .find(|(from, _)| *from == original_host)
        .map(|(_, to)| to.to_string())
        .unwrap_or_else(|| original_host.to_string())
}

/// Run the MITM proxy until the task is cancelled.
///
/// Returns an error when the port can't be bound (the JS build exits with a clear
/// message in that case too) so the UI can tell the user what holds :443.
pub async fn run(port: u16) -> nr_core::Result<()> {
    logger::clear_dump_dir();
    let resolver = crate::mitm::cert::resolver()?;
    serve(port, resolver).await
}

/// Serve with an explicit resolver. Split out so tests can bind an ephemeral port.
pub async fn serve(port: u16, resolver: Arc<CertResolver>) -> nr_core::Result<()> {
    let addr: SocketAddr = if port == 0 {
        SocketAddr::from(([127, 0, 0, 1], 0))
    } else {
        SocketAddr::from(([0, 0, 0, 0], port))
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| nr_core::Error::other(format!("bind :{port}: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| nr_core::Error::other(format!("local_addr: {e}")))?;
    logger::log(&format!("Server ready on :{}", bound.port()));
    serve_listener(listener, resolver).await
}

/// Accept loop: one TLS handshake per connection, then hyper serves that connection.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    resolver: Arc<CertResolver>,
) -> nr_core::Result<()> {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(v) => v,
            // A failed accept must not kill the proxy.
            Err(e) => {
                logger::err(&format!("accept failed: {e}"));
                continue;
            }
        };
        let resolver = resolver.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, resolver).await {
                // Handshake failures are normal: every browser/IDE probe with an
                // unknown SNI lands here.
                tracing::debug!("[MITM] connection ended: {e}");
            }
        });
    }
}

async fn handle_connection(stream: tokio::net::TcpStream, resolver: Arc<CertResolver>) -> nr_core::Result<()> {
    let acceptor = SniAcceptor::new(resolver);
    let tls_stream = acceptor.accept(stream).await?;

    let io = TokioIo::new(tls_stream);
    let service = service_fn(move |req: http::Request<Incoming>| async move {
        Ok::<_, std::convert::Infallible>(proxy_request(req).await)
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        // Clients may use HTTP/2 outside; we only offer http/1.1 in ALPN since the
        // upstream leg is re-originated anyway.
        .await
        .map_err(|e| nr_core::Error::other(format!("http1 connection: {e}")))?;
    Ok(())
}

/// TLS acceptor that mints a leaf cert for the requested SNI.
struct SniAcceptor {
    resolver: Arc<CertResolver>,
}

impl SniAcceptor {
    fn new(resolver: Arc<CertResolver>) -> Self {
        Self { resolver }
    }

    async fn accept(&self, stream: tokio::net::TcpStream) -> nr_core::Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>> {
        let resolver = Arc::new(PerConnectionResolver {
            inner: self.resolver.clone(),
        });
        let mut cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        acceptor
            .accept(stream)
            .await
            .map_err(|e| nr_core::Error::other(format!("tls handshake: {e}")))
    }
}

/// rustls resolver that looks up SNI at handshake time.
struct PerConnectionResolver {
    inner: Arc<CertResolver>,
}

impl std::fmt::Debug for PerConnectionResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PerConnectionResolver")
    }
}

impl rustls::server::ResolvesServerCert for PerConnectionResolver {
    fn resolve(&self, client_hello: rustls::server::ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let server_name = client_hello.server_name()?;
        // A malformed or hostile SNI must fail the handshake, not the process.
        match self.inner.certified_key(server_name) {
            Ok(key) => key,
            Err(e) => {
                logger::err(&format!("SNI cert generation failed: {e}"));
                None
            }
        }
    }
}

/// Route one decrypted request: health endpoint, intercept, or passthrough.
async fn proxy_request(req: http::Request<Incoming>) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body_bytes) = axum::body::to_bytes(Body::new(body), MAX_BODY_BYTES).await else {
        return base::error_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    route(parts.method, &parts.uri, &parts.headers, body_bytes).await
}

/// Ceiling on an intercepted request body. IDE chat turns with inline images can be
/// large, but an unbounded buffer is a trivial memory-exhaustion vector on :443.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

fn host_of(headers: &HeaderMap) -> String {
    headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_else(|| config::TARGET_HOSTS[0].to_string())
}

/// The whole routing decision, shared by the TLS server, the axum fallback and tests.
async fn route(
    method: http::Method,
    uri: &http::Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    if path_and_query == "/_mitm_health" {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "ok": true, "pid": std::process::id() }).to_string(),
            ))
            .unwrap_or_else(|_| Response::new(Body::empty()));
    }

    if logger::dump_enabled() {
        logger::dump_request(
            method.as_str(),
            &path_and_query,
            &host_of(headers),
            headers,
            &body,
        );
    }

    let host = host_of(headers);

    // Anti-loop: never intercept our own outbound calls.
    let is_internal = headers
        .get(INTERNAL_REQUEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == INTERNAL_REQUEST_VALUE);

    let tool = config::get_tool_for_host(&host).filter(|_| !is_internal);
    let Some(tool) = tool.filter(|t| config::is_chat_request(t, &path_and_query, headers)) else {
        return passthrough(&method, &path_and_query, &host, headers, body).await;
    };

    // Cursor uses protobuf; model extraction isn't possible at this layer, so the
    // handler is delegated to directly (and currently answers 501).
    let model = match tool {
        "cursor" => None,
        _ => config::extract_model(&path_and_query, &body),
    };

    // Intentional passthrough: some models must never be re-routed (e.g. Antigravity
    // tab-autocomplete) so latency-critical inline completion stays native. Silent —
    // this is by design, not a leak, and fires per keystroke. See MODEL_NO_MAP.
    if let Some(model) = model.as_deref() {
        if config::model_is_no_map(tool, model) {
            return passthrough(&method, &path_and_query, &host, headers, body).await;
        }
    }

    let mapped = if tool == "cursor" {
        None
    } else {
        handlers::mapped_model(tool, model.as_deref())
    };
    if mapped.is_none() && tool != "cursor" {
        return passthrough(&method, &path_and_query, &host, headers, body).await;
    }

    let ctx = InterceptCtx::new(tool, mapped, headers.clone());
    handlers::intercept(tool, &path_and_query, &body, &ctx)
        .await
        .unwrap_or_else(|e| base::error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

/// Forward to the real upstream and stream the reply back untouched.
///
/// The upstream leg is a *fresh* TLS connection with SNI = hostname and certificate
/// verification disabled: the hosts file points the name at 127.0.0.1, so verifying
/// would recurse back into us. This mirrors the JS `rejectUnauthorized: false`.
async fn passthrough(
    method: &http::Method,
    path_and_query: &str,
    original_host: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let target_host = rewrite_host_for_chat(original_host, path_and_query);
    let tool = config::get_tool_for_host(original_host);

    // Antigravity ships a version marker upstream rejects; rewrite it before forwarding.
    let (body, headers) = match tool {
        Some("antigravity") => {
            let over = antigravity_ide_version::apply_antigravity_ide_version_override(
                &body,
                headers,
                path_and_query,
            );
            (Bytes::from(over.body), over.headers)
        }
        _ => (body, headers.clone()),
    };

    let mut dumper = logger::ResponseDumper::new(path_and_query, original_host, "passthrough");

    let url = format!("https://{target_host}{path_and_query}");
    let mut outbound = base::insecure_client()
        .request(method.clone(), &url)
        .header("host", &target_host);
    for (name, value) in headers.iter() {
        // content-length is recomputed from the (possibly rewritten) body;
        // accept-encoding is dropped so reqwest's own decompression stays coherent.
        if matches!(
            name.as_str(),
            "host" | "connection" | "transfer-encoding" | "content-length" | "accept-encoding"
        ) {
            continue;
        }
        outbound = outbound.header(name, value);
    }
    if !body.is_empty() {
        outbound = outbound.body(body);
    }

    match outbound.send().await {
        Ok(upstream) => {
            if let Some(d) = dumper.as_mut() {
                d.write_header(upstream.status().as_u16(), upstream.headers());
            }
            base::stream_through(upstream, dumper)
        }
        Err(e) => {
            logger::err(&format!("Passthrough error: {e}"));
            if let Some(d) = dumper {
                d.finish();
            }
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Bad Gateway"))
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
    }
}

/// Plain-HTTP entry point for tests and in-process use (no TLS termination).
pub fn app() -> Router {
    Router::new().fallback(axum_proxy_handler)
}

async fn axum_proxy_handler(request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return base::error_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    route(parts.method, &parts.uri, &parts.headers, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn host_rewrite_only_for_chat_endpoints() {
        assert_eq!(
            rewrite_host_for_chat("cloudcode-pa.googleapis.com", "/v1/models/x:generateContent"),
            "daily-cloudcode-pa.googleapis.com"
        );
        // Auth/login endpoints must go to PROD cloudcode-pa.
        assert_eq!(
            rewrite_host_for_chat("cloudcode-pa.googleapis.com", "/v1/auth/login"),
            "cloudcode-pa.googleapis.com"
        );
        assert_eq!(
            rewrite_host_for_chat("api2.cursor.sh", "/RunSSE"),
            "api2.cursor.sh"
        );
    }

    #[test]
    fn host_header_parsing_strips_port() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("api2.cursor.sh:443"));
        assert_eq!(host_of(&h), "api2.cursor.sh");
        assert_eq!(host_of(&HeaderMap::new()), config::TARGET_HOSTS[0]);
    }
}
