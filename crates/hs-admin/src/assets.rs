//! Serves the management interface's built assets at `/admin/` (RFC 0004 section 13):
//! `index.html` fallback for client-side routes, `Cache-Control: immutable` for hashed assets,
//! `no-store` for `index.html` itself.
//!
//! Embeds `crates/hs-admin/web-dist-placeholder/` today, not `web/dist` directly: `web/dist` is a
//! build artifact (`cd web && npm run build`) and is gitignored, so embedding it directly would
//! break a clean checkout that has not run that build. See the comment in
//! `web-dist-placeholder/index.html` for how to switch this to the real build output.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

use crate::router::AdminState;

#[derive(RustEmbed)]
#[folder = "web-dist-placeholder"]
struct Assets;

/// A small, deliberately incomplete extension-to-MIME map covering what a Vite build emits
/// (`.html`, `.js`, `.css`, `.json`, `.svg`, `.png`, `.woff2`, source maps); anything else falls
/// back to `application/octet-stream`. Avoids taking a direct dependency on a MIME-sniffing crate
/// for a handful of well-known extensions.
fn guess_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let cache_control = if path == "index.html" {
                "no-store"
            } else {
                "public, max-age=31536000, immutable"
            };
            (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(guess_content_type(path)),
                )],
                [(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(cache_control),
                )],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => serve_index_fallback(),
    }
}

/// Client-side routes (anything under `/admin/` that is not a real asset) fall back to
/// `index.html`, matching a single-page app's own router.
fn serve_index_fallback() -> Response {
    match Assets::get("index.html") {
        Some(file) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            )],
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            file.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "no management interface assets are embedded",
        )
            .into_response(),
    }
}

async fn admin_root() -> Response {
    serve_index_fallback()
}

async fn admin_asset(Path(path): Path<String>) -> Response {
    serve_embedded(&path)
}

/// The `/admin` and `/admin/*` routes, ready to merge into the main router.
pub fn router() -> axum::Router<AdminState> {
    axum::Router::new()
        .route("/admin", get(admin_root))
        .route("/admin/", get(admin_root))
        .route("/admin/{*path}", get(admin_asset))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    fn app() -> axum::Router {
        router().with_state(crate::router::tests_support::dummy_state())
    }

    #[tokio::test]
    async fn admin_root_serves_placeholder_index() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("hs-admin"),
            "placeholder index.html should be served, got: {text}"
        );
    }

    #[tokio::test]
    async fn unknown_client_route_falls_back_to_index() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/admin/rooms/!abc:example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("<html"));
    }

    #[tokio::test]
    async fn index_html_is_no_store() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }
}
