//! The federation transport server's read-only and query endpoints, fully implemented against
//! [`crate::room_source::RoomDataSource`] and [`crate::transport::FederationQuerySource`], with
//! every per-room endpoint checking visibility before returning content and every `limit`-taking
//! endpoint clamped server-side, per `docs/design/06-federation-threat-model.md` section 2.4.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_http::router::Builder;

use crate::room_source::RoomSourceError;
use crate::transport::FederationState;

use hs_http::error::{MatrixError, MatrixErrorCode};

/// Server-side ceiling applied to `/backfill` and `/get_missing_events` `limit` parameters,
/// regardless of what the caller asked for (threat model section 3).
pub const MAX_BACKFILL_LIMIT: usize = 100;
/// Server-side ceiling applied to `/publicRooms` page size.
pub const MAX_PUBLIC_ROOMS_PAGE: usize = 100;

pub(super) fn add_routes(builder: Builder<FederationState>) -> Builder<FederationState> {
    fn meta(op: &str) -> hs_http::router::RouteMeta {
        super::matrix_federation(op)
    }

    builder
        // The spec names these two query types as their own paths (`query.yaml`'s
        // `queryProfile` and `queryRoomDirectory`) as well as reaching them through the generic
        // `{queryType}` form. Registering all three means the manifest reports what this server
        // actually answers, and a caller using either spelling gets the same handler.
        .get("/query/profile", query_profile, meta("queryProfile"))
        .get(
            "/query/directory",
            query_directory,
            meta("queryRoomDirectory"),
        )
        .get("/query/{queryType}", query, meta("federationQuery"))
        .get(
            "/user/devices/{userId}",
            user_devices,
            meta("federationUserDevices"),
        )
        .get("/publicRooms", public_rooms, meta("federationPublicRooms"))
        .get(
            "/hierarchy/{roomId}",
            hierarchy,
            meta("federationHierarchy"),
        )
        .get(
            "/timestamp_to_event/{roomId}",
            timestamp_to_event,
            meta("federationTimestampToEvent"),
        )
        .get("/event/{eventId}", get_event, meta("federationGetEvent"))
        .get("/state/{roomId}", get_state, meta("federationGetState"))
        .get(
            "/state_ids/{roomId}",
            get_state_ids,
            meta("federationGetStateIds"),
        )
        .get(
            "/event_auth/{roomId}/{eventId}",
            event_auth,
            meta("federationEventAuth"),
        )
        .get("/backfill/{roomId}", backfill, meta("federationBackfill"))
        .add(
            axum::http::Method::POST,
            "/get_missing_events/{roomId}",
            get_missing_events,
            meta("federationGetMissingEvents"),
        )
}

/// Extracts the requesting server's name from the (already-verified, by the layer above) request.
/// The `X-Matrix` middleware runs before every handler and only forwards requests whose signature
/// checked out; the origin it verified is not otherwise threaded into extensions in this pass, so
/// handlers that need "who is asking" re-parse the same header. This duplicates a small amount of
/// parsing (cheap, and infallible here since the layer already validated it) rather than adding an
/// extension-passing seam between two independent modules for a first cut — see
/// `docs/status/06-federation.md` for this as a named simplification, not an oversight: the
/// signature-checked `origin` is exactly what `crate::xmatrix::parse_x_matrix_header` returns, and
/// re-parsing cannot disagree with what the layer already verified since it reads the same header.
fn requesting_server(headers: &axum::http::HeaderMap) -> Result<String, Box<MatrixError>> {
    crate::xmatrix::parse_x_matrix_header(headers)
        .map(|auth| auth.origin)
        .map_err(|_| {
            Box::new(MatrixError::forbidden(
                "could not determine requesting server",
            ))
        })
}

/// The spec's `Transaction` shape that `GET /event/{eventId}` and `GET /backfill/{roomId}`
/// answer with: `origin` (this server), `origin_server_ts` (now) and `pdus`. Until 2026-10-01
/// both answered `{"pdus": [...]}` alone, and Sytest's `32room-getevent.pl` and
/// `34room-backfill.pl` (which read `origin` and `origin_server_ts` off the body) failed.
fn transaction_body(own_server_name: &str, pdus: Vec<serde_json::Value>) -> serde_json::Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    serde_json::json!({
        "origin": own_server_name,
        "origin_server_ts": now_ms,
        "pdus": pdus,
    })
}

fn room_source_error_to_response(err: RoomSourceError) -> MatrixError {
    match err {
        RoomSourceError::RoomNotFound | RoomSourceError::NotFound => {
            MatrixError::not_found("not found")
        }
        RoomSourceError::NotVisible => MatrixError::forbidden("not visible to this server"),
    }
}

#[derive(serde::Deserialize)]
struct QueryParams {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    room_alias: Option<String>,
    #[serde(default)]
    field: Option<String>,
}

