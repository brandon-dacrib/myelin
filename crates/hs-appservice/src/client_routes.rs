//! The client-server routes that are about appservices and are answered by this crate:
//!
//! - `GET /thirdparty/protocols`, `GET /thirdparty/protocol/{protocol}`: the third-party
//!   protocols the registered appservices declare, asked of them and merged
//!   ([`QueryService::protocols`]).
//! - `GET /thirdparty/user[/{protocol}]`, `GET /thirdparty/location[/{protocol}]`: third-party
//!   lookups, passed to the appservices that declare the protocol with the client's query
//!   ([`QueryService::thirdparty_lookup`]).
//! - `PUT`/`DELETE /directory/list/appservice/{networkId}/{roomId}`: an appservice publishing a
//!   room in its own room directory for one of its networks
//!   ([`crate::registry::Registry::set_network_room`]). `/publicRooms` (in `hs-room`) shows those
//!   rooms only for `third_party_instance_id` or `include_all_networks`, through
//!   `hs_auth::appservice::AppserviceRegistry::network_room_ids`.
//!
//! Paths are spec-relative: `hs serve` mounts [`client_router`] under `/_matrix/client/v3` and
//! `/_matrix/client/r0`. Like [`crate::routes::ping_router`] the router's state is `hs-auth`'s
//! [`AuthState`] (so [`Requester`] extracts), with the [`QueryService`] as an extension.
//!
//! Behaviour follows Synapse's (`synapse/rest/client/thirdparty.py`,
//! `synapse/rest/client/directory.py`'s `ClientAppserviceDirectoryListServer`, read for behaviour
//! only) and the spec (`application-service-api`'s third-party networks section and
//! `client-server-api`'s "Room directory").

#![allow(
    clippy::result_large_err,
    reason = "MatrixError is the workspace's standard Matrix-shaped error response type; boxing it would only churn every handler"
)]

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::{Json, Router};
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_http::body::PermissiveJson;
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;
use serde_json::{Value, json};

use crate::error::AppserviceError;
use crate::query::{QueryService, ThirdPartyKind};

/// The query a client sent, minus its `access_token` (which is ours, not the appservice's).
fn lookup_fields(query: Vec<(String, String)>) -> Vec<(String, String)> {
    query
        .into_iter()
        .filter(|(key, _)| key != "access_token")
        .collect()
}

async fn get_protocols<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    // Guests may ask too, as in Synapse (`allow_guest=True`); a token is all that is needed.
    _requester: Requester,
) -> Json<Value> {
    Json(json!(queries.protocols(None).await))
}

async fn get_protocol<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    _requester: Requester,
    Path(protocol): Path<String>,
) -> Result<Json<Value>, MatrixError> {
    let mut protocols = queries.protocols(Some(&protocol)).await;
    match protocols.remove(&protocol) {
        Some(info) => Ok(Json(info)),
        None => Err(MatrixError::custom(
            axum::http::StatusCode::NOT_FOUND,
            MatrixErrorCode::NotFound,
            format!("unknown protocol {protocol}"),
        )),
    }
}

async fn lookup<B: KvBackend>(
    queries: &QueryService<B>,
    kind: ThirdPartyKind,
    protocol: Option<&str>,
    query: Vec<(String, String)>,
) -> Json<Value> {
    let results = queries
        .thirdparty_lookup(kind, protocol, &lookup_fields(query))
        .await;
    Json(Value::Array(results))
}

async fn get_user_for_protocol<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    _requester: Requester,
    Path(protocol): Path<String>,
    Query(query): Query<Vec<(String, String)>>,
) -> Json<Value> {
    lookup(&queries, ThirdPartyKind::User, Some(&protocol), query).await
}

async fn get_user_reverse<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    _requester: Requester,
    Query(query): Query<Vec<(String, String)>>,
) -> Json<Value> {
    lookup(&queries, ThirdPartyKind::User, None, query).await
}

