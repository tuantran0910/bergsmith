//! One-origin browser surface: `/auth`, `/api/v1`, and the built UI.
//!
//! The session cookie is `SameSite=Lax` and the CSRF cookie must be readable
//! by the page, so the UI and API share an origin. Static serving never falls
//! back to `index.html` for `/api` or `/auth`: an unknown API route stays a
//! `404` instead of returning HTML to a typed client.
//!
//! The web build writes Brotli and gzip copies beside each text file
//! (`app.js.br`, `app.js.gz`). A request that accepts one of those codings
//! gets the precompressed copy, so the coordinator never compresses at run
//! time; API responses are never compressed here.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};

use crate::api::{self, ApiState};
use crate::identity::routes::{self as identity_routes, IdentityState};

/// The restrictive policy for the bundled single-page application.
///
/// Styles come only from the built stylesheet: React sets `style` through the
/// CSSOM, which the policy does not restrict. Trusted Types with no policy
/// make every string-to-script sink, such as `innerHTML`, throw. The web UI's
/// browser gate (`web/e2e`) reads this constant and fails on any violation.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
     script-src 'self'; \
     style-src 'self'; \
     img-src 'self' data:; \
     font-src 'self'; \
     connect-src 'self'; \
     object-src 'none'; \
     base-uri 'none'; \
     form-action 'self'; \
     frame-ancestors 'none'; \
     require-trusted-types-for 'script'; \
     trusted-types 'none'";

/// Why the browser surface could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The build directory or its entry point is missing.
    #[error("web root {path} is not a built UI: {reason}")]
    WebRoot {
        /// Configured directory.
        path: PathBuf,
        /// What is missing or unreadable.
        reason: &'static str,
    },
}

/// Builds the one-origin application.
///
/// # Errors
///
/// [`HttpError::WebRoot`] when a web root is configured but its `index.html`
/// is missing. Without a web root, only `/auth`, `/api/v1`, `/healthz`, and
/// `/readyz` are served; a reverse proxy serves the UI.
pub fn app(
    api: ApiState,
    identity: IdentityState,
    web_root: Option<PathBuf>,
) -> Result<Router, HttpError> {
    let database = api.database.clone();
    let root = match web_root {
        Some(web_root) => {
            if !web_root.join("index.html").is_file() {
                return Err(HttpError::WebRoot {
                    path: web_root,
                    reason: "index.html is missing",
                });
            }
            Some(Arc::new(web_root))
        }
        None => None,
    };
    let fallback = move |request: Request| {
        let root = root.clone();
        async move {
            match root {
                Some(root) => serve_static(&root, request).await,
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    };
    Ok(Router::new()
        .route("/healthz", axum::routing::get(health))
        .route(
            "/readyz",
            axum::routing::get(
                move || async move { crate::observability::readiness(&database).await },
            ),
        )
        .merge(api::router(api))
        .merge(identity_routes::router(identity))
        .fallback(fallback)
        .layer(middleware::map_response(security_headers)))
}

/// Liveness for a container or load balancer. It does not touch the database;
/// a coordinator that can answer HTTP is alive even while a dependency is
/// temporarily unavailable.
async fn health() -> StatusCode {
    StatusCode::OK
}

// `/readyz` answers `503` while PostgreSQL does not respond within two
// seconds, so a load balancer stops routing to a coordinator that cannot
// serve requests; see `observability::readiness`.

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
}

/// Content codings the build precompresses, best first, with the suffix of
/// the copy it writes.
const PRECOMPRESSED: [(&str, &str); 2] = [("br", "br"), ("gzip", "gz")];

/// Serves a built file, or `index.html` for a client route, precompressed
/// when the client accepts a coding the build wrote a copy for.
async fn serve_static(root: &Path, request: Request) -> Response {
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") || path == "/auth" || path.starts_with("/auth/")
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(relative) = safe_relative(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let requested = if relative.as_os_str().is_empty() {
        root.join("index.html")
    } else {
        root.join(&relative)
    };
    let file = match tokio::fs::metadata(&requested).await {
        Ok(metadata) if metadata.is_file() => requested,
        _ if has_extension(&relative) => return StatusCode::NOT_FOUND.into_response(),
        _ => root.join("index.html"),
    };
    let compressible = is_compressible(&file);
    let accept_encoding = request
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok());
    let mut encoding = None;
    let mut source = file.clone();
    if compressible {
        for (coding, suffix) in PRECOMPRESSED {
            if !accepts(accept_encoding, coding) {
                continue;
            }
            let candidate = with_suffix(&file, suffix);
            if tokio::fs::metadata(&candidate)
                .await
                .is_ok_and(|metadata| metadata.is_file())
            {
                encoding = Some(coding);
                source = candidate;
                break;
            }
        }
    }
    let Ok(bytes) = tokio::fs::read(&source).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if relative.starts_with("assets") && relative != Path::new("index.html") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(&file))
        .header(header::CACHE_CONTROL, cache);
    if compressible {
        // Caches must key a compressible file on the coding the client
        // accepts, whether or not this response is compressed.
        response = response.header(header::VARY, "accept-encoding");
    }
    if let Some(coding) = encoding {
        response = response.header(header::CONTENT_ENCODING, coding);
    }
    response
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Whether the build writes precompressed copies of this file: text only;
/// fonts and images are already compressed.
fn is_compressible(path: &Path) -> bool {
    matches!(
        path.extension().and_then(std::ffi::OsStr::to_str),
        Some("html" | "js" | "mjs" | "css" | "json" | "map" | "svg" | "txt")
    )
}

/// `path` with `.suffix` appended to its full file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether an `Accept-Encoding` value admits `coding` (RFC 9110, section
/// 12.5.3): named, or covered by `*`, with a non-zero quality. A named entry
/// overrides `*`, and a missing header admits no coding.
fn accepts(accept_encoding: Option<&str>, coding: &str) -> bool {
    let Some(value) = accept_encoding else {
        return false;
    };
    let mut wildcard = None;
    for element in value.split(',') {
        let mut parts = element.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let quality = parts
            .find_map(|parameter| {
                let (key, value) = parameter.split_once('=')?;
                key.trim()
                    .eq_ignore_ascii_case("q")
                    .then(|| value.trim().parse::<f32>().ok())
                    .flatten()
            })
            .unwrap_or(1.0);
        if name.eq_ignore_ascii_case(coding) {
            return quality > 0.0;
        }
        if name == "*" {
            wildcard = Some(quality);
        }
    }
    wildcard.is_some_and(|quality| quality > 0.0)
}

/// Rejects absolute and parent paths before joining them to the web root.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let path = path.trim_start_matches('/');
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(relative)
}

