//! Serves the management interface's built assets at `/admin/` (RFC 0004 section 13):
//! `index.html` fallback for client-side routes, `Cache-Control: immutable` for hashed assets,
//! `no-store` for `index.html` itself.
//!
//! It also answers the server's bare root, `GET /` (exact path only): someone who types the
//! server's address into a browser is sent to `/admin/` when this binary carries the built
//! interface, and otherwise gets a small plain page saying what this server is (the equivalent of
//! Synapse's "It works!" page) rather than a `404`. See [`root_response`].
//!
//! What is embedded is whatever `build.rs` staged in `$OUT_DIR/web-dist`: the built interface
//! when there is one, a placeholder page when there is not. See `build.rs` for how that is
//! chosen and why a release build cannot end up with the placeholder by accident;
//! [`EMBEDDED_UI`] says which one this binary got.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

use crate::router::AdminState;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/web-dist"]
struct Assets;

/// Which management interface this binary carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedUi {
    /// The real interface, from a `web/` build.
    Built,
    /// A page saying the interface was not built in. `/api/v1` works all the same; there is just
    /// nothing at `/admin/` to drive it with.
    Placeholder,
}

/// Which management interface this binary carries, decided by `build.rs` at compile time.
pub const EMBEDDED_UI: EmbeddedUi = match env!("HS_ADMIN_WEB_UI").as_bytes() {
    b"built" => EmbeddedUi::Built,
    _ => EmbeddedUi::Placeholder,
};

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
        // A missing file under `assets/` is a missing file. Those names are content-hashed, so
        // the usual way to ask for one that is not here is a tab left open across an upgrade;
        // answering it with `index.html` and a `200` turns that into "Unexpected token '<'" in
        // the console instead of a failed load the app can notice.
        None if path.starts_with("assets/") => {
            (StatusCode::NOT_FOUND, "no such asset").into_response()
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

/// Where `GET /` sends a browser when the built interface is embedded.
pub const ROOT_REDIRECT_TARGET: &str = "/admin/";

/// The page `GET /` answers when this binary carries only the placeholder interface: enough for
/// someone who typed the address into a browser to know they reached a Matrix homeserver, and
/// where its APIs are.
const ROOT_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Myelin Matrix homeserver</title>
</head>
<body>
<h1>It works! Myelin is running.</h1>
<p>This is a <a href="https://matrix.org/">Matrix</a> homeserver, running Myelin. To use it,
sign in from a Matrix client with this server's address.</p>
<p>This build does not include the management interface. The admin API is at
<code>/api/v1</code>.</p>
</body>
</html>
"#;

/// What `GET /` (and `HEAD /`) answer, given which interface this binary carries.
///
/// With the built interface, a temporary redirect to [`ROOT_REDIRECT_TARGET`]: temporary because
/// what the root does depends on the build, so a browser must not remember it across an upgrade.
/// With the placeholder, `200` and a short plain page naming this server as a Myelin Matrix
/// homeserver. Either way a browser pointed at the server's address lands somewhere useful
/// instead of on a `404`.
pub fn root_response(ui: EmbeddedUi) -> Response {
    match ui {
        EmbeddedUi::Built => Redirect::temporary(ROOT_REDIRECT_TARGET).into_response(),
        EmbeddedUi::Placeholder => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            )],
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            ROOT_PAGE,
        )
            .into_response(),
    }
}

async fn server_root() -> Response {
    root_response(EMBEDDED_UI)
}

async fn admin_root() -> Response {
    serve_index_fallback()
}

async fn admin_asset(Path(path): Path<String>) -> Response {
    serve_embedded(&path)
}

/// The `/admin` and `/admin/*` routes, and the bare root `/` (exact path; see
/// [`root_response`]), ready to merge into the main router. The `GET` routes answer `HEAD` too.
pub fn router() -> axum::Router<AdminState> {
    axum::Router::new()
        .route("/", get(server_root))
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
    async fn a_missing_hashed_asset_is_404_not_the_app_shell() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/admin/assets/index-0ldHash.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Nothing that is only useful to a developer is staged into the binary, whichever
    /// interface it carries.
    #[test]
    fn no_source_maps_or_mock_worker_are_embedded() {
        for file in Assets::iter() {
            assert!(!file.ends_with(".map"), "{file}");
            assert_ne!(file, "mockServiceWorker.js");
        }
        assert!(Assets::get("index.html").is_some());
    }

    #[tokio::test]
    async fn admin_root_serves_the_embedded_index() {
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
        // True of the built interface and of the placeholder alike; which one this is depends
        // on whether `web/dist` existed when the crate was compiled.
        match EMBEDDED_UI {
            EmbeddedUi::Built => assert!(text.contains(r#"<div id="root">"#), "{text}"),
            EmbeddedUi::Placeholder => assert!(text.contains("has not been built"), "{text}"),
        }
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

    async fn request(method: &str, uri: &str) -> Response {
        app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[test]
    fn with_the_built_interface_the_root_redirects_to_it() {
        let response = root_response(EmbeddedUi::Built);
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/admin/");
    }

    #[tokio::test]
    async fn without_the_built_interface_the_root_is_a_plain_page_naming_the_server() {
        let response = root_response(EmbeddedUi::Placeholder);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert!(response.headers().get(header::LOCATION).is_none());
        let text = body_text(response).await;
        assert!(text.contains("Myelin"), "{text}");
        assert!(text.contains("Matrix"), "{text}");
    }

    #[tokio::test]
    async fn get_root_answers_for_the_interface_this_binary_carries() {
        let response = request("GET", "/").await;
        let expected = root_response(EMBEDDED_UI);
        assert_eq!(response.status(), expected.status());
        assert_eq!(
            response.headers().get(header::LOCATION),
            expected.headers().get(header::LOCATION)
        );
        // A query string does not change which route matches.
        assert_eq!(
            request("GET", "/?from=bookmark").await.status(),
            expected.status()
        );
    }

    #[tokio::test]
    async fn head_root_answers_like_get_without_a_body() {
        let get = request("GET", "/").await;
        let head = request("HEAD", "/").await;
        assert_eq!(head.status(), get.status());
        assert_eq!(
            head.headers().get(header::LOCATION),
            get.headers().get(header::LOCATION)
        );
        assert!(body_text(head).await.is_empty());
    }

    /// Only the exact root is taken: every other path is left to whoever else routes it (here,
    /// nobody, so axum's bare `404`; in the server, the Matrix fallback).
    #[tokio::test]
    async fn the_root_route_takes_nothing_but_the_root() {
        for uri in [
            "/index.html",
            "/foo",
            "/_matrix",
            "/.well-known/matrix/client",
        ] {
            let response = request("GET", uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert!(response.headers().get(header::LOCATION).is_none(), "{uri}");
        }
    }
}
