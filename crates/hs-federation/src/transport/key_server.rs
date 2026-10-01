//! The key server: `GET /_matrix/key/v2/server` (and its deprecated `/server/{keyId}` spelling)
//! and the notary endpoints `POST /_matrix/key/v2/query` and `GET /_matrix/key/v2/query/{serverName}`
//! (with the older `/query/{serverName}/{keyId}` spelling Sytest still uses).
//!
//! Every route here answers an **unsigned** request: a server fetches these to learn the keys it
//! needs to check signatures in the first place, so they live outside the `X-Matrix` layer
//! [`crate::transport::router`] applies. [`router`] builds them as their own fragment, for
//! mounting at `/_matrix/key/v2`.
//!
//! The notary answers from [`crate::keys::RemoteKeyCache`] -- the same cache inbound
//! `X-Matrix` verification fills -- and co-signs each origin's own self-signed response with
//! this server's key ([`crate::keys::wrap_for_notary`]), as the spec's "Querying keys through
//! another server" requires. Keys asked about for this server itself are answered with its own
//! freshly signed response.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use serde_json::Value;

use crate::keys::{
    DynRemoteKeyCache, MAX_NOTARY_SERVERS_PER_QUERY, OldVerifyKey, OwnSigningKeys,
    build_server_key_response, wrap_for_notary,
};

/// What the key server answers from.
#[derive(Clone)]
pub struct KeyServerState {
    /// This server's name, as its key responses name it.
    pub server_name: Arc<str>,
    /// The keys this server signs with and publishes.
    pub own_keys: Arc<OwnSigningKeys>,
    /// Keys this server used to sign with, published in `old_verify_keys`.
    pub old_keys: Arc<[OldVerifyKey]>,
    /// How long, in seconds, a key response this server signs claims to stay valid.
    pub valid_for_secs: u64,
    /// The cache of other servers' keys the notary answers from.
    pub cache: Arc<DynRemoteKeyCache>,
}

impl KeyServerState {
    fn own_response(&self) -> Result<Value, Box<MatrixError>> {
        build_server_key_response(
            &self.server_name,
            &self.own_keys,
            &self.old_keys,
            self.valid_for_secs,
        )
        .map_err(|error| {
            tracing::error!(%error, "could not sign this server's key response");
            Box::new(MatrixError::custom(
                StatusCode::INTERNAL_SERVER_ERROR,
                MatrixErrorCode::Unknown,
                "could not sign the server key response",
            ))
        })
    }

    /// The `server_keys` entries for one server: this server's own response, or the notary's
    /// co-signed copies of what the cache holds for another.
    async fn server_keys_for(
        &self,
        server_name: &str,
        key_ids: &[String],
        minimum_valid_until_ts: u64,
    ) -> Vec<Value> {
        if server_name == &*self.server_name {
            return self.own_response().map(|doc| vec![doc]).unwrap_or_default();
        }
        let held = self
            .cache
            .notary_responses(server_name, key_ids, minimum_valid_until_ts)
            .await;
        crate::metrics::record_notary_answer(!held.is_empty());
        held.iter()
            .filter_map(
                |doc| match wrap_for_notary(doc, &self.server_name, &self.own_keys) {
                    Ok(signed) => Some(signed),
                    Err(error) => {
                        tracing::warn!(
                            server = server_name,
                            %error,
                            "notary: could not co-sign a held key response"
                        );
                        None
                    }
                },
            )
            .collect()
    }
}

fn meta(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixFederation, AuthKind::None).with_operation_id(operation_id)
}

/// The key server's routes, relative to `/_matrix/key/v2`, with no `X-Matrix` layer (see the
/// module doc).
pub fn router(state: KeyServerState) -> (axum::Router, RouteManifest) {
    let (router, manifest) = Builder::<KeyServerState>::new()
        .get("/server", server_key, meta("getServerKey"))
        .get(
            "/server/{keyId}",
            server_key_by_id,
            meta("getServerKeyById"),
        )
        .add(
            Method::POST,
            "/query",
            query_keys,
            meta("perspectivesKeysQuery"),
        )
        .get(
            "/query/{serverName}",
            query_server,
            meta("perspectivesKeysQueryServer"),
        )
        .get(
            "/query/{serverName}/{keyId}",
            query_server_key,
            meta("perspectivesKeysQueryServerKey"),
        )
        .build();
    (router.with_state(state), manifest)
}

