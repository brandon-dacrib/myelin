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
//! | `GET /.well-known/matrix/server` | `server.well_known_server` is set, or derived from an `https://` `server.public_baseurl` (decision 0040) | `{"m.server": "<host[:port]>"}` |
//! | `GET /.well-known/matrix/client` | `server.public_baseurl` is set | `{"m.homeserver": {"base_url": "<url>"}}` |
//! | `GET /.well-known/matrix/support` | `server.admin_contact` is set | `{"contacts": [{"role": "m.role.admin", "email_address" or "matrix_id": ...}]}`, or `{"support_page": "<url>"}` |
//!
//! The support document (spec v1.10, MSC1929) is what clients read to tell people who to ask for
//! help or report abuse to; `server.admin_contact` is a `mailto:` address, a bare email address,
//! a Matrix ID, or an `http(s)://` support page ([`SupportContact::parse`]).
//!
//! # The server document's default (decision 0040)
//!
//! `server.well_known_server` unset no longer means "not served". When `server.public_baseurl`
//! is an `https://` URL, the server document is **derived** from it: its host, and its explicit
//! port or 443 (`https://matrix.example.org` publishes `m.server: matrix.example.org:443`,
//! `https://matrix.example.org:8448/` publishes `matrix.example.org:8448`). The reasoning: an
//! operator who set a public base URL has said where this server is reachable over TLS, and a
//! server nobody can discover is not federating. The demo found this the hard way on 2026-10-10:
//! it published a client document and no server document, 8448 was closed, and every signed
//! request it made was answered `401 Failed to find any key to satisfy ...` because no remote
//! server could fetch its signing key. [`ServerSource`] says, for the boot log, where the
//! published value came from and, when nothing is published, why:
//!
//! - `server.well_known_server` set to a `host[:port]` wins over the derivation.
//! - `server.well_known_server` set to the empty string turns the document off (the one way to
//!   say "I serve this document somewhere else, or not at all" when a public base URL is set).
//! - An `http://` public base URL derives nothing: federation needs TLS, so a document naming an
//!   `http://` host would send remote servers to a port that cannot answer them.
//! - `federation.enabled: false` derives nothing: there is no federation to point at.
//!
//! The documents are otherwise **absent** (404 `M_NOT_FOUND`, the same answer as any unrouted
//! path) when their config field is unset, rather than being served with a value derived from
//! `server_name`. A well-known document that points at the server name it was fetched from is
//! indistinguishable from no document at all in the spec's resolution order (both end at
//! "connect to the server name"), so that one adds a failure mode without adding a capability.
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

/// The `.well-known` values this server publishes about itself, read from configuration at
/// startup and again on every change to the `server` section. Cheap to clone, held by the
/// router's handlers through an [`hs_config::Live`] cell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WellKnown {
    /// The `host[:port]` remote servers should connect to for federation: `server.well_known_server`
    /// when set, else derived from `server.public_baseurl` ([`ServerSource`] says which). `None`
    /// means `/.well-known/matrix/server` is not served.
    pub server: Option<String>,
    /// Where [`Self::server`] came from, or why it is `None`; for the boot log.
    pub server_source: ServerSource,
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

/// Where the server document's value came from, or why there is none. Printed in the boot log
/// and when the `server` section is reloaded, so an operator reading "no server document" also
/// reads what to set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerSource {
    /// `server.well_known_server` is set to a `host[:port]`.
    Configured,
    /// `server.well_known_server` is unset and the value is `server.public_baseurl`'s host and
    /// port (its explicit port, or 443).
    DerivedFromPublicBaseUrl,
    /// `server.well_known_server` is the empty string: the operator turned the document off.
    Disabled,
    /// Neither `server.well_known_server` nor `server.public_baseurl` is set; nothing to publish
    /// and nothing to derive it from.
    #[default]
    NothingToDeriveFrom,
    /// `server.public_baseurl` is `http://`, and federation needs TLS.
    PublicBaseUrlIsNotHttps,
    /// `federation.enabled` is false: there is no federation to point remote servers at.
    FederationDisabled,
}

