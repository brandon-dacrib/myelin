//! The `X-Matrix` request-authentication scheme, both directions, and the axum middleware that
//! enforces it over the whole federation router.
//!
//! Written from `docs/design/06-federation-threat-model.md` section 2.3 and the plan in
//! `docs/status/06-federation.md` (item 4). Per the recorded decision there, [`XMatrixLayer`] is
//! applied exactly once, over the whole composed federation router
//! (`Router::layer`/`route_layer`), never per-handler — see `crate::transport` for where it is
//! wired in, and this module's own tests for the guarantee that a request never reaches a handler
//! without passing through [`verify_x_matrix`] first.
//!
//! # The signed object
//!
//! Per the server-server API's "Request Authentication" section, the bytes signed are the
//! canonical JSON form of:
//!
//! ```json
//! {
//!   "method": "POST",
//!   "uri": "/_matrix/federation/v1/send/1",
//!   "origin": "sending.example.com",
//!   "destination": "receiving.example.com",
//!   "content": { ... }
//! }
//! ```
//!
//! `content` is omitted entirely (not `null`) when the request has no body (`GET`, `DELETE`, most
//! reads). `uri` is the request target exactly as sent on the wire (path + query string).
//! `destination` is required by the current spec revision; this implementation rejects a request
//! that omits it rather than guessing an older peer's intent (decision recorded in
//! `docs/status/06-federation.md`).

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::Extension;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_model::signing::{self, SigningKeyPair};

use crate::keys::{DynRemoteKeyCache, KeyLookupError};

/// Max inbound federation request body size before JSON parsing / signature verification begins
/// (threat model section 3: 50 MiB, backstopping `/state`, `/backfill`-shaped bodies; enforced
/// independent of any `Content-Length` the peer claims, by bounding the actual bytes read).
pub const MAX_REQUEST_BODY_BYTES: usize = 50 * 1024 * 1024;

/// A parsed `Authorization: X-Matrix ...` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XMatrixAuth {
    pub origin: String,
    pub destination: String,
    pub key_id: String,
    pub sig: String,
}

/// Errors parsing or validating an `X-Matrix` header, independent of signature verification
/// itself (which needs key material and is therefore async — see [`VerifyError`]).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum XMatrixParseError {
    #[error("missing Authorization header")]
    Missing,
    #[error("multiple Authorization headers")]
    Multiple,
    #[error("Authorization header is not valid UTF-8")]
    NotUtf8,
    #[error("Authorization scheme is not X-Matrix")]
    WrongScheme,
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
}

/// Parses exactly one `Authorization` header out of `headers`, requiring the `X-Matrix` scheme
/// and all four fields (`origin`, `destination`, `key`, `sig`) to be present. Tolerant of field
/// order and of both quoted and bare (unquoted) values, per real-world peer variance; rejects
/// (rather than silently accepting) a header with more than one `Authorization` value, a header
/// using another scheme, or one missing a required field — see the threat model's "confuse a
/// naive parser" threat.
pub fn parse_x_matrix_header(headers: &HeaderMap) -> Result<XMatrixAuth, XMatrixParseError> {
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let Some(first) = values.next() else {
        return Err(XMatrixParseError::Missing);
    };
    if values.next().is_some() {
        return Err(XMatrixParseError::Multiple);
    }
    let value = first.to_str().map_err(|_| XMatrixParseError::NotUtf8)?;
    let rest = value
        .strip_prefix("X-Matrix ")
        .or_else(|| value.strip_prefix("X-Matrix"))
        .ok_or(XMatrixParseError::WrongScheme)?;

    let fields = split_auth_params(rest.trim_start());

    let mut origin = None;
    let mut destination = None;
    let mut key_id = None;
    let mut sig = None;
    for (name, val) in fields {
        match name {
            "origin" => origin = Some(val),
            "destination" => destination = Some(val),
            "key" => key_id = Some(val),
            "sig" => sig = Some(val),
            _ => {}
        }
    }

    Ok(XMatrixAuth {
        origin: origin.ok_or(XMatrixParseError::MissingField("origin"))?,
        destination: destination.ok_or(XMatrixParseError::MissingField("destination"))?,
        key_id: key_id.ok_or(XMatrixParseError::MissingField("key"))?,
        sig: sig.ok_or(XMatrixParseError::MissingField("sig"))?,
    })
}

