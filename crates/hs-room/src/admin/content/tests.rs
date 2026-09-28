//! The room long tail against a real registry over the in-memory backend: purges that survive a
//! reload, forks pruned, a deleted room gone after eviction, and the reads.

use std::sync::Arc;

use hs_admin::rooms::{
    DeleteRoomRequest, NewRoomRequest, NoProgress, PurgeHistoryRequest, RoomContentSource,
    TimelineDirection,
};
use hs_admin::sources::{RoomDirectory, SourceError};
use hs_kv::memory::MemoryBackend;
use ruma::{OwnedRoomId, UserId, user_id};
use serde_json::json;

use crate::actor::CreateRoomRequest;
use crate::admin::RoomRegistryDirectory;
use crate::identity::HomeserverIdentity;
use crate::membership::Action;
use crate::registry::RoomRegistry;

const SERVER: &str = "admin.test";

fn registry() -> Arc<RoomRegistry<MemoryBackend>> {
    Arc::new(
        RoomRegistry::open(MemoryBackend::new(), HomeserverIdentity::for_tests(SERVER))
            .expect("opening an in-memory registry cannot fail"),
    )
}

async fn room(registry: &RoomRegistry<MemoryBackend>, preset: &str) -> OwnedRoomId {
    let handle = registry
        .create_room(
            user_id!("@alice:admin.test").to_owned(),
            CreateRoomRequest {
                preset: Some(preset.to_owned()),
                name: Some("Lounge".to_owned()),
                ..Default::default()
            },
            1_000,
        )
        .await
        .unwrap();
    handle.query(|actor| actor.room_id().to_owned()).await
}

async fn say(
    registry: &RoomRegistry<MemoryBackend>,
    room_id: &OwnedRoomId,
    sender: &UserId,
    body: &str,
    ts: i64,
) -> String {
    registry
        .get_or_load(room_id)
        .await
        .unwrap()
        .send_event(
            sender.to_owned(),
            "m.room.message".to_owned(),
            None,
            json!({ "msgtype": "m.text", "body": body }),
            None,
            ts,
        )
        .await
        .unwrap()
        .event_id()
        .to_string()
}

async fn join(registry: &RoomRegistry<MemoryBackend>, room_id: &OwnedRoomId, user: &UserId) {
    registry
        .get_or_load(room_id)
        .await
        .unwrap()
        .membership(
            user.to_owned(),
            Action::Join,
            user.to_owned(),
            json!({}),
            2_000,
        )
        .await
        .unwrap();
}

async fn bodies(directory: &RoomRegistryDirectory<MemoryBackend>, room_id: &str) -> Vec<String> {
    directory
        .timeline(room_id, None, TimelineDirection::Forward, 1000)
        .await
        .unwrap()
        .events
        .iter()
        .filter_map(|e| e.content.get("body").and_then(|b| b.as_str()))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_purge_removes_old_remote_messages_keeps_state_and_local_and_survives_a_reload() {
    let registry = registry();
    let room_id = room(&registry, "public_chat").await;
    let remote = user_id!("@bob:remote.test");
    join(&registry, &room_id, remote).await;
    let old_remote = say(&registry, &room_id, remote, "old remote", 10_000).await;
    say(
        &registry,
        &room_id,
        user_id!("@alice:admin.test"),
        "old local",
        11_000,
    )
    .await;
    say(&registry, &room_id, remote, "new remote", 50_000).await;
    let directory = RoomRegistryDirectory::new(registry.clone());
    let state_before = directory.state(room_id.as_str()).await.unwrap().len();

    let outcome = directory
        .purge_history(
            room_id.as_str(),
            PurgeHistoryRequest {
                before_ts: Some(20_000),
                ..Default::default()
            },
            &NoProgress,
        )
        .await
        .unwrap();
    assert_eq!(outcome.purged, 1);
    assert_eq!(outcome.kept_local, 1);
    assert!(outcome.kept_state > 0);
    assert_eq!(
        bodies(&directory, room_id.as_str()).await,
        ["old local", "new remote"]
    );
    assert_eq!(
        directory
            .event(Some(room_id.as_str()), &old_remote)
            .await
            .unwrap(),
        None
    );
    assert_eq!(directory.event(None, &old_remote).await.unwrap(), None);

    // Reloaded from the store, the purge holds and the room still works.
    registry.evict_idle(std::time::Duration::ZERO).await;
    assert_eq!(
        bodies(&directory, room_id.as_str()).await,
        ["old local", "new remote"]
    );
    assert_eq!(
        directory.state(room_id.as_str()).await.unwrap().len(),
        state_before
    );
    say(&registry, &room_id, remote, "after", 60_000).await;

    // And with local events included, the local one goes too.
    let outcome = directory
        .purge_history(
            room_id.as_str(),
            PurgeHistoryRequest {
                before_ts: Some(20_000),
                delete_local_events: true,
                ..Default::default()
            },
            &NoProgress,
        )
        .await
        .unwrap();
    assert_eq!(outcome.purged, 1);
    assert_eq!(
        bodies(&directory, room_id.as_str()).await,
        ["new remote", "after"]
    );
}

#[tokio::test]
async fn a_fork_is_reported_and_pruned_to_the_newest_extremity() {
    let registry = registry();
    let room_id = room(&registry, "public_chat").await;
    let handle = registry.get_or_load(&room_id).await.unwrap();
    let base = handle
        .query(|actor| {
            let (id, _) = actor.forward_extremity_ids()[0].clone();
            actor.event_sn_of(&id).unwrap()
        })
        .await;
    say(
        &registry,
        &room_id,
        user_id!("@alice:admin.test"),
        "one",
        5_000,
    )
    .await;
    handle
        .administer(move |actor| {
            actor.send_event_citing(
                user_id!("@alice:admin.test").to_owned(),
                "m.room.message".to_owned(),
                None,
                json!({ "msgtype": "m.text", "body": "fork" }),
                None,
                6_000,
                &[base],
            )
        })
        .await
        .unwrap();
    let directory = RoomRegistryDirectory::new(registry.clone());
    let listed = directory
        .forward_extremities(room_id.as_str())
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);

    let pruned = directory
        .prune_forward_extremities(room_id.as_str())
        .await
        .unwrap();
    assert_eq!(pruned.deleted.len(), 1);
    assert_eq!(pruned.remaining.len(), 1);
    assert_eq!(pruned.remaining[0].event_id, listed[0].event_id);
    registry.evict_idle(std::time::Duration::ZERO).await;
    assert_eq!(
        directory
            .forward_extremities(room_id.as_str())
            .await
            .unwrap()
            .len(),
        1,
        "the prune is durable"
    );
    // A second prune has nothing to do.
    assert!(
        directory
            .prune_forward_extremities(room_id.as_str())
            .await
            .unwrap()
            .deleted
            .is_empty()
    );
    say(
        &registry,
        &room_id,
        user_id!("@alice:admin.test"),
        "after",
        7_000,
    )
    .await;
}

