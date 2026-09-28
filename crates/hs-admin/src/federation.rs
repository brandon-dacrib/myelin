//! The rest of the Federation area (RFC 0004 section 4, "Federation"): the rooms this server
//! shares with a destination (`federation.destinations.rooms`) and the signing keys
//! (`federation.keys.list`, `.get`, `.refresh`). The three `federation.destinations.*`
//! operations that read the destination records themselves live in `crate::router`.
//!
//! # Shared rooms
//!
//! Composed here from the room directory ([`crate::sources::RoomDirectory`]): every room this
//! server knows in which at least one member of the destination is joined, with the room's own
//! joined count and how many of those are the destination's. It reads each room's members, so
//! it costs one member list per room; an administrator's page, not a hot path.
//!
//! # Keys
//!
//! `federation.keys.list` is this server's own signing keys, the ones `/_matrix/key/v2/server`
//! publishes. `federation.keys.get` is what the key cache holds for another server: the keys
//! its signatures are checked against, current and old, and when they were fetched. Both read
//! the [`FederationSource`]. `federation.keys.refresh` is a Task
//! (`federation.refetch_keys`, resource `{type: server, id}`): it fetches the server's keys
//! again, whatever is cached, and ends `succeeded` with what the cache then holds as its
//! `result`, or `failed` when the server could not be reached or answered something that does
//! not verify. The request is audited (`federation.keys.refresh`) and published
//! (`federation.keys_refresh_started`); a success is published as `federation.keys_refreshed`.
//! A state with no task registry runs the fetch inside the request and answers the task
//! finished.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::model::{Actor, Event, Page, ResourceRef, Scope, Task, TaskStatus};
use crate::router::AdminState;
use crate::sources::{FederationSource, RoomFilter, SourceError};

/// The OpenAPI `ServerSigningKey` schema: one signing key, this server's or another's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSigningKey {
    /// `ed25519:<version>`.
    pub key_id: String,
    /// `ed25519`.
    pub algorithm: String,
    /// Base64 (standard alphabet, unpadded), as published.
    pub public_key: String,
    /// Until when it may be used: a cached current key's `valid_until_ts`, an old key's
    /// `expired_ts`. `None` for this server's own keys, which are valid until rotated.
    pub valid_until_at: Option<String>,
    /// Whether it is an old key (`old_verify_keys`), usable only for what was signed before
    /// `valid_until_at`.
    pub old: bool,
}

/// The OpenAPI `RemoteServerKeys` schema: what the key cache holds for one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRemoteServerKeys {
    pub server_name: String,
    /// Current keys first, then old ones, each by key id.
    pub keys: Vec<AdminSigningKey>,
    /// When a key response from (or about) the server was last accepted; `None` when the
    /// cache has keys but no record of when (never, in practice).
    pub cached_at: Option<String>,
}

/// The OpenAPI `DestinationRoom` schema: one room this server shares with a destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDestinationRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub canonical_alias: Option<String>,
    /// Everyone joined to the room, from every server.
    pub joined_members_count: u64,
    /// How many of them are the destination's users.
    pub destination_members_count: u64,
}

/// The task action `federation.keys.refresh` starts.
pub const REFRESH_TASK_ACTION: &str = "federation.refetch_keys";

/// The server part of a user id (`@alice:example.org:8448` → `example.org:8448`).
fn server_of(user_id: &str) -> Option<&str> {
    user_id.split_once(':').map(|(_, server)| server)
}

// -------------------------------------------------------------------------------------------
// Handlers
// -------------------------------------------------------------------------------------------

/// `GET /federation/destinations/{server_name}/rooms`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct DestinationRoomsQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
}