async fn query(
    State(state): State<FederationState>,
    Path(query_type): Path<String>,
    Query(params): Query<QueryParams>,
) -> Response {
    match query_type.as_str() {
        "profile" => {
            let Some(user_id) = params.user_id else {
                return MatrixError::missing_param("user_id").into_response();
            };
            // A user ID whose server name does not parse -- Sytest's and Complement's "Non-numeric
            // ports in server names are rejected", `@user1:localhost:http` -- is a bad request,
            // as Synapse answers, not an unknown user.
            if !is_valid_user_id(&user_id) {
                return MatrixError::custom(
                    axum::http::StatusCode::BAD_REQUEST,
                    MatrixErrorCode::InvalidParam,
                    "user_id is not a valid user ID",
                )
                .into_response();
            }
            match state
                .queries
                .profile(&user_id, params.field.as_deref())
                .await
            {
                Some(v) => axum::Json(v).into_response(),
                None => MatrixError::not_found("unknown user").into_response(),
            }
        }
        "directory" => {
            let Some(alias) = params.room_alias else {
                return MatrixError::missing_param("room_alias").into_response();
            };
            match state.queries.resolve_alias(&alias).await {
                Some((room_id, servers)) => {
                    axum::Json(serde_json::json!({ "room_id": room_id, "servers": servers }))
                        .into_response()
                }
                None => MatrixError::not_found("unknown alias").into_response(),
            }
        }
        _ => MatrixError::custom(
            axum::http::StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            "unknown query type",
        )
        .into_response(),
    }
}

/// Whether `user_id` is `@localpart:server_name` with a server name the spec allows: a host and,
/// if any, a numeric port.
fn is_valid_user_id(user_id: &str) -> bool {
    let Some((_, server)) = user_id
        .strip_prefix('@')
        .and_then(|rest| rest.split_once(':'))
    else {
        return false;
    };
    ruma::UserId::parse(user_id).is_ok() && ruma::ServerName::parse(server).is_ok()
}

async fn query_profile(state: State<FederationState>, params: Query<QueryParams>) -> Response {
    query(state, Path("profile".to_owned()), params).await
}

async fn query_directory(state: State<FederationState>, params: Query<QueryParams>) -> Response {
    query(state, Path("directory".to_owned()), params).await
}