#[tokio::test]
async fn reads_state_aliases_context_nearest_event_and_media() {
    let registry = registry();
    let room_id = room(&registry, "public_chat").await;
    let alice = user_id!("@alice:admin.test");
    say(&registry, &room_id, alice, "first", 10_000).await;
    let middle = say(&registry, &room_id, alice, "second", 20_000).await;
    say(&registry, &room_id, alice, "third", 30_000).await;
    registry
        .get_or_load(&room_id)
        .await
        .unwrap()
        .send_event(
            alice.to_owned(),
            "m.room.message".to_owned(),
            None,
            json!({ "msgtype": "m.image", "body": "cat.png", "url": "mxc://admin.test/cat",
                    "info": { "thumbnail_url": "mxc://admin.test/cat-thumb" } }),
            None,
            40_000,
        )
        .await
        .unwrap();
    let directory = RoomRegistryDirectory::new(registry.clone());
    let room = room_id.as_str();

    let state = directory.state(room).await.unwrap();
    assert!(state.iter().any(|e| e.event_type == "m.room.name"));

    let at = directory
        .event_at(room, 15_000, TimelineDirection::Forward)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(at.event_id, middle);
    let at = directory
        .event_at(room, 25_000, TimelineDirection::Backward)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(at.event_id, middle);

    let context = directory.context(room, &middle, 1).await.unwrap().unwrap();
    assert_eq!(context.events_before[0].content["body"], "first");
    assert_eq!(context.events_after[0].content["body"], "third");
    assert!(!context.state.is_empty());

    let page = directory
        .timeline(room, None, TimelineDirection::Backward, 2)
        .await
        .unwrap();
    assert_eq!(page.events[0].content["body"], "cat.png");
    let older = directory
        .timeline(room, page.next.as_deref(), TimelineDirection::Backward, 2)
        .await
        .unwrap();
    assert_eq!(older.events[0].content["body"], "second");
    assert!(matches!(
        directory
            .timeline(room, Some("garbage"), TimelineDirection::Backward, 2)
            .await,
        Err(SourceError::InvalidField { .. })
    ));

    let mut media = directory.media(room).await.unwrap();
    media.sort();
    assert_eq!(
        media,
        [
            ("admin.test".to_owned(), "cat".to_owned()),
            ("admin.test".to_owned(), "cat-thumb".to_owned())
        ]
    );

    let added = directory
        .add_alias(room, "#lounge:admin.test", "@ops:admin.test")
        .await
        .unwrap();
    assert_eq!(added.creator.as_deref(), Some("@ops:admin.test"));
    assert!(matches!(
        directory
            .add_alias(room, "#lounge:admin.test", "@ops:admin.test")
            .await,
        Err(SourceError::Conflict(_))
    ));
    assert!(matches!(
        directory
            .add_alias(room, "#lounge:elsewhere.test", "@ops:admin.test")
            .await,
        Err(SourceError::InvalidField {
            pointer: "/alias",
            ..
        })
    ));
    let aliases = directory.aliases(room).await.unwrap();
    assert_eq!(aliases.len(), 1);
    assert_eq!(
        registry
            .resolve_alias(ruma::room_alias_id!("#lounge:admin.test"))
            .unwrap()
            .as_deref(),
        Some(room_id.as_ref())
    );
    directory
        .remove_alias(room, "#lounge:admin.test")
        .await
        .unwrap();
    assert!(matches!(
        directory.remove_alias(room, "#lounge:admin.test").await,
        Err(SourceError::NotFound)
    ));
}

