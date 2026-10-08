//! The two `.well-known` discovery documents a homeserver publishes about itself, closing the
//! "`GET /.well-known/matrix/server` not served" gap in `docs/next-steps.md`'s known-gaps table.
//!
//! This is the *serving* side of discovery. The *fetching* side already exists and is what makes
//! these documents matter: `hs_federation::discovery` resolves a remote server name by fetching
//! exactly this document from it (`crates/hs-federation/src/discovery.rs`, step 3 of its
//! resolution order), and `matrix-rust-sdk`/Element resolve a user's homeserver by fetching the
//! client one. A deployment whose `server_name` is `example.org` but whose server actually listens
//! on `matrix.example.org` cannot be found at all without these, which is why the gap was
//! load-bearing rather than cosmetic.
//!
//! # What is served, and when
//!
//! | route | served when | body |
//! |---|---|---|
//! | `GET /.well-known/matrix/server` | `server.well_known_server` is set | `{"m.server": "<host[:port]>"}` |
//! | `GET /.well-known/matrix/client` | `server.public_baseurl` is set | `{"m.homeserver": {"base_url": "<url>"}}` |
//! | `GET /.well-known/matrix/support` | `server.admin_contact` is set | `{"contacts": [{"role": "m.role.admin", "email_address" or "matrix_id": ...}]}`, or `{"support_page": "<url>"}` |
//!
//! The support document (spec v1.10, MSC1929) is what clients read to tell people who to ask for
//! help or report abuse to; `server.admin_contact` is a `mailto:` address, a bare email address,
//! a Matrix ID, or an `http(s)://` support page ([`SupportContact::parse`]).
//!
//! All three are **absent** (404 `M_NOT_FOUND`, the same answer as any unrouted path) when their
//! config field is unset, rather than being served with a value derived from `server_name`. A
//! well-known document that points at the server name it was fetched from is indistinguishable
//! from no document at all in the spec's resolution order — both end at "connect to the server
//! name" — so serving one adds a failure mode (a fetch that can now time out or return a broken
//! body) without adding a capability. Synapse makes the same choice with its
//! `serve_server_wellknown` defaulting to false.
//!
//! # CORS
//!
//! The client document is fetched by browsers from a *different* origin than the one it describes
//! (that is its whole purpose: a web client hosted anywhere resolves `example.org` to a base URL),
//! so it carries `Access-Control-Allow-Origin: *` as the spec requires. The server document is
//! fetched server-to-server and needs no such header, but gets one anyway for symmetry with what
//! every other implementation serves.
//!
//! # Why this lives in `hs-cli` rather than a library crate
//!
//! The bodies are pure functions of configuration, with no store, no auth and no per-request
//! state. Putting them next to the other config-derived routes this binary already owns
//! (`crate::versions`, `crate::capabilities`) keeps the whole `.well-known` surface in one file
//! rather than splitting one two-line document across a crate boundary.

use axum::Json;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// The `.well-known` values this server publishes about itself, read once from configuration at
/// startup. Cheap to clone (two `Option<String>`s), held by the router's handlers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WellKnown {
    /// `server.well_known_server`: the `host[:port]` remote servers should connect to for
    /// federation. `None` means `/.well-known/matrix/server` is not served.
    pub server: Option<String>,
    /// `server.public_baseurl`: the client-facing base URL. `None` means
    /// `/.well-known/matrix/client` is not served.
    pub client_base_url: Option<String>,
    /// `server.admin_contact`: who to ask for help. `None` means
    /// `/.well-known/matrix/support` is not served.
    pub support: Option<SupportContact>,
}

/// How to reach the administrator, as `/.well-known/matrix/support` publishes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupportContact {
    /// An email address (`mailto:` stripped), published as the admin contact's `email_address`.
    Email(String),
    /// A Matrix user ID, published as the admin contact's `matrix_id`.
    MatrixId(String),
    /// A web page, published as `support_page`.
    Page(String),
}