impl ServerSource {
    /// Whether a document is published.
    #[must_use]
    pub fn is_published(self) -> bool {
        matches!(self, Self::Configured | Self::DerivedFromPublicBaseUrl)
    }
}

impl std::fmt::Display for ServerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configured => "set (server.well_known_server)",
            Self::DerivedFromPublicBaseUrl => "derived from server.public_baseurl",
            Self::Disabled => "off (server.well_known_server is the empty string)",
            Self::NothingToDeriveFrom => {
                "not published: set server.public_baseurl (an https:// URL) to derive it, or \
                 server.well_known_server to name the federation host[:port]"
            }
            Self::PublicBaseUrlIsNotHttps => {
                "not published: server.public_baseurl is not an https:// URL with a host, and \
                 federation needs TLS; set server.well_known_server to name the federation \
                 host[:port] to advertise"
            }
            Self::FederationDisabled => {
                "not published: federation.enabled is false, so there is nothing to advertise"
            }
        })
    }
}

/// The `m.server` value a public base URL implies: its host, and its explicit port or 443.
/// `None` for anything but an `https://` URL with a host (federation needs TLS; a document naming
/// an `http://` host would send remote servers to a port that cannot answer them).
///
/// Only the authority is read: a path (`https://example.org/matrix`), a query or a fragment is
/// ignored, as is userinfo. An IPv6 literal keeps its brackets (`[2001:db8::1]:443`), which is
/// how the spec spells a `host:port` for one.
#[must_use]
pub fn derive_server_document(public_baseurl: &str) -> Option<String> {
    let rest = public_baseurl.trim().strip_prefix("https://")?;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (inner, after) = after_bracket.split_once(']')?;
        let host = format!("[{inner}]");
        let port = after.strip_prefix(':').map(str::to_owned);
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port.to_owned())),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse::<u16>().ok()?.to_string(),
        None => "443".to_owned(),
    };
    Some(format!("{host}:{port}"))
}

impl WellKnown {
    /// Reads the three documents from a native configuration. The server document is the
    /// explicit `server.well_known_server`, else derived from `server.public_baseurl`
    /// ([`derive_server_document`]) when federation is on; `server_source` records which.
    #[must_use]
    pub fn from_config(config: &hs_config::Config) -> Self {
        let public_baseurl = config
            .server
            .public_baseurl
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (server, server_source) = match config.server.well_known_server.as_deref() {
            Some(explicit) if !explicit.trim().is_empty() => {
                (Some(explicit.trim().to_owned()), ServerSource::Configured)
            }
            Some(_) => (None, ServerSource::Disabled),
            None => match public_baseurl {
                None => (None, ServerSource::NothingToDeriveFrom),
                Some(_) if !config.federation.enabled => (None, ServerSource::FederationDisabled),
                Some(url) => match derive_server_document(url) {
                    Some(derived) => (Some(derived), ServerSource::DerivedFromPublicBaseUrl),
                    None => (None, ServerSource::PublicBaseUrlIsNotHttps),
                },
            },
        };
        Self {
            server,
            server_source,
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
            server_source: ServerSource::Configured,
            ..WellKnown::default()
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
            client_base_url: Some("https://matrix.example.org".into()),
            ..WellKnown::default()
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
        assert_eq!(well_known.server_source, ServerSource::Configured);
        assert!(!well_known.is_empty());
    }

    /// Decision 0040: with a public base URL and no explicit delegation, the server document is
    /// the base URL's host and port.
    #[test]
    fn from_config_derives_the_server_document_from_an_https_public_base_url() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".into();
        config.server.public_baseurl = Some("https://myelin.dacrib.net".into());
        let well_known = WellKnown::from_config(&config);
        assert_eq!(well_known.server.as_deref(), Some("myelin.dacrib.net:443"));
        assert_eq!(
            well_known.server_source,
            ServerSource::DerivedFromPublicBaseUrl
        );
        assert!(well_known.server_source.is_published());

        config.server.public_baseurl = Some("https://matrix.example.org:8448/".into());
        let well_known = WellKnown::from_config(&config);
        assert_eq!(
            well_known.server.as_deref(),
            Some("matrix.example.org:8448")
        );
    }