fn has_extension(path: &Path) -> bool {
    path.extension().is_some()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_root() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("assets")).unwrap();
        std::fs::write(
            directory.path().join("index.html"),
            "<!doctype html><title>app</title>",
        )
        .unwrap();
        std::fs::write(directory.path().join("assets/app.js"), "console.log('app')").unwrap();
        directory
    }

    async fn request(root: &Path, path: &str) -> Response {
        request_accepting(root, path, None).await
    }

    async fn request_accepting(root: &Path, path: &str, accept_encoding: Option<&str>) -> Response {
        let router = Router::new().fallback({
            let root = root.to_path_buf();
            move |request: Request| {
                let root = root.clone();
                async move { serve_static(&root, request).await }
            }
        });
        let mut builder = Request::builder().uri(path);
        if let Some(value) = accept_encoding {
            builder = builder.header(header::ACCEPT_ENCODING, value);
        }
        router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// A web root whose script and entry point have Brotli and gzip copies.
    fn precompressed_root() -> tempfile::TempDir {
        let directory = web_root();
        for (name, body) in [
            ("assets/app.js.br", "br-bytes"),
            ("assets/app.js.gz", "gz-bytes"),
            ("index.html.br", "br-index"),
        ] {
            std::fs::write(directory.path().join(name), body).unwrap();
        }
        directory
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_client_accepting_brotli_gets_the_brotli_copy() {
        let directory = precompressed_root();
        let response = request_accepting(
            directory.path(),
            "/assets/app.js",
            Some("gzip, deflate, br"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(headers.get(header::VARY).unwrap(), "accept-encoding");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            headers.get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(body(response).await, "br-bytes");
    }

    #[tokio::test]
    async fn a_client_refusing_brotli_gets_the_gzip_copy() {
        let directory = precompressed_root();
        let response =
            request_accepting(directory.path(), "/assets/app.js", Some("br;q=0, gzip")).await;
        assert_eq!(
            response.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip"
        );
        assert_eq!(body(response).await, "gz-bytes");
    }

    #[tokio::test]
    async fn without_an_accepted_coding_the_file_is_sent_as_built() {
        let directory = precompressed_root();
        for accept in [None, Some("identity"), Some("*;q=0")] {
            let response = request_accepting(directory.path(), "/assets/app.js", accept).await;
            assert!(
                response.headers().get(header::CONTENT_ENCODING).is_none(),
                "{accept:?}"
            );
            assert_eq!(
                response.headers().get(header::VARY).unwrap(),
                "accept-encoding",
                "{accept:?}"
            );
            assert_eq!(body(response).await, "console.log('app')", "{accept:?}");
        }
    }

    #[tokio::test]
    async fn a_missing_copy_falls_back_to_the_next_coding_or_the_file() {
        let directory = precompressed_root();
        // The entry point has a Brotli copy only.
        let response = request_accepting(directory.path(), "/jobs/abc", Some("gzip")).await;
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(body(response).await, "<!doctype html><title>app</title>");

        let response = request_accepting(directory.path(), "/jobs/abc", Some("br")).await;
        assert_eq!(
            response.headers().get(header::CONTENT_ENCODING).unwrap(),
            "br"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(body(response).await, "br-index");
    }

    #[tokio::test]
    async fn already_compressed_files_are_never_varied() {
        let directory = precompressed_root();
        std::fs::write(directory.path().join("assets/font.woff2"), "font").unwrap();
        std::fs::write(directory.path().join("assets/font.woff2.br"), "nope").unwrap();
        let response = request_accepting(directory.path(), "/assets/font.woff2", Some("br")).await;
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(response.headers().get(header::VARY).is_none());
        assert_eq!(body(response).await, "font");
    }

    #[test]
    fn accept_encoding_follows_quality_and_wildcard_rules() {
        assert!(accepts(Some("gzip, deflate, br"), "br"));
        assert!(accepts(Some("BR"), "br"));
        assert!(accepts(Some("gzip;q=0.5"), "gzip"));
        assert!(accepts(Some("*"), "br"));
        assert!(accepts(Some("br;q=0.001"), "br"));
        assert!(!accepts(None, "br"));
        assert!(!accepts(Some(""), "br"));
        assert!(!accepts(Some("gzip"), "br"));
        assert!(!accepts(Some("br;q=0"), "br"));
        assert!(!accepts(Some("br; q=0.0"), "br"));
        assert!(!accepts(Some("*;q=0"), "gzip"));
        assert!(!accepts(Some("*, br;q=0"), "br"));
        assert!(accepts(Some("br;q=bogus"), "br"));
    }

    use tower::ServiceExt as _;

    #[tokio::test]
    async fn the_entry_point_is_served_without_extension() {
        let directory = web_root();
        let response = request(directory.path(), "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn a_client_route_gets_the_entry_point() {
        let directory = web_root();
        let response = request(directory.path(), "/jobs/abc").await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_entry_point_is_revalidated_on_every_load() {
        let directory = web_root();
        for path in ["/", "/index.html", "/jobs/abc"] {
            let response = request(directory.path(), path).await;
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-cache",
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn assets_are_immutable_and_typed() {
        let directory = web_root();
        let response = request(directory.path(), "/assets/app.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn a_missing_asset_is_not_replaced_by_html() {
        let directory = web_root();
        let response = request(directory.path(), "/assets/missing.js").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_and_auth_fallbacks_are_not_html() {
        let directory = web_root();
        for path in ["/api/v1/nope", "/auth/nope"] {
            let response = request(directory.path(), path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let directory = web_root();
        let response = request(directory.path(), "/../Cargo.toml").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn safe_relative_keeps_normal_components_only() {
        assert_eq!(
            safe_relative("/assets/app.js").unwrap(),
            PathBuf::from("assets/app.js")
        );
        assert!(safe_relative("/../secret").is_none());
        assert!(safe_relative("//etc/passwd").is_some());
    }

    #[tokio::test]
    async fn security_headers_are_attached() {
        let response = security_headers(StatusCode::OK.into_response()).await;
        let headers = response.headers();
        assert!(headers.get(header::CONTENT_SECURITY_POLICY).is_some());
        assert_eq!(
            headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
        assert_eq!(
            headers.get(header::CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SECURITY_POLICY
        );
    }

    fn directives() -> Vec<&'static str> {
        CONTENT_SECURITY_POLICY.split("; ").collect()
    }

    #[test]
    fn styles_come_only_from_the_origin() {
        assert!(directives().contains(&"style-src 'self'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("'unsafe-inline'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("'unsafe-eval'"));
    }

    #[test]
    fn script_sinks_require_trusted_types_and_none_may_be_created() {
        let directives = directives();
        assert!(directives.contains(&"require-trusted-types-for 'script'"));
        assert!(directives.contains(&"trusted-types 'none'"));
    }

    #[test]
    fn every_directive_is_well_formed() {
        for directive in directives() {
            let name = directive.split(' ').next().unwrap();
            assert!(
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "{directive:?}"
            );
            assert_eq!(directive.trim(), directive, "{directive:?}");
        }
    }
}