/// Splits `name="value",name2=value2` (or any mix of quoted/bare values) into `(name, value)`
/// pairs, honouring `\"` and `\\` escapes inside quoted strings (RFC 7235 `quoted-string`) and
/// tolerant of extra whitespace around commas.
fn split_auth_params(input: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let mut rest = input;
    loop {
        let rest_trimmed = rest.trim_start_matches([' ', ',']);
        if rest_trimmed.is_empty() {
            break;
        }
        let Some(eq) = rest_trimmed.find('=') else {
            break;
        };
        let name = &rest_trimmed[..eq];
        let after_eq = &rest_trimmed[eq + 1..];
        if let Some(quoted) = after_eq.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            let mut escaped = false;
            for (i, c) in &mut chars {
                if escaped {
                    value.push(c);
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == '"' {
                    end = i;
                    break;
                }
                value.push(c);
            }
            out.push((name, value));
            rest = &quoted[(end + 1).min(quoted.len())..];
        } else {
            let end = after_eq.find(',').unwrap_or(after_eq.len());
            out.push((name, after_eq[..end].to_string()));
            rest = &after_eq[end..];
        }
    }
    out
}

/// Builds the `Authorization: X-Matrix ...` header value for an outbound request.
#[must_use]
pub fn build_x_matrix_header(auth: &XMatrixAuth) -> String {
    format!(
        "X-Matrix origin=\"{}\",destination=\"{}\",key=\"{}\",sig=\"{}\"",
        escape_quoted(&auth.origin),
        escape_quoted(&auth.destination),
        escape_quoted(&auth.key_id),
        escape_quoted(&auth.sig)
    )
}

fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Builds the exact JSON object that gets signed/verified: `{method, uri, origin, destination,
/// content?}`, `content` present only when `content` is `Some`.
#[must_use]
pub fn signing_object(
    method: &str,
    uri: &str,
    origin: &str,
    destination: &str,
    content: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "method": method,
        "uri": uri,
        "origin": origin,
        "destination": destination,
    });
    if let Some(c) = content {
        obj["content"] = c.clone();
    }
    obj
}

/// Signs an outbound federation request, returning the `Authorization` header value to send.
///
/// # Errors
/// Returns a [`hs_model::error::SigningError`] if canonicalization fails (only possible if `content`
/// contains a non-canonical number).
pub fn sign_request(
    method: &str,
    uri: &str,
    origin: &str,
    destination: &str,
    content: Option<&serde_json::Value>,
    key: &SigningKeyPair,
) -> Result<String, hs_model::error::SigningError> {
    let object = signing_object(method, uri, origin, destination, content);
    let mut canonical = signing::to_signable_object(&object)?;
    let origin_ruma = ruma::ServerName::parse(origin)
        .map_err(|_| hs_model::error::SigningError::MissingServerSignature(origin.to_string()))?;
    signing::sign_object(&mut canonical, origin_ruma.as_ref(), key)?;
    let sig = canonical
        .get("signatures")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(origin))
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(&key.key_id()))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
        .expect("sign_object always inserts the signature it just computed")
        .to_string();
    Ok(build_x_matrix_header(&XMatrixAuth {
        origin: origin.to_string(),
        destination: destination.to_string(),
        key_id: key.key_id(),
        sig,
    }))
}

/// Why an inbound request was rejected by [`verify_x_matrix`]. Every variant maps to a
/// [`MatrixError`] via [`IntoResponse`]; the mapping deliberately does not distinguish most
/// failure modes in the response body (a hostile peer probing our verifier should not learn
/// *why* a forged request failed) — variants exist for testability, not for the wire.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "fields are read via the Debug impl in the tracing::debug! call"
)]
enum VerifyError {
    Parse(XMatrixParseError),
    WrongDestination,
    BodyTooLarge,
    BadJsonBody,
    KeyLookup(KeyLookupError),
    SignatureInvalid,
}

impl IntoResponse for VerifyError {
    fn into_response(self) -> Response {
        let err = match &self {
            VerifyError::BodyTooLarge => MatrixError::custom(
                StatusCode::PAYLOAD_TOO_LARGE,
                MatrixErrorCode::TooLarge,
                "request body exceeds the federation size limit",
            ),
            _ => MatrixError::custom(
                StatusCode::UNAUTHORIZED,
                MatrixErrorCode::Unauthorized,
                "signature verification failed",
            ),
        };
        tracing::debug!(reason = ?self, "rejected unauthenticated federation request");
        err.into_response()
    }
}