impl SupportContact {
    /// Reads `server.admin_contact`: `mailto:a@example.org` or `a@example.org` (email),
    /// `@admin:example.org` (Matrix ID), `https://example.org/help` (a support page). `None` for
    /// an empty value or one that is none of these.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let value = raw.trim();
        if let Some(email) = value.strip_prefix("mailto:") {
            let email = email.trim();
            return email.contains('@').then(|| Self::Email(email.to_owned()));
        }
        if value.starts_with("https://") || value.starts_with("http://") {
            return Some(Self::Page(value.to_owned()));
        }
        if value.starts_with('@') {
            return ruma::UserId::parse(value)
                .ok()
                .map(|id| Self::MatrixId(id.to_string()));
        }
        (value.contains('@') && !value.contains(char::is_whitespace))
            .then(|| Self::Email(value.to_owned()))
    }

    /// The document's body.
    #[must_use]
    pub fn document(&self) -> serde_json::Value {
        match self {
            Self::Email(email) => {
                json!({"contacts": [{"role": "m.role.admin", "email_address": email}]})
            }
            Self::MatrixId(id) => json!({"contacts": [{"role": "m.role.admin", "matrix_id": id}]}),
            Self::Page(url) => json!({"support_page": url}),
        }
    }
}

impl WellKnown {
    /// Reads both values from a native configuration.
    #[must_use]
    pub fn from_config(config: &hs_config::Config) -> Self {
        Self {
            server: config
                .server
                .well_known_server
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            client_base_url: config
                .server
                .public_baseurl
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.trim_end_matches('/').to_owned()),
            support: config
                .server
                .admin_contact
                .as_deref()
                .and_then(SupportContact::parse),
        }
    }

    /// Whether anything at all is published — used only for the startup log line, so an operator
    /// who expected delegation and configured nothing sees why nothing is served.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.server.is_none() && self.client_base_url.is_none() && self.support.is_none()
    }
}

fn not_found() -> Response {
    hs_http::error::MatrixError::custom(
        axum::http::StatusCode::NOT_FOUND,
        hs_http::error::MatrixErrorCode::NotFound,
        "This server does not publish this .well-known document",
    )
    .into_response()
}

fn with_cors(response: Response) -> Response {
    let mut response = response;
    response.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    response
}

/// `GET /.well-known/matrix/server` — federation delegation, or 404 when not configured.
///
/// Reads the documents through a [`hs_config::Live`] cell, so a change to
/// `server.well_known_server` or `server.public_baseurl` is served on the next request.
pub async fn get_server(
    axum::Extension(well_known): axum::Extension<hs_config::Live<WellKnown>>,
) -> Response {
    let well_known = well_known.get();
    match &well_known.server {
        Some(delegate) => with_cors(Json(json!({ "m.server": delegate })).into_response()),
        None => not_found(),
    }
}

/// `GET /.well-known/matrix/client` — client discovery, or 404 when `public_baseurl` is unset.
pub async fn get_client(
    axum::Extension(well_known): axum::Extension<hs_config::Live<WellKnown>>,
) -> Response {
    let well_known = well_known.get();
    match &well_known.client_base_url {
        Some(base_url) => {
            with_cors(Json(json!({ "m.homeserver": { "base_url": base_url } })).into_response())
        }
        None => not_found(),
    }
}

