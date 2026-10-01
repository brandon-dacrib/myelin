//! Tests of [`super::RoomActor::create_placed`]'s rule that a new room's ID is new: a
//! version-12 room ID is its create event's hash, and two creates of a room by one user with
//! the same content in the same millisecond build one create event. Each test here fails with
//! the claim in `PersistKind::NewRoom` switched off -- the second create then answers the first
//! room's ID, and the registry keeps one room where two were asked for.

use std::collections::BTreeSet;
use std::sync::Arc;

use hs_cluster::store::ClusterStore;
use hs_cluster::{Fence, Ownership, OwnershipEvent, ReplicaId, ShardId, ShardLayout, ShardMap};
use hs_kv::memory::MemoryBackend;
use ruma::{OwnedRoomId, RoomVersionId, user_id};

use crate::actor::{CreateRoomRequest, RoomActorHandle};
use crate::error::RoomError;
use crate::fencing::RoomFencing;
use crate::identity::HomeserverIdentity;
use crate::registry::RoomRegistry;

/// A frozen clock: every create below happens at this one millisecond.
const NOW: i64 = 1_700_000_000_000;

fn v12() -> CreateRoomRequest {
    CreateRoomRequest {
        room_version: Some(RoomVersionId::try_from("12").expect("12 is a room version")),
        ..CreateRoomRequest::default()
    }
}

fn registry(
    backend: MemoryBackend,
    identity: HomeserverIdentity,
) -> Arc<RoomRegistry<MemoryBackend>> {
    Arc::new(RoomRegistry::open(backend, identity).expect("an in-memory registry opens"))
}

async fn room_id(handle: &RoomActorHandle<MemoryBackend>) -> OwnedRoomId {
    handle.query(|a| a.room_id().to_owned()).await
}

/// The create event's `origin_server_ts` of the room behind `handle`.
async fn create_ts(handle: &RoomActorHandle<MemoryBackend>) -> i64 {
    handle
        .query(|a| {
            a.state_event("m.room.create", "")
                .expect("the state is readable")
                .expect("a created room has a create event")
                .header()
                .origin_server_ts
        })
        .await
}

/// The bug Sytest found: alice creates two version-12 rooms with the same (empty) request in
/// the same millisecond. They are two rooms, each with its own ID, both in the store and both
/// in alice's joined rooms, and a message sent into one is not in the other. The second
/// create event was moved back in time to get a different ID; the move is counted.
#[tokio::test]
async fn two_v12_rooms_created_by_one_user_in_one_millisecond_are_two_rooms() {
    let registry = registry(MemoryBackend::new(), HomeserverIdentity::for_tests("hs1"));
    let alice = user_id!("@alice:hs1").to_owned();
    let taken_before = crate::metrics::create_room_id_taken();

    let first = registry
        .create_room(alice.clone(), v12(), NOW)
        .await
        .unwrap();
    let second = registry
        .create_room(alice.clone(), v12(), NOW)
        .await
        .unwrap();
    let (first_id, second_id) = (room_id(&first).await, room_id(&second).await);

    assert_ne!(first_id, second_id, "two creates, two rooms");
    let all: BTreeSet<_> = registry.list_all_room_ids().unwrap().into_iter().collect();
    assert_eq!(all, BTreeSet::from([first_id.clone(), second_id.clone()]));
    let joined: BTreeSet<_> = registry
        .rooms_joined_by_user(&alice)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(joined, all, "alice is joined to both");

    assert_eq!(
        create_ts(&first).await,
        NOW,
        "the first create kept its timestamp"
    );
    let moved = create_ts(&second).await;
    assert!(
        (NOW - 1024..NOW).contains(&moved),
        "the second create event went back at most a second: {moved}"
    );
    assert!(
        crate::metrics::create_room_id_taken() > taken_before,
        "the taken id is counted"
    );

    // Each handle is its own room: a message in the first is not in the second.
    first
        .send_event(
            alice.clone(),
            "m.room.message".to_owned(),
            None,
            serde_json::json!({"msgtype": "m.text", "body": "only in the first"}),
            None,
            NOW + 1,
        )
        .await
        .unwrap();
    let in_second = second
        .query(|a| {
            a.paginate(None, crate::timeline::Direction::Backward, 50)
                .0
                .iter()
                .filter(|e| e.header().event_type == "m.room.message")
                .count()
        })
        .await;
    assert_eq!(in_second, 0, "the second room has no message");
}

/// The way Sytest's runs create them: at once. Twelve identical version-12 creates by one user
/// in one millisecond, concurrently on a multi-threaded runtime -- the claim is one serializable
/// transaction, so two of them cannot both take an ID -- are twelve rooms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_v12_creates_are_all_distinct_rooms() {
    let registry = registry(MemoryBackend::new(), HomeserverIdentity::for_tests("hs1"));
    let alice = user_id!("@alice:hs1").to_owned();

    let creates: Vec<_> = (0..12)
        .map(|_| {
            let registry = registry.clone();
            let alice = alice.clone();
            tokio::spawn(async move { registry.create_room(alice, v12(), NOW).await })
        })
        .collect();
    let mut ids = BTreeSet::new();
    for create in creates {
        let handle = create.await.unwrap().unwrap();
        ids.insert(room_id(&handle).await);
    }

    assert_eq!(ids.len(), 12, "twelve creates, twelve rooms: {ids:?}");
    let all: BTreeSet<_> = registry.list_all_room_ids().unwrap().into_iter().collect();
    assert_eq!(all, ids);
    assert_eq!(registry.rooms_joined_by_user(&alice).unwrap().len(), 12);
}