async fn get_location_for_protocol<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    _requester: Requester,
    Path(protocol): Path<String>,
    Query(query): Query<Vec<(String, String)>>,
) -> Json<Value> {
    lookup(&queries, ThirdPartyKind::Location, Some(&protocol), query).await
}

async fn get_location_reverse<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    _requester: Requester,
    Query(query): Query<Vec<(String, String)>>,
) -> Json<Value> {
    lookup(&queries, ThirdPartyKind::Location, None, query).await
}

/// Only an appservice edits an appservice room directory, and only its own.
fn appservice_of(requester: &Requester) -> Result<&str, MatrixError> {
    requester
        .appservice
        .as_ref()
        .map(|identity| identity.appservice_id.as_str())
        .ok_or_else(|| {
            MatrixError::forbidden(
                "only an application service can edit an application service's room directory",
            )
        })
}

fn publish<B: KvBackend>(
    queries: &QueryService<B>,
    appservice_id: &str,
    network_id: &str,
    room_id: &str,
    published: bool,
) -> Result<Json<Value>, MatrixError> {
    if ruma::RoomId::parse(room_id).is_err() {
        return Err(MatrixError::custom(
            axum::http::StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            format!("{room_id} is not a room ID"),
        ));
    }
    queries
        .registry()
        .set_network_room(appservice_id, network_id, room_id, published)
        .map_err(|error| match error {
            AppserviceError::NotFound(id) => {
                MatrixError::forbidden(format!("no such appservice: {id}"))
            }
            other => MatrixError::custom(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                MatrixErrorCode::Unknown,
                other.to_string(),
            ),
        })?;
    tracing::info!(
        appservice = appservice_id,
        network = network_id,
        room_id,
        published,
        "an appservice changed its room directory"
    );
    Ok(Json(json!({})))
}

async fn put_network_room<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    requester: Requester,
    Path((network_id, room_id)): Path<(String, String)>,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    let appservice_id = appservice_of(&requester)?;
    let published = match body.get("visibility").and_then(Value::as_str) {
        Some("public") => true,
        Some("private") => false,
        _ => {
            return Err(MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                "visibility must be \"public\" or \"private\"",
            ));
        }
    };
    publish(&queries, appservice_id, &network_id, &room_id, published)
}

async fn delete_network_room<B: KvBackend>(
    Extension(queries): Extension<Arc<QueryService<B>>>,
    requester: Requester,
    Path((network_id, room_id)): Path<(String, String)>,
) -> Result<Json<Value>, MatrixError> {
    let appservice_id = appservice_of(&requester)?;
    publish(&queries, appservice_id, &network_id, &room_id, false)
}