/// `GET /.well-known/matrix/support` — who to ask for help, from `server.admin_contact`, or 404
/// when it is unset.
pub async fn get_support(
    axum::Extension(well_known): axum::Extension<hs_config::Live<WellKnown>>,
) -> Response {
    let well_known = well_known.get();
    match &well_known.support {
        Some(contact) => with_cors(Json(contact.document()).into_response()),
        None => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Extension;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(well_known: WellKnown) -> axum::Router {
        axum::Router::new()
            .route("/.well-known/matrix/server", get(get_server))
            .route("/.well-known/matrix/client", get(get_client))
            .route("/.well-known/matrix/support", get(get_support))
            .layer(Extension(hs_config::Live::new(well_known)))
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn server_document_is_the_configured_delegation() {
        let response = app(WellKnown {
            server: Some("matrix.example.org:8448".into()),
            client_base_url: None,
            support: None,
        })
        .oneshot(
            Request::builder()
                .uri("/.well-known/matrix/server")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        assert_eq!(
            body_json(response).await,
            json!({"m.server": "matrix.example.org:8448"})
        );
    }

    #[tokio::test]
    async fn server_document_is_absent_when_not_delegating() {
        let response = app(WellKnown::default())
            .oneshot(
                Request::builder()
                    .uri("/.well-known/matrix/server")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(response).await["errcode"], "M_NOT_FOUND");
    }

    #[tokio::test]
    async fn client_document_carries_the_public_base_url() {
        let response = app(WellKnown {
            server: None,
            client_base_url: Some("https://matrix.example.org".into()),
            support: None,
        })
        .oneshot(
            Request::builder()
                .uri("/.well-known/matrix/client")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({"m.homeserver": {"base_url": "https://matrix.example.org"}})
        );
    }

    #[tokio::test]
    async fn client_document_is_absent_without_a_public_base_url() {
        let response = app(WellKnown::default())
            .oneshot(
                Request::builder()
                    .uri("/.well-known/matrix/client")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn from_config_trims_and_drops_a_trailing_slash() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".into();
        config.server.public_baseurl = Some("https://matrix.example.org/ ".into());
        config.server.well_known_server = Some(" matrix.example.org:8448 ".into());
        let well_known = WellKnown::from_config(&config);
        assert_eq!(
            well_known.client_base_url.as_deref(),
            Some("https://matrix.example.org")
        );
        assert_eq!(
            well_known.server.as_deref(),
            Some("matrix.example.org:8448")
        );
        assert!(!well_known.is_empty());
    }

    #[test]
    fn from_config_treats_an_empty_string_as_unset() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".into();
        config.server.public_baseurl = Some("   ".into());
        let well_known = WellKnown::from_config(&config);
        assert!(well_known.is_empty());
    }

    #[tokio::test]
    async fn support_document_names_the_admin_contact_and_is_absent_without_one() {
        let get = |well_known: WellKnown| async move {
            app(well_known)
                .oneshot(
                    Request::builder()
                        .uri("/.well-known/matrix/support")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        };
        let response = get(WellKnown::default()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = get(WellKnown {
            support: SupportContact::parse("mailto:abuse@example.org"),
            ..WellKnown::default()
        })
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({"contacts": [{"role": "m.role.admin", "email_address": "abuse@example.org"}]})
        );
    }

    #[test]
    fn an_admin_contact_reads_as_an_email_a_matrix_id_or_a_page() {
        assert_eq!(
            SupportContact::parse(" mailto:a@example.org "),
            Some(SupportContact::Email("a@example.org".into()))
        );
        assert_eq!(
            SupportContact::parse("a@example.org"),
            Some(SupportContact::Email("a@example.org".into()))
        );
        assert_eq!(
            SupportContact::parse("@admin:example.org"),
            Some(SupportContact::MatrixId("@admin:example.org".into()))
        );
        assert_eq!(
            SupportContact::parse("https://example.org/help"),
            Some(SupportContact::Page("https://example.org/help".into()))
        );
        assert_eq!(
            SupportContact::parse("@admin:example.org")
                .unwrap()
                .document(),
            json!({"contacts": [{"role": "m.role.admin", "matrix_id": "@admin:example.org"}]})
        );
        assert_eq!(
            SupportContact::parse("https://example.org/help")
                .unwrap()
                .document(),
            json!({"support_page": "https://example.org/help"})
        );
        for nothing in ["", "   ", "mailto:", "call the front desk", "@not a user"] {
            assert_eq!(SupportContact::parse(nothing), None, "{nothing:?}");
        }
    }
}