async fn server_key(State(state): State<KeyServerState>) -> Response {
    match state.own_response() {
        Ok(doc) => axum::Json(doc).into_response(),
        Err(error) => error.into_response(),
    }
}

/// The deprecated `GET /_matrix/key/v2/server/{keyId}`: the same document as `/server`, whatever
/// key it names (the spec: "servers should not use this", but it answers the whole response).
async fn server_key_by_id(state: State<KeyServerState>, Path(_key_id): Path<String>) -> Response {
    server_key(state).await
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn bad_request(message: impl Into<String>) -> Response {
    MatrixError::custom(StatusCode::BAD_REQUEST, MatrixErrorCode::BadJson, message).into_response()
}

/// `POST /_matrix/key/v2/query`: `{"server_keys": {server: {key_id: {"minimum_valid_until_ts":
/// n}}}}`. A server with no key IDs asks for all of its keys; a missing
/// `minimum_valid_until_ts` means now, as the spec says.
async fn query_keys(State(state): State<KeyServerState>, body: axum::body::Bytes) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return bad_request("the body is not JSON"),
    };
    let Some(servers) = parsed.get("server_keys").and_then(Value::as_object) else {
        return bad_request("`server_keys` must be an object");
    };
    if servers.len() > MAX_NOTARY_SERVERS_PER_QUERY {
        return MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::LimitExceeded,
            format!("at most {MAX_NOTARY_SERVERS_PER_QUERY} servers may be asked about at once"),
        )
        .into_response();
    }
    let now = now_ms();
    let mut server_keys = Vec::new();
    for (server_name, criteria) in servers {
        let Some(criteria) = criteria.as_object() else {
            return bad_request(format!("`server_keys.{server_name}` must be an object"));
        };
        let key_ids: Vec<String> = criteria.keys().cloned().collect();
        // The latest any key asked about needs: one fetch then covers them all.
        let minimum = criteria
            .values()
            .map(|c| {
                c.get("minimum_valid_until_ts")
                    .and_then(Value::as_u64)
                    .unwrap_or(now)
            })
            .max()
            .unwrap_or(now);
        server_keys.extend(state.server_keys_for(server_name, &key_ids, minimum).await);
    }
    axum::Json(serde_json::json!({ "server_keys": server_keys })).into_response()
}

#[derive(serde::Deserialize)]
struct MinimumValidUntil {
    #[serde(default)]
    minimum_valid_until_ts: Option<u64>,
}

/// `GET /_matrix/key/v2/query/{serverName}`: every key held for one server.
async fn query_server(
    State(state): State<KeyServerState>,
    Path(server_name): Path<String>,
    Query(params): Query<MinimumValidUntil>,
) -> Response {
    let minimum = params.minimum_valid_until_ts.unwrap_or_else(now_ms);
    let server_keys = state.server_keys_for(&server_name, &[], minimum).await;
    axum::Json(serde_json::json!({ "server_keys": server_keys })).into_response()
}