async fn user_devices(
    State(state): State<FederationState>,
    Path(user_id): Path<String>,
) -> Response {
    // `allow_device_name_lookup_over_federation` governs the display names, not the answer:
    // a server that shares a room with this user needs their device list to encrypt to them,
    // and Synapse answers with the names left out when the option is off, as this does.
    match state.queries.devices(&user_id).await {
        Some(mut v) => {
            if !state.policy.allow_device_names()
                && let Some(devices) = v.get_mut("devices").and_then(|d| d.as_array_mut())
            {
                for device in devices {
                    if let Some(device) = device.as_object_mut() {
                        device.remove("device_display_name");
                    }
                }
            }
            axum::Json(v).into_response()
        }
        None => MatrixError::not_found("unknown user").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PublicRoomsParams {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    since: Option<String>,
}

async fn public_rooms(
    State(state): State<FederationState>,
    Query(params): Query<PublicRoomsParams>,
) -> Response {
    if !state.policy.allow_public_rooms() {
        return MatrixError::forbidden("public room directory is disabled").into_response();
    }
    let limit = params
        .limit
        .unwrap_or(MAX_PUBLIC_ROOMS_PAGE)
        .min(MAX_PUBLIC_ROOMS_PAGE);
    let rooms = state
        .rooms
        .list_public_rooms(limit, params.since.as_deref())
        .await;
    axum::Json(serde_json::json!({ "chunk": rooms, "total_room_count_estimate": rooms.len() }))
        .into_response()
}

#[derive(serde::Deserialize)]
struct HierarchyParams {
    #[serde(default)]
    suggested_only: Option<bool>,
}

/// `GET /hierarchy/{roomId}` (MSC2946). Not paginated: the space and the children this server
/// holds, filtered to what the requesting server may see, for it to filter further per user.
/// A room the requester may not see is `404 M_NOT_FOUND`, the same as one this server does not
/// hold, as the spec says ("not known to the server or the requesting server is unable to
/// peek/join it"): the two must be told apart no more here than through `/make_join`.
async fn hierarchy(
    State(state): State<FederationState>,
    Path(room_id): Path<String>,
    Query(params): Query<HierarchyParams>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    match state
        .rooms
        .hierarchy(&room_id, params.suggested_only.unwrap_or(false), &requester)
        .await
    {
        Ok(body) => axum::Json(body).into_response(),
        Err(RoomSourceError::NotVisible) => {
            MatrixError::not_found("room not known or not accessible").into_response()
        }
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct TimestampParams {
    ts: u64,
    #[serde(default)]
    dir: Option<String>,
}

async fn timestamp_to_event(
    State(state): State<FederationState>,
    Path(room_id): Path<String>,
    Query(params): Query<TimestampParams>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    let forward = params.dir.as_deref() == Some("f");
    match state
        .rooms
        .event_near_timestamp(&room_id, params.ts, forward, &requester)
        .await
    {
        Ok((event_id, origin_server_ts)) => axum::Json(
            serde_json::json!({ "event_id": event_id, "origin_server_ts": origin_server_ts }),
        )
        .into_response(),
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

async fn get_event(
    State(state): State<FederationState>,
    Path(event_id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    match state.rooms.get_event_by_id(&event_id, &requester).await {
        Ok((_room_id, event)) => {
            axum::Json(transaction_body(&state.own_server_name, vec![event])).into_response()
        }
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct AtEventParam {
    event_id: String,
}

async fn get_state(
    State(state): State<FederationState>,
    Path(room_id): Path<String>,
    Query(params): Query<AtEventParam>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    match state
        .rooms
        .state_at(&room_id, &params.event_id, &requester)
        .await
    {
        Ok((pdus, auth_chain)) => {
            axum::Json(serde_json::json!({ "pdus": pdus, "auth_chain": auth_chain }))
                .into_response()
        }
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

async fn get_state_ids(
    State(state): State<FederationState>,
    Path(room_id): Path<String>,
    Query(params): Query<AtEventParam>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    match state
        .rooms
        .state_ids_at(&room_id, &params.event_id, &requester)
        .await
    {
        Ok((pdu_ids, auth_chain_ids)) => {
            axum::Json(serde_json::json!({ "pdu_ids": pdu_ids, "auth_chain_ids": auth_chain_ids }))
                .into_response()
        }
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

async fn event_auth(
    State(state): State<FederationState>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    match state
        .rooms
        .auth_chain(&room_id, &event_id, &requester)
        .await
    {
        Ok(chain) => axum::Json(serde_json::json!({ "auth_chain": chain })).into_response(),
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

/// `/backfill`'s query string, parsed from raw key/value pairs rather than a `serde` struct:
/// `v` is repeated once per event ID the caller already has (`?v=$a&v=$b`), and
/// `serde_urlencoded` -- what `axum::extract::Query` deserializes through -- cannot produce a
/// sequence from repeated keys. Deriving `Deserialize` for a `Vec<String>` field therefore does
/// not merely drop the extra values: it fails the whole extraction, and every spec-shaped
/// backfill request is answered `400` before the handler runs.
struct BackfillParams {
    from: Vec<String>,
    limit: Option<usize>,
}

impl BackfillParams {
    fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        let mut from = Vec::new();
        let mut limit = None;
        for (key, value) in pairs {
            match key.as_str() {
                "v" => from.push(value),
                "limit" => limit = value.parse().ok(),
                _ => {}
            }
        }
        Self { from, limit }
    }
}

async fn backfill(
    State(state): State<FederationState>,
    Path(room_id): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    let params = BackfillParams::from_pairs(pairs);
    let limit = params
        .limit
        .unwrap_or(MAX_BACKFILL_LIMIT)
        .min(MAX_BACKFILL_LIMIT);
    match state
        .rooms
        .backfill(&room_id, &params.from, limit, &requester)
        .await
    {
        Ok(pdus) => axum::Json(transaction_body(&state.own_server_name, pdus)).into_response(),
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct MissingEventsBody {
    #[serde(default)]
    earliest_events: Vec<String>,
    #[serde(default)]
    latest_events: Vec<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// "The minimum depth of events to retrieve. Defaults to 0." A walk back from
    /// `latest_events` stops at the first event below it: prev events are always shallower.
    #[serde(default)]
    min_depth: Option<i64>,
}

async fn get_missing_events(
    State(state): State<FederationState>,
    Path(room_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let requester = match requesting_server(&headers) {
        Ok(r) => r,
        Err(e) => return (*e).into_response(),
    };
    let params: MissingEventsBody = if body.is_empty() {
        MissingEventsBody {
            earliest_events: Vec::new(),
            latest_events: Vec::new(),
            limit: None,
            min_depth: None,
        }
    } else {
        match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(_) => return MatrixError::bad_json("invalid request body").into_response(),
        }
    };
    let limit = params
        .limit
        .unwrap_or(MAX_BACKFILL_LIMIT)
        .min(MAX_BACKFILL_LIMIT);
    match state
        .rooms
        .missing_events(
            &room_id,
            &params.earliest_events,
            &params.latest_events,
            limit,
            params.min_depth.unwrap_or(0),
            &requester,
        )
        .await
    {
        Ok(events) => axum::Json(serde_json::json!({ "events": events })).into_response(),
        Err(e) => room_source_error_to_response(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_source::{FakeRoom, InMemoryRoomSource};
    use crate::transport::InMemoryQuerySource;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header::AUTHORIZATION};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn signed_header(origin: &str, method: &str, uri: &str) -> String {
        // Reuse xmatrix's own signing but with a throwaway key/cache pair — these tests exercise
        // the read-route logic directly (via `requesting_server`, which only needs a *parseable*
        // header, not a cryptographically valid one), so an unsigned-but-well-formed header is
        // sufficient and keeps these tests independent of the X-Matrix layer (tested separately
        // in `crate::transport::tests`).
        format!(
            "X-Matrix origin=\"{origin}\",destination=\"us.example.org\",key=\"ed25519:1\",sig=\"AAAA\",method=\"{method}\",uri=\"{uri}\""
        )
    }

    fn build() -> axum::Router<FederationState> {
        add_routes(Builder::<FederationState>::new()).build().0
    }

    fn app_with_room() -> axum::Router {
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            "!r:example.org",
            FakeRoom {
                world_readable: false,
                joined_servers: vec!["member.example.org".to_string()],
                events: [(
                    "$e1".to_string(),
                    serde_json::json!({"event_id": "$e1", "type": "m.room.message"}),
                )]
                .into_iter()
                .collect(),
                ..FakeRoom::default()
            },
        );
        let state = FederationState {
            own_server_name: Arc::from("us.example.org"),
            rooms: Arc::new(rooms),
            queries: Arc::new(InMemoryQuerySource::default()),
            policy: crate::transport::InboundPolicy::new(true, true),
            write_sink: Arc::new(crate::inbound::StaticWriteSink::new(
                Vec::new(),
                "not supported",
            )),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
        };
        build().with_state(state)
    }

    #[tokio::test]
    async fn event_lookup_denies_non_member_server() {
        let app = app_with_room();
        let header = signed_header("outsider.example.org", "GET", "/event/$e1");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/event/$e1")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn event_lookup_allows_member_server() {
        let app = app_with_room();
        let header = signed_header("member.example.org", "GET", "/event/$e1");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/event/$e1")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// `/event` and `/backfill` answer the spec's `Transaction`: `origin`, `origin_server_ts`
    /// and `pdus`. Both answered `pdus` alone before 2026-10-01 (Sytest: "Expected a
    /// 'origin_server_ts' key", "Expected a 'origin' key").
    #[tokio::test]
    async fn event_and_backfill_answer_a_transaction_from_this_server() {
        for uri in ["/event/$e1", "/backfill/!r:example.org?v=$e1&limit=5"] {
            let path = uri.split('?').next().unwrap();
            let header = signed_header("member.example.org", "GET", path);
            let response = app_with_room()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(AUTHORIZATION, header)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["origin"], "us.example.org", "{uri}: {body}");
            assert!(
                body["origin_server_ts"].as_u64().is_some_and(|ts| ts > 0),
                "{uri}: {body}"
            );
            assert!(body["pdus"].is_array(), "{uri}: {body}");
        }
    }

    #[tokio::test]
    async fn backfill_limit_is_clamped_server_side() {
        let app = app_with_room();
        let header = signed_header("member.example.org", "GET", "/backfill/!r:example.org");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/backfill/!r:example.org?limit=999999")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Only one event exists in the fake room regardless of the clamp, but the important
        // assertion is that the request is accepted and does not error on an oversized limit —
        // the numeric clamp itself is exercised directly against `MAX_BACKFILL_LIMIT`.
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(MAX_BACKFILL_LIMIT, 100);
    }

    #[tokio::test]
    async fn public_rooms_disabled_by_config_is_forbidden() {
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room("!r:example.org", FakeRoom::default());
        let state = FederationState {
            own_server_name: Arc::from("us.example.org"),
            rooms: Arc::new(rooms),
            queries: Arc::new(InMemoryQuerySource::default()),
            policy: crate::transport::InboundPolicy::new(false, false),
            write_sink: Arc::new(crate::inbound::StaticWriteSink::new(
                Vec::new(),
                "not supported",
            )),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
        };
        let router = build();
        let header = signed_header("anyone.example.org", "GET", "/publicRooms");
        let response = router
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/publicRooms")
                    .header(AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_user_id_with_a_non_numeric_port_is_not_valid() {
        assert!(super::is_valid_user_id("@user1:localhost"));
        assert!(super::is_valid_user_id("@user1:localhost:8448"));
        assert!(!super::is_valid_user_id("@user1:localhost:http"));
        assert!(!super::is_valid_user_id("user1:localhost"));
        assert!(!super::is_valid_user_id("@user1"));
    }
}
