//! `make_leave`/`send_leave`, `make_knock`/`send_knock` and `invite`: the membership handshakes
//! besides the join, wired against [`crate::join`] (the first two, which are the join's
//! handshake with another `membership`) and [`crate::invite`].
//!
//! [`add_routes`] registers the v1 spellings; [`add_routes_v2`] the v2 `send_leave` and
//! `invite`, for the router mounted at `/_matrix/federation/v2` (see
//! `crate::transport::router_v2`). The v1 and v2 `send_leave` differ only in the response: v1
//! answers `[200, {}]`, v2 `{}`. The v1 and v2 `invite` differ in the request as well: v1's body
//! is the event itself, with the stripped state in its `unsigned.invite_room_state` and no room
//! version (so room version 1 is assumed, which this server does not support); v2's body is
//! `{room_version, event, invite_room_state}`.

use axum::extract::{Extension, Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::Builder;
use serde_json::{Value, json};

use super::join::{join_error_response, requesting_server};
use crate::invite::{self, InviteError};
use crate::join::{self, Handshake};
use crate::transport::FederationState;
use crate::xmatrix::XMatrixContext;

pub(super) fn add_routes(builder: Builder<FederationState>) -> Builder<FederationState> {
    builder
        .add(
            Method::GET,
            "/make_leave/{roomId}/{userId}",
            make_leave,
            super::matrix_federation("federationMakeLeave"),
        )
        .add(
            Method::PUT,
            "/send_leave/{roomId}/{eventId}",
            send_leave_v1,
            super::matrix_federation("federationSendLeaveV1"),
        )
        .add(
            Method::GET,
            "/make_knock/{roomId}/{userId}",
            make_knock,
            super::matrix_federation("federationMakeKnock"),
        )
        .add(
            Method::PUT,
            "/send_knock/{roomId}/{eventId}",
            send_knock,
            super::matrix_federation("federationSendKnock"),
        )
        .add(
            Method::PUT,
            "/invite/{roomId}/{eventId}",
            invite_v1,
            super::matrix_federation("federationInviteV1"),
        )
}

pub(super) fn add_routes_v2(builder: Builder<FederationState>) -> Builder<FederationState> {
    builder
        .add(
            Method::PUT,
            "/send_leave/{roomId}/{eventId}",
            send_leave_v2,
            super::matrix_federation("federationSendLeaveV2"),
        )
        .add(
            Method::PUT,
            "/invite/{roomId}/{eventId}",
            invite_v2,
            super::matrix_federation("federationInviteV2"),
        )
}

async fn make_template(
    state: &FederationState,
    headers: &axum::http::HeaderMap,
    room_id: &str,
    user_id: &str,
    versions: &[String],
    handshake: Handshake,
) -> Response {
    let origin = match requesting_server(headers) {
        Ok(origin) => origin,
        Err(err) => return (*err).into_response(),
    };
    // A server asks for its own users' templates only: a template for somebody else's user is
    // an event that server could never sign.
    if user_id.split_once(':').map(|(_, server)| server) != Some(origin.as_str()) {
        return MatrixError::forbidden("the user is not on the requesting server").into_response();
    }
    match join::make_membership(state.rooms.as_ref(), room_id, user_id, versions, handshake).await {
        Ok(template) => axum::Json(json!({
            "event": template.event,
            "room_version": template.room_version,
        }))
        .into_response(),
        Err(e) => join_error_response(&e),
    }
}

async fn make_leave(
    State(state): State<FederationState>,
    Path((room_id, user_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    make_template(&state, &headers, &room_id, &user_id, &[], Handshake::Leave).await
}

async fn make_knock(
    State(state): State<FederationState>,
    Path((room_id, user_id)): Path<(String, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let versions: Vec<String> = pairs
        .into_iter()
        .filter(|(k, _)| k == "ver")
        .map(|(_, v)| v)
        .collect();
    // `ver` is required for a knock (unlike a join, where its absence means "version 1"): a
    // server that names none supports no version that has knocking.
    if versions.is_empty() {
        return MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::MissingParam,
            "make_knock requires at least one `ver`",
        )
        .into_response();
    }
    make_template(
        &state,
        &headers,
        &room_id,
        &user_id,
        &versions,
        Handshake::Knock,
    )
    .await
}

/// `send_leave`/`send_knock`'s shared body: parse, then [`join::send_membership`].
async fn submit(
    state: &FederationState,
    ctx: &XMatrixContext,
    headers: &axum::http::HeaderMap,
    room_id: &str,
    event_id: &str,
    body: &[u8],
    handshake: Handshake,
) -> Result<join::SendJoinResult, Box<Response>> {
    let origin = requesting_server(headers).map_err(|err| Box::new((*err).into_response()))?;
    let signed_event: Value = serde_json::from_slice(body).map_err(|_| {
        Box::new(
            MatrixError::bad_json(format!("invalid {} event body", handshake.membership()))
                .into_response(),
        )
    })?;
    join::send_membership(
        state.rooms.as_ref(),
        state.write_sink.as_ref(),
        &ctx.key_cache,
        room_id,
        event_id,
        &signed_event,
        &origin,
        &state.own_server_name,
        state.sender.as_deref(),
        handshake,
    )
    .await
    .map_err(|e| Box::new(join_error_response(&e)))
}

async fn send_leave(
    state: State<FederationState>,
    ctx: Extension<std::sync::Arc<XMatrixContext>>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
    v2: bool,
) -> Response {
    match submit(
        &state,
        &ctx,
        &headers,
        &room_id,
        &event_id,
        &body,
        Handshake::Leave,
    )
    .await
    {
        Ok(_) if v2 => axum::Json(json!({})).into_response(),
        Ok(_) => axum::Json(json!([200, {}])).into_response(),
        Err(response) => *response,
    }
}

async fn send_leave_v1(
    state: State<FederationState>,
    ctx: Extension<std::sync::Arc<XMatrixContext>>,
    path: Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    send_leave(state, ctx, path, headers, body, false).await
}

async fn send_leave_v2(
    state: State<FederationState>,
    ctx: Extension<std::sync::Arc<XMatrixContext>>,
    path: Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    send_leave(state, ctx, path, headers, body, true).await
}

async fn send_knock(
    State(state): State<FederationState>,
    Extension(ctx): Extension<std::sync::Arc<XMatrixContext>>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    match submit(
        &state,
        &ctx,
        &headers,
        &room_id,
        &event_id,
        &body,
        Handshake::Knock,
    )
    .await
    {
        Ok(result) => {
            let knock_room_state = crate::stripped::stripped_state(&result.state, &[]);
            axum::Json(json!({ "knock_room_state": knock_room_state })).into_response()
        }
        Err(response) => *response,
    }
}

#[allow(clippy::too_many_arguments)]
async fn invite(
    state: &FederationState,
    ctx: &XMatrixContext,
    headers: &axum::http::HeaderMap,
    room_id: &str,
    event_id: &str,
    room_version: &str,
    event: &Value,
    invite_room_state: &[Value],
) -> Result<Value, Box<Response>> {
    let origin = requesting_server(headers).map_err(|err| Box::new((*err).into_response()))?;
    let Some(handling) = &state.invites else {
        return Err(Box::new(
            MatrixError::custom(
                StatusCode::NOT_IMPLEMENTED,
                MatrixErrorCode::Other("M_NOT_IMPLEMENTED".to_owned()),
                "this server does not accept invites over federation",
            )
            .into_response(),
        ));
    };
    invite::receive_invite(
        handling,
        &ctx.key_cache,
        &state.own_server_name,
        &origin,
        room_id,
        event_id,
        room_version,
        event,
        invite_room_state,
    )
    .await
    .map_err(|e| Box::new(invite_error_response(&e)))
}

fn invite_error_response(e: &InviteError) -> Response {
    match e {
        InviteError::IncompatibleRoomVersion(_) => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::IncompatibleRoomVersion,
            e.to_string(),
        )
        .into_response(),
        InviteError::Malformed(msg) => MatrixError::bad_json(msg.clone()).into_response(),
        InviteError::Forbidden(msg) => MatrixError::forbidden(msg.clone()).into_response(),
        InviteError::Store(msg) => MatrixError::custom(
            StatusCode::INTERNAL_SERVER_ERROR,
            MatrixErrorCode::Unknown,
            msg.clone(),
        )
        .into_response(),
    }
}

async fn invite_v1(
    State(state): State<FederationState>,
    Extension(ctx): Extension<std::sync::Arc<XMatrixContext>>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return MatrixError::bad_json("invalid invite event body").into_response();
    };
    let invite_room_state = event
        .pointer("/unsigned/invite_room_state")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // v1 carries no room version: the spec says to assume "1" -- which this server does not
    // support, so this is the incompatible-version answer for every modern room, as it is from
    // Synapse. It is still run through the same path so the answer is the same shape.
    match invite(
        &state,
        &ctx,
        &headers,
        &room_id,
        &event_id,
        "1",
        &event,
        &invite_room_state,
    )
    .await
    {
        Ok(event) => axum::Json(json!([200, { "event": event }])).into_response(),
        Err(response) => *response,
    }
}

async fn invite_v2(
    State(state): State<FederationState>,
    Extension(ctx): Extension<std::sync::Arc<XMatrixContext>>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return MatrixError::bad_json("invalid invite body").into_response();
    };
    let Some(room_version) = body.get("room_version").and_then(Value::as_str) else {
        return MatrixError::bad_json("the invite names no room_version").into_response();
    };
    let Some(event) = body.get("event") else {
        return MatrixError::bad_json("the invite carries no event").into_response();
    };
    let invite_room_state = body
        .get("invite_room_state")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match invite(
        &state,
        &ctx,
        &headers,
        &room_id,
        &event_id,
        room_version,
        event,
        &invite_room_state,
    )
    .await
    {
        Ok(event) => axum::Json(json!({ "event": event })).into_response(),
        Err(response) => *response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::{InMemoryTransactionStore, StaticWriteSink};
    use crate::keys::{DynRemoteKeyCache, KeyServerFetcher, RemoteKeyCache};
    use crate::room_source::InMemoryRoomSource;
    use crate::transport::InMemoryQuerySource;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct EmptyFetcher;
    #[async_trait]
    impl KeyServerFetcher for EmptyFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<Value> {
            None
        }
    }

    fn app(v2: bool) -> axum::Router {
        let state = FederationState {
            own_server_name: Arc::from("resident.example.org"),
            rooms: Arc::new(InMemoryRoomSource::new()),
            queries: Arc::new(InMemoryQuerySource::default()),
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
            write_sink: Arc::new(StaticWriteSink::new(Vec::new(), "unused")),
            transactions: Arc::new(InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
        };
        let key_cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(
            Box::new(EmptyFetcher) as Box<dyn KeyServerFetcher>,
        ));
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "resident.example.org".to_owned(),
            key_cache,
        });
        let builder = Builder::<FederationState>::new();
        let builder = if v2 {
            add_routes_v2(builder)
        } else {
            add_routes(builder)
        };
        builder
            .build()
            .0
            .with_state(state)
            .layer(axum::Extension(ctx))
    }

    /// The header the X-Matrix layer would have verified; the handlers only read `origin`.
    fn header(origin: &str, method: &str, uri: &str) -> String {
        format!(
            "X-Matrix origin=\"{origin}\",destination=\"resident.example.org\",key=\"ed25519:1\",sig=\"AAAA\",method=\"{method}\",uri=\"{uri}\""
        )
    }

    async fn call(app: axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(
                        axum::http::header::AUTHORIZATION,
                        header("remote.example.org", method, uri),
                    )
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn make_knock_without_a_version_is_a_missing_parameter() {
        let (status, body) = call(
            app(false),
            "GET",
            "/make_knock/!r:resident.example.org/@bob:remote.example.org",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["errcode"], "M_MISSING_PARAM");
    }

    #[tokio::test]
    async fn a_template_for_another_servers_user_is_refused() {
        let (status, body) = call(
            app(false),
            "GET",
            "/make_leave/!r:resident.example.org/@bob:elsewhere.example.org",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    #[tokio::test]
    async fn an_unknown_room_is_not_found_for_a_leave() {
        let (status, _) = call(
            app(false),
            "GET",
            "/make_leave/!nope:resident.example.org/@bob:remote.example.org",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_invite_with_nowhere_to_go_is_not_implemented_and_a_bodyless_one_is_bad_json() {
        let uri = "/invite/!r:remote.example.org/$event";
        let (status, _) = call(
            app(true),
            "PUT",
            uri,
            json!({"room_version": "11", "event": {}, "invite_room_state": []}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        let (status, body) = call(app(true), "PUT", uri, json!({"event": {}})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["errcode"], "M_BAD_JSON");
    }
}
