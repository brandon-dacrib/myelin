//! HTTP tests of the room long-tail handlers against [`InMemoryRoomContent`].

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::*;
use crate::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
use crate::handler_kit::testing::{call, state as base_state};
use crate::media::{AdminMediaItem, InMemoryMediaSource};
use crate::tasks::TaskRegistry;

const ROOM: &str = "!lounge:example.org";
const ROOM_PATH: &str = "/api/v1/rooms/%21lounge%3Aexample.org";

fn event(n: i64, state_key: Option<&str>) -> AdminRoomEvent {
    AdminRoomEvent {
        event_id: format!("$e{n}"),
        room_id: ROOM.to_owned(),
        event_type: if state_key.is_some() {
            "m.room.topic".to_owned()
        } else {
            "m.room.message".to_owned()
        },
        sender: "@alice:example.org".to_owned(),
        content: json!({ "body": format!("message {n}") }),
        origin_server_ts: n * 1000,
        state_key: state_key.map(str::to_owned),
        redacted: false,
        redacts: None,
    }
}

fn state_event(event_type: &str, state_key: &str) -> AdminStateEvent {
    AdminStateEvent {
        event_id: format!("${event_type}{state_key}"),
        event_type: event_type.to_owned(),
        state_key: state_key.to_owned(),
        sender: "@alice:example.org".to_owned(),
        content: json!({}),
        origin_server_ts: 1,
    }
}

fn media_item(id: &str, quarantined: bool, protected: bool) -> AdminMediaItem {
    AdminMediaItem {
        server_name: "example.org".to_owned(),
        media_id: id.to_owned(),
        origin: "local".to_owned(),
        created_at: "2026-09-01T00:00:00.000Z".to_owned(),
        quarantined,
        protected,
        ..Default::default()
    }
}

fn room() -> InMemoryRoom {
    let mut timeline: Vec<AdminRoomEvent> = (1..=10).map(|n| event(n, None)).collect();
    timeline.insert(2, event(100, Some("")));
    timeline[2].origin_server_ts = 2500;
    InMemoryRoom {
        timeline,
        state: vec![
            state_event("m.room.name", ""),
            state_event("m.room.create", ""),
            state_event("m.room.member", "@alice:example.org"),
        ],
        members: vec![AdminRoomMember {
            user_id: "@alice:example.org".to_owned(),
            membership: "join".to_owned(),
            display_name: None,
            avatar_url: None,
        }],
        extremities: vec![
            AdminForwardExtremity {
                event_id: "$e10".to_owned(),
                event_type: "m.room.message".to_owned(),
                sender: "@alice:example.org".to_owned(),
                depth: 12,
                origin_server_ts: 10_000,
                state_key: None,
            },
            AdminForwardExtremity {
                event_id: "$fork".to_owned(),
                event_type: "m.room.message".to_owned(),
                sender: "@bob:remote.example".to_owned(),
                depth: 11,
                origin_server_ts: 9_500,
                state_key: None,
            },
        ],
        media: vec![
            ("example.org".to_owned(), "cat".to_owned()),
            ("example.org".to_owned(), "dog".to_owned()),
            ("example.org".to_owned(), "safe".to_owned()),
            ("elsewhere.example".to_owned(), "not-held".to_owned()),
        ],
        children: vec![
            "!child:example.org".to_owned(),
            "!far:remote.example".to_owned(),
        ],
        name: Some("Lounge".to_owned()),
        room_type: Some("m.space".to_owned()),
        ..Default::default()
    }
}

fn wired() -> (
    AdminState,
    Arc<InMemoryAuditSink>,
    Arc<InMemoryRoomContent>,
    Arc<InMemoryMediaSource>,
) {
    let (state, audit) = base_state();
    let content = Arc::new(
        InMemoryRoomContent::new()
            .with_room(ROOM, room())
            .with_room(
                "!child:example.org",
                InMemoryRoom {
                    name: Some("Child".to_owned()),
                    ..Default::default()
                },
            ),
    );
    let media = Arc::new(
        InMemoryMediaSource::new()
            .with_item(media_item("cat", false, false))
            .with_item(media_item("dog", true, false))
            .with_item(media_item("safe", false, true)),
    );
    let state = state
        .with_room_content(content.clone())
        .with_media(media.clone())
        .with_tasks(TaskRegistry::in_memory());
    (state, audit, content, media)
}