    #[test]
    fn from_config_publishes_no_server_document_for_an_http_public_base_url() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".into();
        config.server.public_baseurl = Some("http://localhost:8008".into());
        let well_known = WellKnown::from_config(&config);
        assert_eq!(well_known.server, None);
        assert_eq!(
            well_known.server_source,
            ServerSource::PublicBaseUrlIsNotHttps
        );
        assert!(!well_known.server_source.is_published());
        // The client document is unaffected: clients talk to http://localhost happily.
        assert_eq!(
            well_known.client_base_url.as_deref(),
            Some("http://localhost:8008")
        );
    }

    #[test]
    fn from_config_lets_an_empty_well_known_server_turn_the_document_off() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".into();
        config.server.public_baseurl = Some("https://matrix.example.org".into());
        config.server.well_known_server = Some(String::new());
        let well_known = WellKnown::from_config(&config);
        assert_eq!(well_known.server, None);
        assert_eq!(well_known.server_source, ServerSource::Disabled);

        config.server.well_known_server = Some("   ".into());
        assert_eq!(
            WellKnown::from_config(&config).server_source,
            ServerSource::Disabled
        );
    }

    #[test]
    fn from_config_derives_nothing_without_federation_or_a_public_base_url() {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".into();
        assert_eq!(
            WellKnown::from_config(&config).server_source,
            ServerSource::NothingToDeriveFrom
        );

        config.server.public_baseurl = Some("https://matrix.example.org".into());
        config.federation.enabled = false;
        let well_known = WellKnown::from_config(&config);
        assert_eq!(well_known.server, None);
        assert_eq!(well_known.server_source, ServerSource::FederationDisabled);

        // An explicit delegation is published whatever federation.enabled says: the operator
        // asked for exactly this document.
        config.server.well_known_server = Some("matrix.example.org:8448".into());
        assert_eq!(
            WellKnown::from_config(&config).server_source,
            ServerSource::Configured
        );
    }

    #[test]
    fn the_derivation_reads_the_authority_only() {
        for (url, want) in [
            ("https://example.org", Some("example.org:443")),
            ("https://example.org/", Some("example.org:443")),
            ("https://example.org:8448", Some("example.org:8448")),
            (
                "https://example.org:8448/matrix/?x=1#y",
                Some("example.org:8448"),
            ),
            ("https://user:pw@example.org", Some("example.org:443")),
            ("https://[2001:db8::1]", Some("[2001:db8::1]:443")),
            ("https://[2001:db8::1]:8448/", Some("[2001:db8::1]:8448")),
            ("  https://example.org  ", Some("example.org:443")),
            ("http://example.org", None),
            ("https://", None),
            ("https:///path", None),
            ("https://example.org:notaport", None),
            ("https://[2001:db8::1", None),
            ("example.org", None),
        ] {
            assert_eq!(derive_server_document(url).as_deref(), want, "{url:?}");
        }
    }

    #[test]
    fn every_source_says_where_the_value_came_from_or_what_to_set() {
        assert_eq!(
            ServerSource::DerivedFromPublicBaseUrl.to_string(),
            "derived from server.public_baseurl"
        );
        assert_eq!(
            ServerSource::Configured.to_string(),
            "set (server.well_known_server)"
        );
        for unpublished in [
            ServerSource::Disabled,
            ServerSource::NothingToDeriveFrom,
            ServerSource::PublicBaseUrlIsNotHttps,
            ServerSource::FederationDisabled,
        ] {
            assert!(!unpublished.is_published());
            assert!(!unpublished.to_string().is_empty());
        }
        assert!(
            ServerSource::NothingToDeriveFrom
                .to_string()
                .contains("server.public_baseurl")
        );
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