/// The routes in the module docs, and their manifest (`routes.json` rows). Spec-relative paths.
pub fn client_router<B: KvBackend>(
    queries: Arc<QueryService<B>>,
) -> (Router<AuthState>, RouteManifest) {
    let client = |id: &str| {
        RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(id.to_owned())
    };
    let appservice = |id: &str| {
        RouteMeta::new(Surface::MatrixClient, AuthKind::Appservice).with_operation_id(id.to_owned())
    };
    let (router, manifest) = Builder::<AuthState>::new()
        .get(
            "/thirdparty/protocols",
            get_protocols::<B>,
            client("getProtocols"),
        )
        .get(
            "/thirdparty/protocol/{protocol}",
            get_protocol::<B>,
            client("getProtocolMetadata"),
        )
        .get(
            "/thirdparty/user/{protocol}",
            get_user_for_protocol::<B>,
            client("queryUserByProtocol"),
        )
        .get(
            "/thirdparty/user",
            get_user_reverse::<B>,
            client("queryUserByID"),
        )
        .get(
            "/thirdparty/location/{protocol}",
            get_location_for_protocol::<B>,
            client("queryLocationByProtocol"),
        )
        .get(
            "/thirdparty/location",
            get_location_reverse::<B>,
            client("queryLocationByAlias"),
        )
        .put(
            "/directory/list/appservice/{networkId}/{roomId}",
            put_network_room::<B>,
            appservice("updateAppserviceRoomDirectoryVisibility"),
        )
        .delete(
            "/directory/list/appservice/{networkId}/{roomId}",
            delete_network_room::<B>,
            appservice("deleteAppserviceRoomDirectoryVisibility"),
        )
        .build();
    (router.layer(Extension(queries)), manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_registry::RegistryAppserviceAdapter;
    use crate::query::HttpQueryTransport;
    use crate::registration::Registration;
    use crate::registry::Registry;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hs_kv::memory::MemoryBackend;
    use ruma::server_name;
    use tower::ServiceExt;

    /// A bridge of protocol `ymca` with one network, `libera`.
    async fn bridge() -> String {
        let app = Router::new().route(
            "/_matrix/app/v1/thirdparty/protocol/ymca",
            axum::routing::get(|| async {
                Json(json!({
                    "user_fields": ["nick"],
                    "location_fields": ["channel"],
                    "icon": "mxc://example.org/ymca",
                    "field_types": {},
                    "instances": [{"desc": "Libera", "network_id": "libera", "fields": {}}],
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn app() -> (Router, Arc<Registry<MemoryBackend>>) {
        let registry =
            Arc::new(Registry::open(MemoryBackend::new(), server_name!("example.org")).unwrap());
        let mut registration = Registration::parse_yaml(
            "id: irc\nas_token: as_irc\nhs_token: hs_irc\nsender_localpart: ircbot\n\
             namespaces: {}\nprotocols: [ymca]\n",
        )
        .unwrap();
        registration.url = Some(bridge().await);
        registry.add(&registration).unwrap();
        let queries = Arc::new(QueryService::new(
            registry.clone(),
            Arc::new(HttpQueryTransport::new()),
        ));
        let auth = AuthState::in_memory().with_appservices(Arc::new(
            RegistryAppserviceAdapter::new(registry.clone()).with_queries(queries.clone()),
        ));
        let (router, manifest) = client_router::<MemoryBackend>(queries);
        assert_eq!(manifest.routes.len(), 8);
        (router.with_state(auth), registry)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer as_irc")
            .header("content-type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
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
    async fn protocols_come_from_the_bridges_and_an_unknown_one_is_not_found() {
        let (app, _) = app().await;
        let (status, body) = call(&app, "GET", "/thirdparty/protocols", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ymca"]["instances"][0]["instance_id"], "irc|libera");
        let (status, body) = call(&app, "GET", "/thirdparty/protocol/ymca", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["user_fields"], json!(["nick"]));
        let (status, body) = call(&app, "GET", "/thirdparty/protocol/gopher", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["errcode"], "M_NOT_FOUND");
        // A lookup nobody answers is an empty list, not an error.
        let (status, body) = call(&app, "GET", "/thirdparty/user/ymca?nick=alice", None).await;
        assert_eq!((status, body), (StatusCode::OK, json!([])));
    }

    #[tokio::test]
    async fn an_appservice_publishes_and_withdraws_rooms_in_its_network_directory() {
        let (app, registry) = app().await;
        let path = "/directory/list/appservice/libera/!room:example.org";
        let (status, body) = call(&app, "PUT", path, Some(json!({"visibility": "public"}))).await;
        assert_eq!((status, body), (StatusCode::OK, json!({})));
        assert_eq!(
            registry.network_room_ids(Some("irc|libera")).unwrap(),
            vec!["!room:example.org"]
        );
        let (status, _) = call(&app, "PUT", path, Some(json!({"visibility": "sideways"}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &app,
            "PUT",
            "/directory/list/appservice/libera/not-a-room",
            Some(json!({"visibility": "public"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, "DELETE", path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(registry.network_room_ids(None).unwrap().is_empty());
    }

    #[test]
    fn only_an_appservice_edits_an_appservice_directory() {
        let person = Requester::for_user(ruma::user_id!("@alice:example.org").to_owned());
        let err = appservice_of(&person).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }
}