/// `GET /_matrix/key/v2/query/{serverName}/{keyId}`: the older, deprecated spelling naming one
/// key, which Sytest and older servers still ask.
async fn query_server_key(
    State(state): State<KeyServerState>,
    Path((server_name, key_id)): Path<(String, String)>,
    Query(params): Query<MinimumValidUntil>,
) -> Response {
    let minimum = params.minimum_valid_until_ts.unwrap_or_else(now_ms);
    let server_keys = state
        .server_keys_for(&server_name, &[key_id], minimum)
        .await;
    axum::Json(serde_json::json!({ "server_keys": server_keys })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{KeyServerFetcher, RemoteKeyCache};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use hs_model::signing;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Answers each server's `/server` fetch from a script that can be changed between requests,
    /// and counts the fetches.
    #[derive(Default)]
    struct Scripted {
        answers: Mutex<HashMap<String, Option<Value>>>,
        fetches: Mutex<usize>,
    }

    struct Shared(Arc<Scripted>);

    #[async_trait]
    impl KeyServerFetcher for Shared {
        async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
            *self.0.fetches.lock().unwrap() += 1;
            self.0
                .answers
                .lock()
                .unwrap()
                .get(server_name)
                .cloned()
                .flatten()
        }
    }

    fn keys() -> OwnSigningKeys {
        let dir = tempfile::tempdir().unwrap();
        OwnSigningKeys::load_or_generate(dir.path()).unwrap()
    }

    /// A self-signed response for `server` with one key, valid until `valid_until_ts` (which
    /// may be in the past).
    fn response(server: &str, keys: &OwnSigningKeys, valid_until_ts: u64) -> Value {
        let body = serde_json::json!({
            "server_name": server,
            "verify_keys": keys.verify_keys_json(),
            "old_verify_keys": {},
            "valid_until_ts": valid_until_ts,
        });
        let mut object = signing::to_signable_object(&body).unwrap();
        let name = ruma::ServerName::parse(server).unwrap();
        signing::sign_object(&mut object, name.as_ref(), keys.primary()).unwrap();
        serde_json::from_slice(
            &hs_model::canonical::CanonicalJsonValue::Object(object).to_canonical_bytes(),
        )
        .unwrap()
    }

    struct Fixture {
        notary: Arc<OwnSigningKeys>,
        script: Arc<Scripted>,
        router: axum::Router,
    }

    fn fixture() -> Fixture {
        let notary = Arc::new(keys());
        let script = Arc::new(Scripted::default());
        let cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(Box::new(Shared(
            script.clone(),
        ))
            as Box<dyn KeyServerFetcher>));
        let (router, _) = router(KeyServerState {
            server_name: Arc::from("notary.example.org"),
            own_keys: notary.clone(),
            old_keys: Arc::from(Vec::new()),
            valid_for_secs: 3600,
            cache,
        });
        Fixture {
            notary,
            script,
            router,
        }
    }

    async fn call(router: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Value {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{method} {uri}");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn post_query(server: &str, key_id: &str, minimum: u64) -> Value {
        serde_json::json!({"server_keys": {server: {key_id: {"minimum_valid_until_ts": minimum}}}})
    }

    /// The deprecated key-id form answers exactly what `/server` answers (without it, Sytest's
    /// federation client could not learn this server's key, and most of its federation tests
    /// never started).
    #[tokio::test]
    async fn the_key_id_spelling_answers_the_server_document() {
        let f = fixture();
        let plain = call(&f.router, "GET", "/server", None).await;
        let by_id = call(&f.router, "GET", "/server/ed25519:whatever", None).await;
        assert_eq!(plain["server_name"], "notary.example.org");
        assert_eq!(plain["verify_keys"], by_id["verify_keys"]);
        assert_eq!(by_id["server_name"], "notary.example.org");
        let object = signing::to_signable_object(&by_id).unwrap();
        signing::verify_object(
            &object,
            "notary.example.org",
            &f.notary.primary().key_id(),
            &f.notary.primary().verifying_key(),
        )
        .unwrap();
    }

    /// Both notary spellings fetch the origin's keys and answer its own response, co-signed by
    /// this server and still carrying the origin's signature.
    #[tokio::test]
    async fn the_notary_answers_an_origins_keys_co_signed_by_both_spellings() {
        let f = fixture();
        let origin = keys();
        let far_future = now_ms() + 3_600_000;
        f.script.answers.lock().unwrap().insert(
            "origin.example.org".into(),
            Some(response("origin.example.org", &origin, far_future)),
        );
        let key_id = origin.primary().key_id();
        let by_post = call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("origin.example.org", &key_id, 0)),
        )
        .await;
        let by_get = call(
            &f.router,
            "GET",
            &format!("/query/origin.example.org/{key_id}"),
            None,
        )
        .await;
        let by_get_all = call(&f.router, "GET", "/query/origin.example.org", None).await;
        for answer in [&by_post, &by_get, &by_get_all] {
            let docs = answer["server_keys"].as_array().unwrap();
            assert_eq!(docs.len(), 1, "{answer}");
            let object = signing::to_signable_object(&docs[0]).unwrap();
            signing::verify_object(
                &object,
                "notary.example.org",
                &f.notary.primary().key_id(),
                &f.notary.primary().verifying_key(),
            )
            .unwrap();
            signing::verify_object(
                &object,
                "origin.example.org",
                &key_id,
                &origin.primary().verifying_key(),
            )
            .unwrap();
            assert_eq!(
                docs[0]["signatures"]["notary.example.org"]
                    .as_object()
                    .unwrap()
                    .len(),
                1
            );
        }
        // The first request fetched; the two after it were answered from what was held.
        assert_eq!(*f.script.fetches.lock().unwrap(), 1);
    }

    /// Asked about this server itself, the notary answers its own response.
    #[tokio::test]
    async fn the_notary_answers_for_itself_without_fetching() {
        let f = fixture();
        let answer = call(
            &f.router,
            "POST",
            "/query",
            Some(serde_json::json!({"server_keys": {"notary.example.org": {}}})),
        )
        .await;
        assert_eq!(
            answer["server_keys"][0]["server_name"],
            "notary.example.org"
        );
        assert_eq!(*f.script.fetches.lock().unwrap(), 0);
    }

    /// Sytest's "Key notary server should return an expired key if it can't find any others":
    /// an expired response is refetched when the caller needs a later one, and when the origin
    /// then fails, the expired one is still the answer.
    #[tokio::test]
    async fn an_expired_key_is_answered_when_nothing_newer_can_be_found() {
        let f = fixture();
        let origin = keys();
        let expired = now_ms() - 86_400_000;
        let doc = response("origin.example.org", &origin, expired);
        let key_id = origin.primary().key_id();
        f.script
            .answers
            .lock()
            .unwrap()
            .insert("origin.example.org".into(), Some(doc.clone()));
        let first = call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("origin.example.org", &key_id, 0)),
        )
        .await;
        assert_eq!(first["server_keys"][0]["valid_until_ts"], expired);

        f.script
            .answers
            .lock()
            .unwrap()
            .insert("origin.example.org".into(), None);
        let second = call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("origin.example.org", &key_id, expired + 1000)),
        )
        .await;
        assert_eq!(second["server_keys"][0]["valid_until_ts"], expired);
        assert!(
            second["server_keys"][0]["verify_keys"]
                .get(&key_id)
                .is_some()
        );
        // The second request needed a later key than was held, so it asked the origin again.
        assert_eq!(*f.script.fetches.lock().unwrap(), 2);
    }

    /// Sytest's "Key notary server must not overwrite a valid key with a spurious result from
    /// the origin server" (Synapse issue 5305): a later response listing only another key does
    /// not make the notary forget the first.
    #[tokio::test]
    async fn a_response_listing_another_key_does_not_displace_a_held_one() {
        let f = fixture();
        let first_key = keys();
        let second_key = keys();
        let expiry = now_ms() - 86_400_000;
        let first_id = first_key.primary().key_id();
        f.script.answers.lock().unwrap().insert(
            "origin.example.org".into(),
            Some(response("origin.example.org", &first_key, expiry)),
        );
        call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("origin.example.org", &first_id, 0)),
        )
        .await;
        f.script.answers.lock().unwrap().insert(
            "origin.example.org".into(),
            Some(response("origin.example.org", &second_key, expiry + 1000)),
        );
        call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("origin.example.org", &first_id, now_ms())),
        )
        .await;
        let third = call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("origin.example.org", &first_id, expiry)),
        )
        .await;
        let docs = third["server_keys"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert!(docs[0]["verify_keys"].get(&first_id).is_some(), "{third}");
        // Held fresh enough for the third request: no third fetch.
        assert_eq!(*f.script.fetches.lock().unwrap(), 2);
    }

    /// A server nobody can reach and nothing is held for is left out, not an error.
    #[tokio::test]
    async fn an_unreachable_server_with_nothing_held_is_left_out() {
        let f = fixture();
        let answer = call(
            &f.router,
            "POST",
            "/query",
            Some(post_query("gone.example.org", "ed25519:a", 0)),
        )
        .await;
        assert_eq!(answer["server_keys"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_malformed_query_is_a_bad_request() {
        let f = fixture();
        let response = f
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/query")
                    .body(Body::from(r#"{"server_keys": []}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