/// `GET /api/v1/federation/destinations/{server_name}/rooms` (`admin:read`): the rooms this
/// server shares with the destination, most of its members first. `404` when this server has
/// never tried to reach it and shares no room with it.
pub(crate) async fn destination_rooms(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
    Query(query): Query<DestinationRoomsQuery>,
) -> Response {
    let instance = format!("/api/v1/federation/destinations/{server_name}/rooms");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let Some(rooms) = &state.rooms else {
        return unwired("room directory", &instance);
    };
    let all = match rooms.list_rooms(&RoomFilter::default()).await {
        Ok(all) => all,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let mut shared = Vec::new();
    for room in all {
        let members = match rooms.list_members(&room.room_id).await {
            Ok(members) => members,
            // Gone between the listing and now.
            Err(SourceError::NotFound) => continue,
            Err(e) => return e.to_problem().with_instance(instance).into_response(),
        };
        let theirs = members
            .iter()
            .filter(|m| m.membership == "join" && server_of(&m.user_id) == Some(&server_name))
            .count() as u64;
        if theirs > 0 {
            shared.push(AdminDestinationRoom {
                room_id: room.room_id,
                name: room.name,
                canonical_alias: room.canonical_alias,
                joined_members_count: room.joined_members_count,
                destination_members_count: theirs,
            });
        }
    }
    if shared.is_empty() {
        let known = match &state.federation {
            Some(federation) => match federation.get_destination(&server_name).await {
                Ok(destination) => destination.is_some(),
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            },
            None => false,
        };
        if !known {
            return Problem::not_found()
                .with_detail(format!(
                    "this server has never tried to reach {server_name} and shares no room with it"
                ))
                .with_instance(instance)
                .into_response();
        }
    }
    shared.sort_by(|a, b| {
        b.destination_members_count
            .cmp(&a.destination_members_count)
            .then_with(|| a.room_id.cmp(&b.room_id))
    });
    axum::Json(Page::paginate(
        shared,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `GET /api/v1/federation/keys` (`admin:read`): this server's own signing keys.
pub(crate) async fn keys_list(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/federation/keys";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(federation) = &state.federation else {
        return unwired("federation", instance);
    };
    match federation.own_keys().await {
        Ok(keys) => axum::Json(keys).into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/federation/keys/{server_name}` (`admin:read`): what the key cache holds for
/// `server_name`; `404` when it holds nothing.
pub(crate) async fn keys_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
) -> Response {
    let instance = format!("/api/v1/federation/keys/{server_name}");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let Some(federation) = &state.federation else {
        return unwired("federation", &instance);
    };
    match federation.remote_keys(&server_name).await {
        Ok(Some(keys)) => axum::Json(keys).into_response(),
        Ok(None) => Problem::not_found()
            .with_detail(format!(
                "this server holds no keys for {server_name}: it has not needed to check one of \
                 its signatures since it started"
            ))
            .with_instance(instance)
            .into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/federation/keys/{server_name}/refresh` (`admin:write`, Task): fetches
/// `server_name`'s keys again. See the module docs.
pub(crate) async fn keys_refresh(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/federation/keys/{server_name}/refresh");
    let operation_id = "federation.keys.refresh";
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(federation) = state.federation.clone() else {
        return unwired("federation", &instance);
    };
    if server_name.is_empty() || server_name.contains(['/', '@', ' ']) {
        return Problem::not_found()
            .with_detail(format!("{server_name:?} is not a server name"))
            .with_instance(instance)
            .into_response();
    }
    // The body is empty; the idempotency key covers the server named in the path.
    let fingerprint = [server_name.as_bytes(), &body].concat();
    if let Err(response) = check_replay(&state, &headers, operation_id, &fingerprint, &instance) {
        return response;
    }
    let actor = principal.to_actor();
    let resource = ResourceRef::new("server", server_name.clone());
    let refresh = Refresh {
        federation,
        server_name: server_name.clone(),
        events: Arc::clone(&state.events),
        actor: actor.clone(),
    };
    let task = match &state.tasks {
        Some(tasks) => match tasks
            .spawn(
                REFRESH_TASK_ACTION,
                Some(resource.clone()),
                actor,
                move |_context| refresh.run(),
            )
            .await
        {
            Ok(task) => task,
            Err(e) => return e.to_problem().with_instance(instance).into_response(),
        },
        None => refresh.run_inline(resource.clone()).await,
    };
    tracing::info!(task = %task.id, server = %server_name, "an administrator asked to refetch a server's signing keys");
    if let Err(response) = record(
        &state,
        &principal,
        operation_id,
        "federation.keys_refresh_started",
        resource,
        Vec::new(),
        json!({ "server_name": server_name, "task_id": task.id }),
        StatusCode::ACCEPTED.as_u16(),
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        operation_id,
        &fingerprint,
        StatusCode::ACCEPTED,
        &task,
        &[("location", format!("/api/v1/tasks/{}", task.id))],
    )
}

/// One refetch of a server's keys: what the task runs.
struct Refresh {
    federation: Arc<dyn FederationSource>,
    server_name: String,
    events: Arc<crate::events::EventBus>,
    actor: Actor,
}

impl Refresh {
    // The shape `TaskRegistry::spawn` runs: its problem becomes the task's `error`.
    #[allow(clippy::result_large_err)]
    async fn run(self) -> Result<serde_json::Value, Problem> {
        match self.federation.refresh_remote_keys(&self.server_name).await {
            Ok(keys) => {
                tracing::info!(
                    server = %self.server_name,
                    keys = keys.keys.len(),
                    "refetched a server's signing keys"
                );
                let value = serde_json::to_value(&keys).unwrap_or_default();
                self.events.publish(
                    Event::new("federation.keys_refreshed", value.clone())
                        .with_resource(ResourceRef::new("server", self.server_name.clone()))
                        .with_actor(self.actor),
                );
                Ok(value)
            }
            Err(error) => {
                tracing::warn!(server = %self.server_name, %error, "could not refetch a server's signing keys");
                Err(error.to_problem())
            }
        }
    }

    /// [`Refresh::run`] inside the request, for a state with no task registry.
    async fn run_inline(self, resource: ResourceRef) -> Task {
        let mut task = Task::scheduled(REFRESH_TASK_ACTION, Some(resource), self.actor.clone());
        task.started_at = Some(hs_http::time::now_rfc3339());
        match self.run().await {
            Ok(result) => {
                task.status = TaskStatus::Succeeded;
                task.result = Some(result);
            }
            Err(problem) => {
                task.status = TaskStatus::Failed;
                task.error = Some(problem);
            }
        }
        task.finished_at = Some(hs_http::time::now_rfc3339());
        task
    }
}

/// What an in-memory [`FederationSource`] (`crate::sources::InMemoryFederationSource`) serves
/// for keys: set by tests and `hs-admin-mock`.
#[derive(Debug, Clone, Default)]
pub struct InMemoryKeys {
    /// This server's own keys.
    pub own: Vec<AdminSigningKey>,
    /// The cache, by server.
    pub cached: BTreeMap<String, AdminRemoteServerKeys>,
    /// What a refresh finds, by server; a server missing here cannot be reached.
    pub reachable: BTreeMap<String, AdminRemoteServerKeys>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler_kit::testing::{call, state};
    use crate::model::{AdminDestination, AdminRoom, AdminRoomMember};
    use crate::sources::{InMemoryFederationSource, InMemoryRoomDirectory};

    fn key(id: &str, old: bool) -> AdminSigningKey {
        AdminSigningKey {
            key_id: format!("ed25519:{id}"),
            algorithm: "ed25519".to_owned(),
            public_key: format!("pk-{id}"),
            valid_until_at: None,
            old,
        }
    }

    fn remote(server: &str, ids: &[&str]) -> AdminRemoteServerKeys {
        AdminRemoteServerKeys {
            server_name: server.to_owned(),
            keys: ids.iter().map(|id| key(id, false)).collect(),
            cached_at: Some("2026-09-28T00:00:00Z".to_owned()),
        }
    }

    fn federation() -> InMemoryFederationSource {
        InMemoryFederationSource::new()
            .with_destination(AdminDestination {
                server_name: "lonely.example".to_owned(),
                ..AdminDestination::default()
            })
            .with_keys(InMemoryKeys {
                own: vec![key("a_1", false)],
                cached: [(
                    "remote.example".to_owned(),
                    remote("remote.example", &["old"]),
                )]
                .into(),
                reachable: [(
                    "remote.example".to_owned(),
                    remote("remote.example", &["new"]),
                )]
                .into(),
            })
    }

    fn member(user_id: &str, membership: &str) -> AdminRoomMember {
        AdminRoomMember {
            user_id: user_id.to_owned(),
            membership: membership.to_owned(),
            display_name: None,
            avatar_url: None,
        }
    }

    fn room(id: &str, joined: u64) -> AdminRoom {
        AdminRoom {
            room_id: id.to_owned(),
            joined_members_count: joined,
            ..AdminRoom::default()
        }
    }

    #[tokio::test]
    async fn the_rooms_shared_with_a_destination_most_of_its_members_first() {
        let (state, _) = state();
        let rooms = InMemoryRoomDirectory::new()
            .with_room(room("!a:here", 2))
            .with_room(room("!b:here", 4))
            .with_room(room("!c:here", 1))
            .with_member("!a:here", member("@me:here", "join"))
            .with_member("!a:here", member("@x:remote.example", "join"))
            .with_member("!b:here", member("@me:here", "join"))
            .with_member("!b:here", member("@x:remote.example", "join"))
            .with_member("!b:here", member("@y:remote.example", "join"))
            .with_member("!b:here", member("@z:remote.example", "leave"))
            .with_member("!c:here", member("@me:here", "join"))
            .with_member("!c:here", member("@x:remote.example:8448", "join"));
        let state = state
            .with_rooms(Arc::new(rooms))
            .with_federation(Arc::new(federation()));
        let uri = "/api/v1/federation/destinations/remote.example/rooms?include_total=true";
        let (status, _, page) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{page}");
        assert_eq!(items[0]["room_id"], "!b:here");
        assert_eq!(items[0]["destination_members_count"], 2);
        assert_eq!(items[0]["joined_members_count"], 4);
        assert_eq!(items[1]["room_id"], "!a:here");
        assert_eq!(page["total"], 2);

        // Known but sharing nothing: an empty page. Neither: 404.
        let uri = "/api/v1/federation/destinations/lonely.example/rooms";
        let (status, _, page) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"], json!([]));
        let uri = "/api/v1/federation/destinations/nobody.example/rooms";
        let (status, _, _) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn own_keys_and_the_cache_are_readable_with_admin_read() {
        let (state, _) = state();
        let state = state.with_federation(Arc::new(federation()));
        let (status, _, keys) = call(
            &state,
            "GET",
            "/api/v1/federation/keys",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{keys}");
        assert_eq!(keys[0]["key_id"], "ed25519:a_1");
        let (status, _, cached) = call(
            &state,
            "GET",
            "/api/v1/federation/keys/remote.example",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cached}");
        assert_eq!(cached["keys"][0]["key_id"], "ed25519:old");
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/federation/keys/unknown.example",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(&state, "GET", "/api/v1/federation/keys", None, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    async fn settled(state: &AdminState, id: &str) -> serde_json::Value {
        for _ in 0..200 {
            let uri = format!("/api/v1/tasks/{id}");
            let (_, _, task) = call(state, "GET", &uri, Some("read"), None, None).await;
            if task["status"] != "running" {
                return task;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("task {id} never ended");
    }

    #[tokio::test]
    async fn a_refresh_is_an_audited_task_that_ends_with_what_the_cache_holds() {
        let (state, audit) = state();
        let state = state
            .with_federation(Arc::new(federation()))
            .with_tasks(crate::tasks::TaskRegistry::in_memory());
        let mut events = state.events.subscribe();
        let uri = "/api/v1/federation/keys/remote.example/refresh";
        let (status, _, _) = call(&state, "POST", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, headers, task) = call(&state, "POST", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{task}");
        assert_eq!(task["action"], REFRESH_TASK_ACTION);
        assert_eq!(task["resource"]["id"], "remote.example");
        let id = task["id"].as_str().unwrap();
        assert_eq!(
            headers.get("location").unwrap(),
            &format!("/api/v1/tasks/{id}")
        );
        let task = settled(&state, id).await;
        assert_eq!(task["status"], "succeeded", "{task}");
        assert_eq!(task["result"]["keys"][0]["key_id"], "ed25519:new");
        let (_, _, cached) = call(
            &state,
            "GET",
            "/api/v1/federation/keys/remote.example",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(
            cached["keys"][0]["key_id"], "ed25519:new",
            "the cache now holds it"
        );
        let mut types = Vec::new();
        while let Ok(event) = events.try_recv() {
            types.push(event.r#type);
        }
        assert!(
            types.contains(&"federation.keys_refresh_started".to_owned()),
            "{types:?}"
        );
        assert!(
            types.contains(&"federation.keys_refreshed".to_owned()),
            "{types:?}"
        );
        let entries =
            crate::audit::AuditSink::query(audit.as_ref(), &crate::audit::AuditFilter::default())
                .await
                .unwrap();
        let entry = entries
            .iter()
            .find(|e| e.action == "federation.keys.refresh")
            .unwrap();
        assert_eq!(entry.outcome.status, 202);

        // A server that cannot be reached: the task fails and says why.
        let uri = "/api/v1/federation/keys/gone.example/refresh";
        let (status, _, task) = call(&state, "POST", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let task = settled(&state, task["id"].as_str().unwrap()).await;
        assert_eq!(task["status"], "failed", "{task}");
        assert!(
            task["error"]["detail"]
                .as_str()
                .unwrap()
                .contains("gone.example")
        );
    }
}