/// Two replicas share one store; a room one of them created is not resident on the other.
/// The check is against the store, not the registry's memory, so the second replica's
/// identical create is still a new room.
#[tokio::test]
async fn an_identical_create_on_another_registry_over_the_same_store_is_a_new_room() {
    let backend = MemoryBackend::new();
    let identity = HomeserverIdentity::for_tests("hs1");
    let a = registry(backend.clone(), identity.clone());
    let b = registry(backend, identity);
    let alice = user_id!("@alice:hs1").to_owned();

    let on_a = room_id(&a.create_room(alice.clone(), v12(), NOW).await.unwrap()).await;
    let on_b = room_id(&b.create_room(alice.clone(), v12(), NOW).await.unwrap()).await;

    assert_ne!(on_a, on_b);
    assert_eq!(b.list_all_room_ids().unwrap().len(), 2);
}

/// Owns exactly the room shards in `owned`, each with an inert fence (as `crate::fencing`'s
/// own tests do).
struct OwnsShards {
    me: ReplicaId,
    owned: Vec<u32>,
}

impl Ownership for OwnsShards {
    fn me(&self) -> &ReplicaId {
        &self.me
    }

    fn owner_of(&self, shard: ShardId) -> Option<ReplicaId> {
        Some(if self.is_mine(shard) {
            self.me.clone()
        } else {
            ReplicaId::new("elsewhere")
        })
    }

    fn is_mine(&self, shard: ShardId) -> bool {
        shard.kind == hs_cluster::ShardKind::Room && self.owned.contains(&shard.index)
    }

    fn fence(&self, shard: ShardId) -> Option<Fence> {
        self.is_mine(shard).then(|| Fence::inert(shard))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<OwnershipEvent> {
        tokio::sync::broadcast::channel(1).1
    }

    fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<ShardMap>> {
        tokio::sync::watch::channel(Arc::new(ShardMap::default())).1
    }
}

/// Placement (decision 0020) and uniqueness share one loop and one bound. With one room shard
/// of four owned, every identical create walks back from the same millisecond onto the same
/// first owned ID; twenty of them by one user at one frozen millisecond are still twenty rooms,
/// each on the owned shard, within the bound of 16 attempts a shard.
#[tokio::test]
async fn a_burst_of_identical_v12_creates_is_placed_here_and_kept_apart() {
    const LAYOUT: ShardLayout = ShardLayout::small(4);
    let backend = MemoryBackend::new();
    let registry = registry(backend.clone(), HomeserverIdentity::for_tests("hs1"));
    registry.install_fencing(Arc::new(RoomFencing {
        ownership: Arc::new(OwnsShards {
            me: ReplicaId::new("hs-a"),
            owned: vec![3],
        }),
        layout: LAYOUT,
        cluster_store: ClusterStore::open(backend).unwrap(),
    }));
    let alice = user_id!("@alice:hs1").to_owned();

    let mut ids = BTreeSet::new();
    for _ in 0..20 {
        let handle = registry
            .create_room(alice.clone(), v12(), NOW)
            .await
            .unwrap();
        let id = room_id(&handle).await;
        assert_eq!(LAYOUT.room_shard(id.as_str()).index, 3, "{id}");
        ids.insert(id);
    }
    assert_eq!(ids.len(), 20, "twenty creates, twenty rooms");
    assert_eq!(registry.rooms_joined_by_user(&alice).unwrap().len(), 20);
}

/// An ID the caller chose (an upgrade's replacement room, the shard gate's pre-assigned one)
/// is not swapped for another, but a room that already has it is not answered either: the
/// create is refused, `M_ROOM_IN_USE`, and the existing room is untouched.
#[tokio::test]
async fn a_chosen_id_a_room_already_has_is_refused() {
    let registry = registry(MemoryBackend::new(), HomeserverIdentity::for_tests("hs1"));
    let alice = user_id!("@alice:hs1").to_owned();
    let chosen = ruma::RoomId::new_v1(ruma::server_name!("hs1"));
    let request = |name: &str| CreateRoomRequest {
        room_id: Some(chosen.clone()),
        name: Some(name.to_owned()),
        ..CreateRoomRequest::default()
    };

    let first = registry
        .create_room(alice.clone(), request("first"), NOW)
        .await
        .unwrap();
    assert_eq!(room_id(&first).await, chosen);
    let err = registry
        .create_room(alice.clone(), request("second"), NOW + 5)
        .await
        .err()
        .expect("a chosen id already in use is refused");
    assert!(
        matches!(err, RoomError::RoomAlreadyExists(_)),
        "got {err:?}"
    );
    assert_eq!(err.to_matrix_error().errcode.as_str(), "M_ROOM_IN_USE");

    let resident = registry.get_or_load(&chosen).await.unwrap();
    let name = resident
        .query(|a| {
            a.state_event("m.room.name", "").unwrap().and_then(|e| {
                e.json()
                    .get("content")?
                    .as_object()?
                    .get("name")?
                    .as_str()
                    .map(str::to_owned)
            })
        })
        .await;
    assert_eq!(
        name.as_deref(),
        Some("first"),
        "the existing room is untouched"
    );
    assert_eq!(registry.list_all_room_ids().unwrap(), vec![chosen]);
}