#[tokio::test]
async fn an_administrator_joins_a_user_to_a_private_room_through_an_invite() {
    let registry = registry();
    let room_id = room(&registry, "private_chat").await;
    let directory = RoomRegistryDirectory::new(registry.clone());
    let member = directory
        .join(room_id.as_str(), "@carol:admin.test")
        .await
        .unwrap();
    assert_eq!(member.membership, "join");
    assert!(matches!(
        directory.join(room_id.as_str(), "@dave:remote.test").await,
        Err(SourceError::InvalidField {
            pointer: "/user_id",
            ..
        })
    ));
}

#[tokio::test]
async fn a_space_hierarchy_lists_known_and_unknown_children() {
    let registry = registry();
    let space = registry
        .create_room(
            user_id!("@alice:admin.test").to_owned(),
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                name: Some("Space".to_owned()),
                creation_content: json!({ "type": "m.space" }),
                ..Default::default()
            },
            1_000,
        )
        .await
        .unwrap();
    let space_id = space.query(|a| a.room_id().to_owned()).await;
    let child = room(&registry, "public_chat").await;
    for target in [child.as_str(), "!elsewhere:remote.test"] {
        space
            .send_event(
                user_id!("@alice:admin.test").to_owned(),
                "m.space.child".to_owned(),
                Some(target.to_owned()),
                json!({ "via": ["admin.test"] }),
                None,
                3_000,
            )
            .await
            .unwrap();
    }
    let directory = RoomRegistryDirectory::new(registry.clone());
    let nodes = directory.hierarchy(space_id.as_str(), 5).await.unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0].room_type.as_deref(), Some("m.space"));
    assert_eq!(nodes[0].children.len(), 2);
    assert!(nodes.iter().any(|n| n.room_id == child.as_str() && n.known));
    assert!(
        nodes
            .iter()
            .any(|n| n.room_id == "!elsewhere:remote.test" && !n.known)
    );
}

#[tokio::test]
async fn a_deleted_room_is_left_blocked_and_gone_and_its_members_move_on() {
    let registry = registry();
    let room_id = room(&registry, "public_chat").await;
    let bob = user_id!("@bob:admin.test");
    join(&registry, &room_id, bob).await;
    say(&registry, &room_id, bob, "hello", 5_000).await;
    let directory = RoomRegistryDirectory::new(registry.clone());
    directory
        .add_alias(room_id.as_str(), "#doomed:admin.test", "@ops:admin.test")
        .await
        .unwrap();
    registry.set_directory_visibility(&room_id, true).unwrap();

    let outcome = directory
        .delete_room(
            room_id.as_str(),
            DeleteRoomRequest {
                block: true,
                purge: true,
                message: Some("Moved".to_owned()),
                new_room: Some(NewRoomRequest {
                    name: Some("Elsewhere".to_owned()),
                    creator: "@alice:admin.test".to_owned(),
                }),
                requested_by: "@ops:admin.test".to_owned(),
            },
            &NoProgress,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.kicked_users,
        ["@alice:admin.test", "@bob:admin.test"]
    );
    assert!(outcome.failed_to_kick_users.is_empty());
    assert_eq!(outcome.local_aliases, ["#doomed:admin.test"]);
    assert!(outcome.events_deleted > 0);
    let new_room: OwnedRoomId = outcome.new_room_id.unwrap().try_into().unwrap();

    assert_eq!(directory.get_room(room_id.as_str()).await.unwrap(), None);
    assert!(!directory.exists(room_id.as_str()).await.unwrap());
    assert!(
        registry
            .list_all_room_ids()
            .unwrap()
            .iter()
            .all(|r| r != &room_id)
    );
    assert!(registry.list_published_room_ids().unwrap().is_empty());
    assert_eq!(
        registry
            .resolve_alias(ruma::room_alias_id!("#doomed:admin.test"))
            .unwrap(),
        None
    );
    assert!(registry.room_block_reason(&room_id).unwrap().is_some());
    assert!(
        !registry
            .rooms_joined_by_user(bob)
            .unwrap()
            .contains(&room_id)
    );
    assert!(
        registry
            .rooms_joined_by_user(bob)
            .unwrap()
            .contains(&new_room)
    );
    assert!(matches!(
        registry.get_or_load(&room_id).await,
        Err(crate::error::RoomError::RoomNotFound(_))
    ));
}
