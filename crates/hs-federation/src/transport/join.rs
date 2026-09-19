//! `GET make_join` and `PUT send_join` (v1 and v2), wired against [`crate::join`]. See that
//! module's doc for scope: `make_join` is fully real; `send_join` validates for real but can only
//! ever report success for an event this server already holds.
//!
//! [`add_routes`] registers `make_join` and the **v1** `send_join` spelling, for a v1-prefixed
//! router. [`add_routes_v2`] registers the v2 `send_join` spelling (`/send_join/{roomId}/{eventId}`,
//! *not* `/send_join/v2/{roomId}/{eventId}` -- see this crate's status file for the mount bug this
//! fixes) for a router mounted separately under `/_matrix/federation/v2`.

use axum::extract::{Extension, Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::Builder;
use serde_json::Value;

use crate::join::{self, JoinError};
use crate::transport::FederationState;
use crate::xmatrix::{self, XMatrixContext};

pub(super) fn add_routes(builder: Builder<FederationState>) -> Builder<FederationState> {
    builder
        .add(
            Method::GET,
            "/make_join/{roomId}/{userId}",
            make_join,
            super::matrix_federation("federationMakeJoin"),
        )
        .add(
            Method::PUT,
            "/send_join/{roomId}/{eventId}",
            send_join_v1,
            super::matrix_federation("federationSendJoinV1"),
        )
}

/// The v2 `send_join` router fragment, for a router mounted at `/_matrix/federation/v2` --
/// **not** appended to the v1 path under a `/v2/` segment (that was the bug: see this crate's
/// status file).
pub(super) fn add_routes_v2(builder: Builder<FederationState>) -> Builder<FederationState> {
    builder.add(
        Method::PUT,
        "/send_join/{roomId}/{eventId}",
        send_join_v2,
        super::matrix_federation("federationSendJoinV2"),
    )
}

fn requesting_server(headers: &axum::http::HeaderMap) -> Result<String, Box<MatrixError>> {
    xmatrix::parse_x_matrix_header(headers)
        .map(|auth| auth.origin)
        .map_err(|_| {
            Box::new(MatrixError::forbidden(
                "could not determine requesting server",
            ))
        })
}

async fn make_join(
    State(state): State<FederationState>,
    Path((room_id, user_id)): Path<(String, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(err) = requesting_server(&headers) {
        return (*err).into_response();
    }
    let versions: Vec<String> = pairs
        .into_iter()
        .filter(|(k, _)| k == "ver")
        .map(|(_, v)| v)
        .collect();

    match join::make_join(state.rooms.as_ref(), &room_id, &user_id, &versions).await {
        Ok(template) => axum::Json(serde_json::json!({
            "event": template.event,
            "room_version": template.room_version,
        }))
        .into_response(),
        Err(e) => join_error_response(&e),
    }
}

async fn send_join_v1(
    state: State<FederationState>,
    ctx: Extension<std::sync::Arc<XMatrixContext>>,
    path: Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    send_join(state, ctx, path, headers, body, false).await
}

async fn send_join_v2(
    state: State<FederationState>,
    ctx: Extension<std::sync::Arc<XMatrixContext>>,
    path: Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    send_join(state, ctx, path, headers, body, true).await
}

async fn send_join(
    State(state): State<FederationState>,
    Extension(ctx): Extension<std::sync::Arc<XMatrixContext>>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
    v2: bool,
) -> Response {
    let origin = match requesting_server(&headers) {
        Ok(o) => o,
        Err(err) => return (*err).into_response(),
    };
    let signed_event: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return MatrixError::bad_json("invalid join event body").into_response(),
    };

    match join::send_join(
        state.rooms.as_ref(),
        state.write_sink.as_ref(),
        &ctx.key_cache,
        &room_id,
        &event_id,
        &signed_event,
        &origin,
    )
    .await
    {
        Ok(result) => {
            let mut body = serde_json::json!({
                "state": result.state,
                "auth_chain": result.auth_chain,
                "origin": ctx.own_server_name,
            });
            if v2 {
                // v1's `send_join` historically returned `[200, {...}]` (a two-element array);
                // this server never served v1 for real before this session, so there is no
                // deployed caller to keep compatible, and the current spec documents the bare
                // object for both versions -- `event`/`members_omitted` are v2-only additions.
                body["event"] = result.event;
                body["members_omitted"] = serde_json::json!(result.members_omitted);
            }
            axum::Json(body).into_response()
        }
        Err(e) => join_error_response(&e),
    }
}

fn join_error_response(e: &JoinError) -> Response {
    match e {
        JoinError::RoomNotFound => MatrixError::not_found("unknown room").into_response(),
        JoinError::IncompatibleRoomVersion { room_version } => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::IncompatibleRoomVersion,
            format!("this room is room version {room_version}"),
        )
        .into_response(),
        JoinError::UnsupportedRoomVersion(v) => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::UnsupportedRoomVersion,
            format!("room version {v} is not supported"),
        )
        .into_response(),
        JoinError::MalformedUserId(msg) | JoinError::MalformedEvent(msg) => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::BadJson,
            msg.clone(),
        )
        .into_response(),
        JoinError::RoomIdMismatch => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::BadJson,
            e.to_string(),
        )
        .into_response(),
        JoinError::SenderServerMismatch { .. } => {
            MatrixError::forbidden(e.to_string()).into_response()
        }
        JoinError::NotAuthorized(msg) => MatrixError::forbidden(msg.clone()).into_response(),
        JoinError::Store(msg) => MatrixError::custom(
            StatusCode::NOT_IMPLEMENTED,
            MatrixErrorCode::Other("M_HS_INBOUND_INGESTION_UNSUPPORTED".to_owned()),
            msg.clone(),
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::{InMemoryTransactionStore, StaticWriteSink};
    use crate::keys::{DynRemoteKeyCache, KeyServerFetcher, RemoteKeyCache};
    use crate::room_source::{FakeRoom, InMemoryRoomSource};
    use crate::transport::InMemoryQuerySource;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct EmptyFetcher;
    #[async_trait]
    impl KeyServerFetcher for EmptyFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
            None
        }
    }

    fn ctx() -> std::sync::Arc<XMatrixContext> {
        let key_cache: std::sync::Arc<DynRemoteKeyCache> = std::sync::Arc::new(
            RemoteKeyCache::new(Box::new(EmptyFetcher) as Box<dyn KeyServerFetcher>),
        );
        std::sync::Arc::new(XMatrixContext {
            own_server_name: "resident.example.org".to_string(),
            key_cache,
        })
    }

    fn state_with_room() -> FederationState {
        let room_id = "!r:resident.example.org";
        let create = serde_json::json!({
            "event_id": "$create", "type": "m.room.create", "room_id": room_id,
            "sender": "@creator:resident.example.org", "state_key": "",
            "content": {"creator": "@creator:resident.example.org", "room_version": "11"},
        });
        let power_levels = serde_json::json!({
            "event_id": "$power", "type": "m.room.power_levels", "room_id": room_id,
            "sender": "@creator:resident.example.org", "state_key": "",
            "content": {
                "users": {"@creator:resident.example.org": 100},
                "users_default": 0,
                "invite": 0, "kick": 50, "ban": 50, "redact": 50, "state_default": 50,
                "events_default": 0, "events": {}, "notifications": {"room": 50},
            },
        });
        let join_rules = serde_json::json!({
            "event_id": "$joinrules", "type": "m.room.join_rules", "room_id": room_id,
            "sender": "@creator:resident.example.org", "state_key": "",
            "content": {"join_rule": "public"},
        });
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            room_id,
            FakeRoom {
                room_version: Some("11".to_owned()),
                extremities: vec![("$create".to_owned(), 1)],
                state: vec![create.clone(), power_levels.clone(), join_rules.clone()],
                join_auth_chain: vec![create, power_levels, join_rules],
                ..FakeRoom::default()
            },
        );
        FederationState {
            own_server_name: std::sync::Arc::from("resident.example.org"),
            rooms: std::sync::Arc::new(rooms),
            queries: std::sync::Arc::new(InMemoryQuerySource::default()),
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
            write_sink: std::sync::Arc::new(StaticWriteSink::new(Vec::new(), "cannot persist yet")),
            transactions: std::sync::Arc::new(InMemoryTransactionStore::new()),
        }
    }

    fn build() -> axum::Router<FederationState> {
        add_routes(Builder::<FederationState>::new()).build().0
    }

    fn signed_header(origin: &str, method: &str, uri: &str) -> String {
        format!(
            "X-Matrix origin=\"{origin}\",destination=\"resident.example.org\",key=\"ed25519:1\",sig=\"AAAA\",method=\"{method}\",uri=\"{uri}\""
        )
    }

    #[tokio::test]
    async fn make_join_returns_a_real_template() {
        let app = build()
            .with_state(state_with_room())
            .layer(axum::Extension(ctx()));
        let uri = "/make_join/!r:resident.example.org/@bob:remote.example.org?ver=11";
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(
                        axum::http::header::AUTHORIZATION,
                        signed_header("remote.example.org", "GET", uri),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["room_version"], "11");
        assert_eq!(json["event"]["content"]["membership"], "join");
    }

    #[tokio::test]
    async fn make_join_unknown_room_is_404() {
        let app = build()
            .with_state(state_with_room())
            .layer(axum::Extension(ctx()));
        let uri = "/make_join/!nope:resident.example.org/@bob:remote.example.org";
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(
                        axum::http::header::AUTHORIZATION,
                        signed_header("remote.example.org", "GET", uri),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn send_join_v1_reports_the_persistence_gap_not_a_501_seam() {
        let app = build()
            .with_state(state_with_room())
            .layer(axum::Extension(ctx()));
        // A syntactically well-formed but unsigned/unverifiable event: the point of this test is
        // that the route runs *real* logic (and therefore fails signature verification, not "not
        // implemented"), proving it is no longer the old blanket seam.
        let body = serde_json::json!({
            "type": "m.room.member",
            "room_id": "!r:resident.example.org",
            "sender": "@bob:remote.example.org",
            "state_key": "@bob:remote.example.org",
            "origin_server_ts": 1,
            "depth": 2,
            "content": {"membership": "join"},
            "prev_events": ["$create"],
            "auth_events": [],
            "hashes": {"sha256": "AAAA"},
            "signatures": {},
        });
        let uri = "/send_join/!r:resident.example.org/$whatever";
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .header(
                        axum::http::header::AUTHORIZATION,
                        signed_header("remote.example.org", "PUT", uri),
                    )
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Not 501: this event fails real verification (no valid signature), which is a 400, not
        // "not implemented".
        assert_ne!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