async fn actions(audit: &InMemoryAuditSink, action: &str) -> Vec<crate::model::AuditEntry> {
    audit
        .query(&AuditFilter {
            action: Some(action.to_owned()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
}

async fn settled(state: &AdminState, id: &str) -> Value {
    for _ in 0..400 {
        let (status, _, task) = call(
            state,
            "GET",
            &format!("/api/v1/tasks/{id}"),
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{task}");
        if ["succeeded", "failed", "cancelled"].contains(&task["status"].as_str().unwrap()) {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("task {id} never ended");
}

#[tokio::test]
async fn nothing_is_served_until_a_source_is_wired() {
    let (state, _) = base_state();
    let (status, _, body) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/state"),
        Some("admin"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn state_is_sorted_filtered_and_readable_by_a_moderator() {
    let (state, _, _, _) = wired();
    let (status, _, page) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/state"),
        Some("mod-read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let types: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["m.room.create", "m.room.member", "m.room.name"]);
    let (_, _, page) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/state?type=m.room.member"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let (status, _, _) = call(
        &state,
        "GET",
        "/api/v1/rooms/%21nope%3Aexample.org/state",
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn message_content_needs_admin_read_and_every_read_is_audited() {
    let (state, audit, _, _) = wired();
    let (status, _, body) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/messages"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(actions(&audit, "rooms.content.read").await.is_empty());

    let (status, _, page) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/messages?limit=4"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["$e10", "$e9", "$e8", "$e7"]);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let (_, _, older) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/messages?limit=4&cursor={cursor}"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(older["items"][0]["event_id"], "$e6");

    let (status, _, event) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/events/%24e3"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(event["content"]["body"], "message 3");
    let (status, _, event) = call(
        &state,
        "GET",
        "/api/v1/events/%24e4",
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(event["room_id"], ROOM);
    let (status, _, _) = call(
        &state,
        "GET",
        "/api/v1/events/%24nope",
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let reads = actions(&audit, "rooms.content.read").await;
    assert_eq!(reads.len(), 4, "a failed read is not recorded as a read");
    assert!(reads.iter().all(|e| e.actor.id == "@viewer:example.org"));
    assert!(
        reads
            .iter()
            .any(|e| e.request.as_ref().unwrap().path.ends_with("/messages"))
    );

    let (status, _, problem) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/messages?cursor=nonsense"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["errors"][0]["pointer"], "/cursor");
}

#[tokio::test]
async fn the_event_nearest_a_timestamp_and_its_context() {
    let (state, _, _, _) = wired();
    let (status, _, event) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/events/at?ts=4500"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{event}");
    assert_eq!(event["event_id"], "$e5");
    let (_, _, event) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/events/at?ts=4500&dir=b"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(event["event_id"], "$e4");
    let (status, _, _) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/events/at?ts=999999"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, problem) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/events/at"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["errors"][0]["pointer"], "/ts");

    let (status, _, context) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/events/%24e5/context?limit=2"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{context}");
    assert_eq!(context["event"]["event_id"], "$e5");
    assert_eq!(context["events_before"][0]["event_id"], "$e4");
    assert_eq!(context["events_after"][0]["event_id"], "$e6");
    assert_eq!(context["events_before"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn aliases_are_added_listed_and_removed_with_an_audit_trail() {
    let (state, audit, _, _) = wired();
    let path = format!("{ROOM_PATH}/aliases");
    let (status, _, added) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "alias": "#lounge:example.org" })),
        Some("k1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["creator"], "@mod:example.org");
    // Replayed, not added twice.
    let (status, headers, _) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "alias": "#lounge:example.org" })),
        Some("k1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["idempotency-replayed"], "true");
    let (status, _, _) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "alias": "#lounge:example.org" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, problem) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["errors"][0]["pointer"], "/alias");

    let (_, _, list) = call(&state, "GET", &path, Some("mod-read"), None, None).await;
    assert_eq!(list[0]["alias"], "#lounge:example.org");

    let (status, _, _) = call(
        &state,
        "DELETE",
        &format!("{path}/%23lounge%3Aexample.org"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call(
        &state,
        "DELETE",
        &format!("{path}/%23lounge%3Aexample.org"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(actions(&audit, "rooms.aliases.add").await.len(), 1);
    assert_eq!(actions(&audit, "rooms.aliases.remove").await.len(), 1);
}

#[tokio::test]
async fn the_hierarchy_walks_known_and_unknown_children() {
    let (state, _, _, _) = wired();
    let (status, _, page) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/hierarchy"),
        Some("mod-read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let nodes = page["items"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0]["room_id"], ROOM);
    assert_eq!(nodes[0]["depth"], 0);
    assert_eq!(nodes[1]["name"], "Child");
    assert_eq!(nodes[2]["known"], false);
}

#[tokio::test]
async fn an_administrator_joins_a_user_and_it_is_audited() {
    let (state, audit, content, _) = wired();
    let path = format!("{ROOM_PATH}/join");
    let (status, _, _) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "user_id": "@carol:example.org" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "join needs admin:write");
    let (status, _, member) = call(
        &state,
        "POST",
        &path,
        Some("admin"),
        Some(json!({ "user_id": "@carol:example.org" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{member}");
    assert_eq!(member["membership"], "join");
    assert!(
        content
            .room(ROOM)
            .unwrap()
            .members
            .iter()
            .any(|m| m.user_id == "@carol:example.org")
    );
    assert_eq!(actions(&audit, "rooms.join").await.len(), 1);
}

#[tokio::test]
async fn forward_extremities_are_listed_and_pruned_to_the_newest() {
    let (state, audit, _, _) = wired();
    let path = format!("{ROOM_PATH}/forward-extremities");
    let (_, _, list) = call(&state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (status, _, pruned) = call(&state, "DELETE", &path, Some("admin"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{pruned}");
    assert_eq!(pruned["deleted"], json!(["$fork"]));
    assert_eq!(pruned["remaining"][0]["event_id"], "$e10");
    let entries = actions(&audit, "rooms.forward_extremities.delete").await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].changes[0].to, Some(json!(1)));
}

#[tokio::test]
async fn room_media_is_listed_and_quarantined_as_a_task() {
    let (state, audit, _, media) = wired();
    let (status, _, page) = call(
        &state,
        "GET",
        &format!("{ROOM_PATH}/media"),
        Some("mod-read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["items"].as_array().unwrap().len(),
        3,
        "media this server does not hold is not listed"
    );
    let (status, headers, task) = call(
        &state,
        "POST",
        &format!("{ROOM_PATH}/media/quarantine"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{task}");
    let id = task["id"].as_str().unwrap();
    assert_eq!(headers["location"], format!("/api/v1/tasks/{id}"));
    assert_eq!(task["resource"]["type"], "room");
    let done = settled(&state, id).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(
        done["result"],
        json!({ "quarantined": 1, "already_quarantined": 1, "protected": 1 })
    );
    let cat = crate::media::MediaSource::get(media.as_ref(), "example.org", "cat")
        .await
        .unwrap()
        .unwrap();
    assert!(cat.quarantined);
    assert_eq!(actions(&audit, "rooms.media.quarantine").await.len(), 1);
}

#[tokio::test]
async fn purge_history_runs_as_a_task_and_the_timeline_loses_the_old_messages() {
    let (state, audit, content, _) = wired();
    let path = format!("{ROOM_PATH}/purge-history");
    let (status, _, problem) = call(&state, "POST", &path, Some("mod-write"), None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["errors"][0]["pointer"], "/before");
    let (status, _, problem) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "before": "yesterday" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    let (status, _, _) = call(
        &state,
        "POST",
        "/api/v1/rooms/%21nope%3Aexample.org/purge-history",
        Some("mod-write"),
        Some(json!({ "before": "1970-01-01T00:00:05Z" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, task) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "before": "1970-01-01T00:00:05Z" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{task}");
    assert_eq!(task["action"], "rooms.purge_history");
    let done = settled(&state, task["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["result"]["purged"], 4);
    assert_eq!(done["result"]["kept_state"], 1);
    let left: Vec<String> = content
        .room(ROOM)
        .unwrap()
        .timeline
        .iter()
        .map(|e| e.event_id.clone())
        .collect();
    assert_eq!(left.first().map(String::as_str), Some("$e100"));
    assert_eq!(actions(&audit, "rooms.purge_history").await.len(), 1);
}

#[tokio::test]
async fn delete_runs_as_a_task_and_the_room_is_gone() {
    let (state, audit, content, _) = wired();
    let path = format!("{ROOM_PATH}/delete");
    let (status, _, problem) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "new_room": { "name": "Elsewhere" } })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["errors"][0]["pointer"], "/new_room/creator");

    let (status, _, task) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "block": true })),
        Some("delete-1"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{task}");
    let done = settled(&state, task["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(
        done["result"]["kicked_users"],
        json!(["@alice:example.org"])
    );
    assert_eq!(done["result"]["purged"], true);
    assert!(content.room(ROOM).is_none());
    let entries = actions(&audit, "rooms.delete").await;
    assert_eq!(entries.len(), 1);
    // The same key again is the same task, not a second deletion.
    let (status, headers, again) = call(
        &state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({ "block": true })),
        Some("delete-1"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(headers["idempotency-replayed"], "true");
    assert_eq!(again["id"], task["id"]);
}