/// Shared state the verification middleware needs: this server's own name (to check the
/// `destination` field) and a key cache to resolve `origin`'s verifying key.
pub struct XMatrixContext {
    pub own_server_name: String,
    pub key_cache: Arc<DynRemoteKeyCache>,
}

/// The axum middleware itself. Wire it with
/// `.layer(axum::middleware::from_fn(verify_x_matrix)).layer(Extension(Arc::new(ctx)))` — **in
/// that order**: `Router::layer` wraps outside-in, so the layer added *last* runs *first*; the
/// `Extension` layer must run before `verify_x_matrix`'s own `Extension` extractor does, or the
/// context is missing and every request is rejected. See `crate::transport` for where this is
/// wired over the whole composed federation router, never per-handler.
///
/// On success, forwards the request unchanged (with its body reconstructed from the bytes this
/// function had to buffer to verify the signature) to `next`. On failure, short-circuits with a
/// Matrix-shaped error response — the handler never runs.
pub async fn verify_x_matrix(
    Extension(ctx): Extension<Arc<XMatrixContext>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    match do_verify(&ctx, req).await {
        Ok(req) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

async fn do_verify(ctx: &XMatrixContext, req: Request<Body>) -> Result<Request<Body>, VerifyError> {
    let auth = parse_x_matrix_header(req.headers()).map_err(VerifyError::Parse)?;
    if auth.destination != ctx.own_server_name {
        return Err(VerifyError::WrongDestination);
    }

    let method = req.method().as_str().to_string();
    let uri = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().to_string());

    let (parts, body) = req.into_parts();
    let bytes: Bytes = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|_| VerifyError::BodyTooLarge)?;

    let content: Option<serde_json::Value> = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).map_err(|_| VerifyError::BadJsonBody)?)
    };

    let verifying_key = ctx
        .key_cache
        .get_current(&auth.origin, &auth.key_id)
        .await
        .map_err(VerifyError::KeyLookup)?;

    let mut object = signing_object(
        &method,
        &uri,
        &auth.origin,
        &auth.destination,
        content.as_ref(),
    );
    object["signatures"] =
        serde_json::json!({ auth.origin.clone(): { auth.key_id.clone(): auth.sig.clone() } });
    let canonical =
        signing::to_signable_object(&object).map_err(|_| VerifyError::SignatureInvalid)?;
    signing::verify_object(&canonical, &auth.origin, &auth.key_id, &verifying_key)
        .map_err(|_| VerifyError::SignatureInvalid)?;

    Ok(Request::from_parts(parts, Body::from(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{
        KeyServerFetcher, OwnSigningKeys, RemoteKeyCache, build_server_key_response,
    };
    use async_trait::async_trait;
    use axum::Router;
    use axum::http::header::AUTHORIZATION;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    struct FixedFetcher(std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>);
    impl FixedFetcher {
        fn new() -> Self {
            Self(std::sync::Mutex::new(std::collections::HashMap::new()))
        }
        fn set(&self, server: &str, doc: serde_json::Value) {
            self.0.lock().unwrap().insert(server.to_string(), doc);
        }
    }
    #[async_trait]
    impl KeyServerFetcher for FixedFetcher {
        async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
            self.0.lock().unwrap().get(server_name).cloned()
        }
    }

    fn origin_keys() -> (OwnSigningKeys, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        (keys, dir)
    }

    fn test_app(ctx: Arc<XMatrixContext>) -> Router {
        async fn ok_handler() -> &'static str {
            "ok"
        }
        async fn echo_handler(body: Bytes) -> Bytes {
            body
        }
        Router::new()
            .route("/_matrix/federation/v1/version", get(ok_handler))
            .route("/_matrix/federation/v1/send/{txn}", post(echo_handler))
            .layer(axum::middleware::from_fn(verify_x_matrix))
            .layer(Extension(ctx))
    }

    async fn cache_with_origin(origin: &str, keys: &OwnSigningKeys) -> Arc<DynRemoteKeyCache> {
        let fetcher = FixedFetcher::new();
        let doc = build_server_key_response(origin, keys, &[], 3600).unwrap();
        fetcher.set(origin, doc);
        Arc::new(RemoteKeyCache::new(
            Box::new(fetcher) as Box<dyn KeyServerFetcher>
        ))
    }

    #[tokio::test]
    async fn valid_signed_get_request_is_accepted() {
        let (keys, _dir) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        let header = sign_request(
            "GET",
            "/_matrix/federation/v1/version",
            "origin.example.org",
            "dest.example.org",
            None,
            keys.primary(),
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_matrix/federation/v1/version")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_signature_is_rejected() {
        let (keys, _dir) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_matrix/federation/v1/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tampered_body_is_rejected() {
        let (keys, _dir) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        let content = serde_json::json!({"pdus": []});
        let header = sign_request(
            "POST",
            "/_matrix/federation/v1/send/1",
            "origin.example.org",
            "dest.example.org",
            Some(&content),
            keys.primary(),
        )
        .unwrap();

        // Send a *different* body than what was signed.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_matrix/federation/v1/send/1")
                    .header(AUTHORIZATION, header)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"pdus": [1, 2, 3]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_destination_is_rejected() {
        let (keys, _dir) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        let header = sign_request(
            "GET",
            "/_matrix/federation/v1/version",
            "origin.example.org",
            "someone-else.example.org",
            None,
            keys.primary(),
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_matrix/federation/v1/version")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_key_is_rejected() {
        let (keys, _dir) = origin_keys();
        let (other_keys, _dir2) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        // Signed with a key that is not the one cached for origin.example.org.
        let header = sign_request(
            "GET",
            "/_matrix/federation/v1/version",
            "origin.example.org",
            "dest.example.org",
            None,
            other_keys.primary(),
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_matrix/federation/v1/version")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn replayed_request_to_a_different_route_is_rejected() {
        // The signature covers `uri`, so replaying a validly-signed GET /version request body+sig
        // against a different path must fail (method/uri binding, not just "any valid sig").
        let (keys, _dir) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        let header = sign_request(
            "GET",
            "/_matrix/federation/v1/version",
            "origin.example.org",
            "dest.example.org",
            None,
            keys.primary(),
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_matrix/federation/v1/send/1")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_origin_is_rejected_without_a_cached_key() {
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache: Arc::new(RemoteKeyCache::new(
                Box::new(FixedFetcher::new()) as Box<dyn KeyServerFetcher>
            )),
        });
        let app = test_app(ctx);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_matrix/federation/v1/version")
                    .header(
                        AUTHORIZATION,
                        "X-Matrix origin=\"nowhere.example.org\",destination=\"dest.example.org\",key=\"ed25519:a_1\",sig=\"AAAA\"",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn multiple_authorization_headers_are_rejected() {
        let (keys, _dir) = origin_keys();
        let key_cache = cache_with_origin("origin.example.org", &keys).await;
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "dest.example.org".to_string(),
            key_cache,
        });
        let app = test_app(ctx);

        let header = sign_request(
            "GET",
            "/_matrix/federation/v1/version",
            "origin.example.org",
            "dest.example.org",
            None,
            keys.primary(),
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_matrix/federation/v1/version")
                    .header(AUTHORIZATION, header.clone())
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // --- Header parsing unit tests (no network/router involved) --------------------------------

    #[test]
    fn parses_quoted_fields_in_any_order() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            "X-Matrix key=\"ed25519:1\",sig=\"abc\",origin=\"a.org\",destination=\"b.org\""
                .parse()
                .unwrap(),
        );
        let auth = parse_x_matrix_header(&headers).unwrap();
        assert_eq!(auth.origin, "a.org");
        assert_eq!(auth.destination, "b.org");
        assert_eq!(auth.key_id, "ed25519:1");
        assert_eq!(auth.sig, "abc");
    }

    #[test]
    fn rejects_wrong_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer sometoken".parse().unwrap());
        assert_eq!(
            parse_x_matrix_header(&headers).unwrap_err(),
            XMatrixParseError::WrongScheme
        );
    }

    #[test]
    fn rejects_missing_field() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            "X-Matrix origin=\"a.org\",key=\"ed25519:1\",sig=\"abc\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            parse_x_matrix_header(&headers).unwrap_err(),
            XMatrixParseError::MissingField("destination")
        );
    }

    #[test]
    fn round_trips_sign_and_parse() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let header = sign_request(
            "PUT",
            "/_matrix/federation/v1/send/abc",
            "a.org",
            "b.org",
            Some(&serde_json::json!({"x": 1})),
            keys.primary(),
        )
        .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, header.parse().unwrap());
        let auth = parse_x_matrix_header(&headers).unwrap();
        assert_eq!(auth.origin, "a.org");
        assert_eq!(auth.destination, "b.org");
        assert_eq!(auth.key_id, keys.primary().key_id());
    }
}
