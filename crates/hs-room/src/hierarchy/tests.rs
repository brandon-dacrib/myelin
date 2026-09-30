//! The space hierarchy against a real registry over the in-memory backend: the order of
//! children, who may see which room, the walk's pages and tokens, and the federation seam
//! through a fake that answers as another server would.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use hs_kv::memory::MemoryBackend;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, room_id, user_id};
use serde_json::{Value, json};

use super::*;
use crate::actor::{CreateRoomRequest, InitialStateEvent};
use crate::identity::HomeserverIdentity;
use crate::membership::Action;

const SERVER: &str = "hs1";
fn alice() -> OwnedUserId {
    user_id!("@alice:hs1").to_owned()
}
fn bob() -> OwnedUserId {
    user_id!("@bob:hs1").to_owned()
}

fn registry() -> Arc<RoomRegistry<MemoryBackend>> {
    Arc::new(
        RoomRegistry::open(MemoryBackend::new(), HomeserverIdentity::for_tests(SERVER))
            .expect("opening an in-memory registry cannot fail"),
    )
}

struct RoomSpec {
    preset: &'static str,
    name: &'static str,
    space: bool,
    world_readable: bool,
    join_rules: Option<Value>,
    room_version: Option<&'static str>,
}

impl RoomSpec {
    fn public(name: &'static str) -> Self {
        Self {
            preset: "public_chat",
            name,
            space: false,
            world_readable: false,
            join_rules: None,
            room_version: None,
        }
    }
    fn private(name: &'static str) -> Self {
        Self {
            preset: "private_chat",
            ..Self::public(name)
        }
    }
    fn space(mut self) -> Self {
        self.space = true;
        self
    }
    fn world_readable(mut self) -> Self {
        self.world_readable = true;
        self
    }
    fn restricted_to(mut self, room: &RoomId) -> Self {
        self.join_rules = Some(json!({
            "join_rule": "restricted",
            "allow": [{ "type": "m.room_membership", "room_id": room, "via": [SERVER] }],
        }));
        self.room_version = Some("8");
        self
    }
}

async fn create(registry: &RoomRegistry<MemoryBackend>, spec: RoomSpec, ts: i64) -> OwnedRoomId {
    let mut initial_state = Vec::new();
    if spec.world_readable {
        initial_state.push(InitialStateEvent {
            event_type: "m.room.history_visibility".to_owned(),
            state_key: String::new(),
            content: json!({ "history_visibility": "world_readable" }),
        });
    }
    if let Some(join_rules) = spec.join_rules {
        initial_state.push(InitialStateEvent {
            event_type: "m.room.join_rules".to_owned(),
            state_key: String::new(),
            content: join_rules,
        });
    }
    let handle = registry
        .create_room(
            alice(),
            CreateRoomRequest {
                room_version: spec
                    .room_version
                    .map(|v| ruma::RoomVersionId::try_from(v).unwrap()),
                preset: Some(spec.preset.to_owned()),
                name: Some(spec.name.to_owned()),
                creation_content: if spec.space {
                    json!({ "type": "m.space" })
                } else {
                    json!({})
                },
                initial_state,
                ..Default::default()
            },
            ts,
        )
        .await
        .unwrap();
    handle.query(|actor| actor.room_id().to_owned()).await
}

async fn link(
    registry: &RoomRegistry<MemoryBackend>,
    parent: &RoomId,
    child: &RoomId,
    content: Value,
    ts: i64,
) {
    registry
        .get_or_load(parent)
        .await
        .unwrap()
        .send_event(
            alice(),
            "m.space.child".to_owned(),
            Some(child.to_string()),
            content,
            None,
            ts,
        )
        .await
        .unwrap();
}

async fn join(registry: &RoomRegistry<MemoryBackend>, room: &RoomId, user: &UserId, ts: i64) {
    registry
        .get_or_load(room)
        .await
        .unwrap()
        .membership(
            user.to_owned(),
            Action::Join,
            user.to_owned(),
            json!({}),
            ts,
        )
        .await
        .unwrap();
}

async fn invite(registry: &RoomRegistry<MemoryBackend>, room: &RoomId, user: &UserId, ts: i64) {
    registry
        .get_or_load(room)
        .await
        .unwrap()
        .membership(alice(), Action::Invite, user.to_owned(), json!({}), ts)
        .await
        .unwrap();
}

fn request(root: &RoomId, requester: &UserId) -> HierarchyRequest {
    HierarchyRequest {
        root: root.to_owned(),
        requester: requester.to_owned(),
        suggested_only: false,
        limit: MAX_LIMIT,
        max_depth: None,
        from: None,
    }
}

fn ids(page: &HierarchyPage) -> Vec<String> {
    page.rooms
        .iter()
        .map(|room| room["room_id"].as_str().unwrap().to_owned())
        .collect()
}

fn children_of<'a>(page: &'a HierarchyPage, room: &RoomId) -> Vec<&'a str> {
    page.rooms
        .iter()
        .find(|r| r["room_id"] == room.as_str())
        .unwrap()["children_state"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["state_key"].as_str().unwrap())
        .collect()
}

// ------------------------------------------------------------------------------------------
// Ordering
// ------------------------------------------------------------------------------------------

fn child(room: &str, order: Option<&str>, ts: i64) -> ChildLink {
    ChildLink {
        room_id: RoomId::parse(room).unwrap().to_owned(),
        via: vec![SERVER.to_owned()],
        suggested: false,
        order: order.filter(|o| valid_order(o)).map(str::to_owned),
        origin_server_ts: ts,
        sender: alice().to_string(),
        content: json!({}),
    }
}

#[test]
fn children_sort_by_order_then_timestamp_then_room_id() {
    let mut links = vec![
        child("!late:hs1", None, 300),
        child("!b:hs1", Some("b"), 900),
        child("!early:hs1", None, 100),
        child("!a:hs1", Some("a"), 950),
        child("!tie2:hs1", None, 200),
        child("!tie1:hs1", None, 200),
    ];
    sort_child_links(&mut links);
    let order: Vec<&str> = links.iter().map(|l| l.room_id.as_str()).collect();
    assert_eq!(
        order,
        [
            "!a:hs1",
            "!b:hs1",
            "!early:hs1",
            "!tie1:hs1",
            "!tie2:hs1",
            "!late:hs1"
        ]
    );
}

#[test]
fn an_order_outside_the_spec_is_ignored() {
    assert!(valid_order("abc"));
    assert!(valid_order(" ~"));
    assert!(!valid_order("caf\u{e9}"));
    assert!(!valid_order("tab\there"));
    assert!(!valid_order(&"x".repeat(51)));
    assert!(valid_order(&"x".repeat(50)));
    // An invalid order sorts after every valid one, as if absent.
    let mut links = vec![
        child("!bad:hs1", Some("\u{e9}"), 1),
        child("!z:hs1", Some("z"), 2),
    ];
    sort_child_links(&mut links);
    assert_eq!(links[0].room_id, "!z:hs1");
}

#[tokio::test]
async fn a_space_lists_its_children_in_order_and_a_plain_room_none() {
    let registry = registry();
    let space = create(&registry, RoomSpec::public("Space").space(), 1_000).await;
    let room = create(&registry, RoomSpec::public("Room"), 1_000).await;
    let other = create(&registry, RoomSpec::public("Other"), 1_000).await;
    link(&registry, &space, &room, json!({ "via": [SERVER] }), 2_000).await;
    link(
        &registry,
        &space,
        &other,
        json!({ "via": [SERVER], "order": "0", "suggested": true }),
        3_000,
    )
    .await;
    // A link without `via` is a removed link; one with a non-string `via` is malformed.
    link(
        &registry,
        &space,
        room_id!("!removed:hs1"),
        json!({}),
        4_000,
    )
    .await;
    link(
        &registry,
        &space,
        room_id!("!broken:hs1"),
        json!({ "via": [1] }),
        4_000,
    )
    .await;
    // A plain room's links are not children of anything.
    link(&registry, &room, &other, json!({ "via": [SERVER] }), 5_000).await;

    let handle = registry.get_or_load(&space).await.unwrap();
    let links = handle.query(|a| child_links(a, false)).await.unwrap();
    let order: Vec<&str> = links.iter().map(|l| l.room_id.as_str()).collect();
    assert_eq!(order, [other.as_str(), room.as_str()]);
    let suggested = handle.query(|a| child_links(a, true)).await.unwrap();
    assert_eq!(suggested.len(), 1);
    assert_eq!(suggested[0].room_id, other);
    assert_eq!(
        suggested[0].stripped(),
        json!({
            "type": "m.space.child",
            "state_key": other,
            "sender": alice(),
            "content": { "via": [SERVER], "order": "0", "suggested": true },
            "origin_server_ts": 3_000,
        })
    );
    let plain = registry.get_or_load(&room).await.unwrap();
    assert!(
        plain
            .query(|a| child_links(a, false))
            .await
            .unwrap()
            .is_empty()
    );
}

// ------------------------------------------------------------------------------------------
// Summaries and visibility
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_summary_carries_the_public_rooms_chunk_fields() {
    let registry = registry();
    let space = create(&registry, RoomSpec::public("Space").space(), 1_000).await;
    let restricted = create(
        &registry,
        RoomSpec::public("Room").restricted_to(&space),
        1_000,
    )
    .await;
    let handle = registry.get_or_load(&space).await.unwrap();
    let summary = handle.query(summarize).await.unwrap();
    assert_eq!(summary.room_id, space);
    assert_eq!(summary.name.as_deref(), Some("Space"));
    assert_eq!(summary.room_type.as_deref(), Some("m.space"));
    assert_eq!(summary.join_rule.as_deref(), Some("public"));
    assert_eq!(summary.num_joined_members, 1);
    assert!(!summary.world_readable);
    assert!(!summary.guest_can_join);
    assert!(summary.allowed_room_ids.is_empty());
    let json = summary.to_json_with_children(&[]);
    assert_eq!(json["children_state"], json!([]));
    assert!(
        json.get("topic").is_none(),
        "unset fields are left out: {json}"
    );
    assert!(json.get("allowed_room_ids").is_none());

    let handle = registry.get_or_load(&restricted).await.unwrap();
    let summary = handle.query(summarize).await.unwrap();
    assert_eq!(summary.join_rule.as_deref(), Some("restricted"));
    assert_eq!(summary.allowed_room_ids, vec![space.clone()]);
    assert_eq!(summary.room_version.as_deref(), Some("8"));
}

#[tokio::test]
async fn the_spec_list_decides_who_may_see_a_room() {
    let registry = registry();
    let space = create(&registry, RoomSpec::public("Space").space(), 1_000).await;
    let public = create(&registry, RoomSpec::public("Public"), 1_000).await;
    let private = create(&registry, RoomSpec::private("Private"), 1_000).await;
    let peekable = create(
        &registry,
        RoomSpec::private("Peekable").world_readable(),
        1_000,
    )
    .await;
    let restricted = create(
        &registry,
        RoomSpec::private("Restricted").restricted_to(&space),
        1_000,
    )
    .await;
    let invited = create(&registry, RoomSpec::private("Invited"), 1_000).await;
    invite(&registry, &invited, &bob(), 2_000).await;

    let access = |room: &RoomId, user: &UserId| {
        let registry = registry.clone();
        let room = room.to_owned();
        let user = user.to_owned();
        async move {
            registry
                .get_or_load(&room)
                .await
                .unwrap()
                .query(move |a| local_access(a, &user))
                .await
        }
    };
    assert_eq!(access(&public, &bob()).await, Access::Visible);
    assert_eq!(access(&private, &alice()).await, Access::Visible, "joined");
    assert_eq!(access(&private, &bob()).await, Access::Hidden);
    assert_eq!(access(&peekable, &bob()).await, Access::Visible);
    assert_eq!(access(&invited, &bob()).await, Access::Visible, "invited");
    assert_eq!(
        access(&restricted, &bob()).await,
        Access::IfInAnyOf(vec![space.clone()])
    );

    // A server sees a room through its users: bob's invite is hs1's; hs2 has nobody.
    let server = |room: &RoomId, server: &'static str| {
        let registry = registry.clone();
        let room = room.to_owned();
        async move {
            registry
                .get_or_load(&room)
                .await
                .unwrap()
                .query(move |a| server_access(a, server))
                .await
        }
    };
    assert_eq!(server(&invited, "hs1").await, Access::Visible);
    assert_eq!(server(&invited, "hs2").await, Access::Hidden);
    assert_eq!(server(&public, "hs2").await, Access::Visible);
    assert_eq!(server(&peekable, "hs2").await, Access::Visible);
    assert_eq!(
        server(&restricted, "hs2").await,
        Access::IfInAnyOf(vec![space.clone()])
    );
}

#[test]
fn a_remote_summary_is_judged_by_its_join_rule_and_allowed_rooms() {
    let space = room_id!("!space:hs1").to_owned();
    let joined: HashSet<OwnedRoomId> = [space.clone()].into_iter().collect();
    let none = HashSet::new();
    assert_eq!(
        remote_access(&json!({ "join_rule": "public" }), &none),
        Some(true)
    );
    assert_eq!(
        remote_access(&json!({ "join_rule": "knock" }), &none),
        Some(true)
    );
    assert_eq!(
        remote_access(&json!({}), &none),
        Some(true),
        "no join rule reads as public"
    );
    assert_eq!(
        remote_access(
            &json!({ "join_rule": "invite", "world_readable": true }),
            &none
        ),
        Some(true)
    );
    let restricted = json!({ "join_rule": "restricted", "allowed_room_ids": [space] });
    assert_eq!(remote_access(&restricted, &joined), Some(true));
    assert_eq!(
        remote_access(&restricted, &none),
        None,
        "left to what this server holds"
    );
    assert_eq!(
        remote_access(&json!({ "join_rule": "invite" }), &joined),
        None
    );
}

// ------------------------------------------------------------------------------------------
// The walk
// ------------------------------------------------------------------------------------------

/// Complement's `TestClientSpacesSummary` tree: `Root -> R1, SS1, R2`; `SS1 -> SS2`;
/// `SS2 -> R3, R4`; `R2 -> R5` (ignored: R2 is not a space) and `R2 -> Root` as a parent link.
struct Tree {
    root: OwnedRoomId,
    r1: OwnedRoomId,
    ss1: OwnedRoomId,
    r2: OwnedRoomId,
    ss2: OwnedRoomId,
    r3: OwnedRoomId,
    r4: OwnedRoomId,
}

async fn tree(registry: &RoomRegistry<MemoryBackend>) -> Tree {
    let root = create(registry, RoomSpec::public("Root").space(), 1_000).await;
    let r1 = create(registry, RoomSpec::public("R1"), 1_000).await;
    let ss1 = create(registry, RoomSpec::public("Sub-Space 1").space(), 1_000).await;
    let r2 = create(registry, RoomSpec::public("R2"), 1_000).await;
    let ss2 = create(registry, RoomSpec::public("SS2").space(), 1_000).await;
    let r3 = create(registry, RoomSpec::public("R3"), 1_000).await;
    // R4 is bob's, world-readable, and alice is not in it.
    let r4 = {
        let handle = registry
            .create_room(
                bob(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("R4".to_owned()),
                    initial_state: vec![InitialStateEvent {
                        event_type: "m.room.history_visibility".to_owned(),
                        state_key: String::new(),
                        content: json!({ "history_visibility": "world_readable" }),
                    }],
                    ..Default::default()
                },
                1_000,
            )
            .await
            .unwrap();
        handle.query(|a| a.room_id().to_owned()).await
    };
    let r5 = create(registry, RoomSpec::public("R5"), 1_000).await;
    let via = json!({ "via": [SERVER] });
    let suggested = json!({ "via": [SERVER], "suggested": true });
    link(registry, &root, &r1, suggested.clone(), 2_001).await;
    link(registry, &root, &ss1, via.clone(), 2_002).await;
    link(registry, &root, &r2, suggested, 2_003).await;
    link(registry, &r2, &r5, via.clone(), 2_004).await;
    link(registry, &ss1, &ss2, via.clone(), 2_005).await;
    link(registry, &ss2, &r3, via.clone(), 2_006).await;
    // bob links R4 under SS2 as alice would: the sender does not matter to the walk.
    link(registry, &ss2, &r4, via, 2_007).await;
    Tree {
        root,
        r1,
        ss1,
        r2,
        ss2,
        r3,
        r4,
    }
}

#[tokio::test]
async fn the_whole_graph_comes_back_depth_first_with_children_state() {
    let registry = registry();
    let t = tree(&registry).await;
    let page = walk(&registry, &request(&t.root, &alice())).await.unwrap();
    assert_eq!(
        ids(&page),
        [&t.root, &t.r1, &t.ss1, &t.ss2, &t.r3, &t.r4, &t.r2]
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
    );
    assert!(page.next_batch.is_none());
    assert_eq!(
        children_of(&page, &t.root),
        [t.r1.as_str(), t.ss1.as_str(), t.r2.as_str()]
    );
    assert_eq!(children_of(&page, &t.ss1), [t.ss2.as_str()]);
    assert_eq!(children_of(&page, &t.ss2), [t.r3.as_str(), t.r4.as_str()]);
    assert!(
        children_of(&page, &t.r2).is_empty(),
        "a plain room has no children"
    );
    assert_eq!(page.stats.depth_reached, 3);
    assert_eq!(page.stats.remote_fetches, 0);
    let ss1 = page
        .rooms
        .iter()
        .find(|r| r["room_id"] == t.ss1.as_str())
        .unwrap();
    assert_eq!(ss1["room_type"], "m.space");
    assert_eq!(ss1["name"], "Sub-Space 1");
}

#[tokio::test]
async fn max_depth_and_suggested_only_prune_the_walk() {
    let registry = registry();
    let t = tree(&registry).await;
    let mut shallow = request(&t.root, &alice());
    shallow.max_depth = Some(1);
    let page = walk(&registry, &shallow).await.unwrap();
    assert_eq!(
        ids(&page),
        [&t.root, &t.r1, &t.ss1, &t.r2]
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
    );
    // The links of a space at the depth limit are still listed.
    assert_eq!(children_of(&page, &t.ss1), [t.ss2.as_str()]);

    let mut root_only = request(&t.root, &alice());
    root_only.max_depth = Some(0);
    let page = walk(&registry, &root_only).await.unwrap();
    assert_eq!(ids(&page), [t.root.to_string()]);

    let mut suggested = request(&t.root, &alice());
    suggested.suggested_only = true;
    let page = walk(&registry, &suggested).await.unwrap();
    assert_eq!(
        ids(&page),
        [&t.root, &t.r1, &t.r2]
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(children_of(&page, &t.root), [t.r1.as_str(), t.r2.as_str()]);
}

#[tokio::test]
async fn a_limit_pages_the_walk_and_the_token_resumes_it() {
    let registry = registry();
    let t = tree(&registry).await;
    let mut first = request(&t.root, &alice());
    first.limit = 4;
    let page = walk(&registry, &first).await.unwrap();
    assert_eq!(
        ids(&page),
        [&t.root, &t.r1, &t.ss1, &t.ss2]
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
    );
    let token = page.next_batch.clone().expect("more to come");
    assert_eq!(token.len(), 48, "24 random bytes as hex");

    let mut second = request(&t.root, &alice());
    second.from = Some(token.clone());
    let page = walk(&registry, &second).await.unwrap();
    assert_eq!(
        ids(&page),
        [&t.r3, &t.r4, &t.r2]
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
    );
    assert!(page.next_batch.is_none());

    // The token is still good for the same page again (a client retrying).
    let page = walk(&registry, &second).await.unwrap();
    assert_eq!(page.rooms.len(), 3);

    // Not for a different requester, root, suggested_only or max_depth, nor when made up.
    let mut other_user = second.clone();
    other_user.requester = bob();
    assert!(matches!(
        walk(&registry, &other_user).await,
        Err(RoomError::InvalidParam(_))
    ));
    let mut other_root = second.clone();
    other_root.root = t.ss1.clone();
    assert!(matches!(
        walk(&registry, &other_root).await,
        Err(RoomError::InvalidParam(_))
    ));
    let mut other_filter = second.clone();
    other_filter.suggested_only = true;
    assert!(matches!(
        walk(&registry, &other_filter).await,
        Err(RoomError::InvalidParam(_))
    ));
    let mut other_depth = second.clone();
    other_depth.max_depth = Some(9);
    assert!(matches!(
        walk(&registry, &other_depth).await,
        Err(RoomError::InvalidParam(_))
    ));
    let mut made_up = second.clone();
    made_up.from = Some("0".repeat(48));
    assert!(matches!(
        walk(&registry, &made_up).await,
        Err(RoomError::InvalidParam(_))
    ));
}

#[tokio::test]
async fn a_limit_above_the_maximum_is_clamped() {
    let registry = registry();
    let t = tree(&registry).await;
    let mut big = request(&t.root, &alice());
    big.limit = 10_000;
    let page = walk(&registry, &big).await.unwrap();
    assert_eq!(page.rooms.len(), 7);
    assert_eq!(page.stats.remote_fetches, 0);
}

#[test]
fn expired_tokens_are_gone_and_the_store_is_bounded() {
    let sessions = PaginationSessions::default();
    let session = |age: Duration| Session {
        requester: alice().to_string(),
        root: room_id!("!space:hs1").to_owned(),
        suggested_only: false,
        max_depth: None,
        queue: Vec::new(),
        processed: HashSet::new(),
        created: Instant::now() - age,
    };
    let stale = sessions.store(session(TOKEN_VALIDITY + Duration::from_secs(1)));
    assert!(sessions.get(&stale).is_none(), "expired on read");
    let fresh = sessions.store(session(Duration::ZERO));
    assert!(sessions.get(&fresh).is_some());
    assert_eq!(sessions.len(), 1, "the store swept the expired one");
    for _ in 0..MAX_SESSIONS {
        sessions.store(session(Duration::ZERO));
    }
    assert_eq!(sessions.len(), MAX_SESSIONS);
    assert!(
        sessions.get(&fresh).is_none(),
        "the oldest went when the store was full"
    );
}

#[tokio::test]
async fn a_room_linked_twice_is_visited_once() {
    let registry = registry();
    let root = create(&registry, RoomSpec::public("Root").space(), 1_000).await;
    let sub = create(&registry, RoomSpec::public("Sub").space(), 1_000).await;
    let leaf = create(&registry, RoomSpec::public("Leaf"), 1_000).await;
    let via = json!({ "via": [SERVER] });
    link(&registry, &root, &sub, via.clone(), 2_001).await;
    link(&registry, &root, &leaf, via.clone(), 2_002).await;
    link(&registry, &sub, &leaf, via.clone(), 2_003).await;
    // A cycle back to the root, too.
    link(&registry, &sub, &root, via, 2_004).await;
    let page = walk(&registry, &request(&root, &alice())).await.unwrap();
    assert_eq!(
        ids(&page),
        [root.to_string(), sub.to_string(), leaf.to_string()]
    );
}

// ------------------------------------------------------------------------------------------
// Visibility in the walk
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn rooms_the_requester_may_not_see_are_left_out_with_their_subtrees() {
    // Complement's `TestClientSpacesSummaryJoinRules`: everything invite-only but R2.
    let registry = registry();
    let root = create(&registry, RoomSpec::public("Root").space(), 1_000).await;
    let r1 = create(&registry, RoomSpec::private("R1"), 1_000).await;
    let ss1 = create(&registry, RoomSpec::private("Sub-Space 1").space(), 1_000).await;
    let r2 = create(&registry, RoomSpec::public("R2").world_readable(), 1_000).await;
    let r3 = create(&registry, RoomSpec::private("R3"), 1_000).await;
    let via = json!({ "via": [SERVER] });
    link(&registry, &root, &r1, via.clone(), 2_001).await;
    link(&registry, &root, &ss1, via.clone(), 2_002).await;
    link(&registry, &ss1, &r2, via.clone(), 2_003).await;
    link(&registry, &ss1, &r3, via, 2_004).await;
    join(&registry, &root, &bob(), 3_000).await;

    let page = walk(&registry, &request(&root, &bob())).await.unwrap();
    assert_eq!(ids(&page), [root.to_string()]);
    assert_eq!(children_of(&page, &root), [r1.as_str(), ss1.as_str()]);
    assert_eq!(page.stats.hidden, 2);

    invite(&registry, &r1, &bob(), 4_000).await;
    invite(&registry, &r3, &bob(), 4_000).await;
    let page = walk(&registry, &request(&root, &bob())).await.unwrap();
    assert_eq!(ids(&page), [root.to_string(), r1.to_string()]);

    invite(&registry, &ss1, &bob(), 5_000).await;
    let page = walk(&registry, &request(&root, &bob())).await.unwrap();
    assert_eq!(
        ids(&page),
        [
            root.to_string(),
            r1.to_string(),
            ss1.to_string(),
            r2.to_string(),
            r3.to_string()
        ]
    );
}

#[tokio::test]
async fn a_restricted_room_shows_once_the_requester_is_in_the_room_it_allows() {
    // Complement's `TestRestrictedRoomsSpacesSummaryLocal`.
    let registry = registry();
    let space = create(
        &registry,
        RoomSpec::public("Space").space().world_readable(),
        1_000,
    )
    .await;
    let room = create(
        &registry,
        RoomSpec::public("Room").restricted_to(&space),
        1_000,
    )
    .await;
    link(&registry, &space, &room, json!({ "via": [SERVER] }), 2_000).await;

    let page = walk(&registry, &request(&space, &bob())).await.unwrap();
    assert_eq!(ids(&page), [space.to_string()]);

    join(&registry, &space, &bob(), 3_000).await;
    let page = walk(&registry, &request(&space, &bob())).await.unwrap();
    assert_eq!(ids(&page), [space.to_string(), room.to_string()]);
    let room_json = &page.rooms[1];
    assert_eq!(room_json["join_rule"], "restricted");
    assert_eq!(room_json["allowed_room_ids"], json!([space]));
}

#[tokio::test]
async fn a_root_the_requester_may_not_see_is_forbidden_and_an_unknown_one_too() {
    let registry = registry();
    let private = create(&registry, RoomSpec::private("Private").space(), 1_000).await;
    assert!(matches!(
        walk(&registry, &request(&private, &bob())).await,
        Err(RoomError::Forbidden(_))
    ));
    assert!(matches!(
        walk(&registry, &request(room_id!("!nowhere:hs2"), &bob())).await,
        Err(RoomError::Forbidden(_))
    ));
    // The creator sees it.
    let page = walk(&registry, &request(&private, &alice())).await.unwrap();
    assert_eq!(ids(&page), [private.to_string()]);
}

// ------------------------------------------------------------------------------------------
// Federation
// ------------------------------------------------------------------------------------------

/// Another server, as the walk sees it: an answer per room, or a failure.
#[derive(Default)]
struct FakeRemote {
    answers: HashMap<(String, OwnedRoomId), Result<Value, String>>,
    asked: Mutex<Vec<(String, OwnedRoomId, bool)>>,
}

impl FakeRemote {
    fn answers(mut self, destination: &str, room: &RoomId, body: Value) -> Self {
        self.answers
            .insert((destination.to_owned(), room.to_owned()), Ok(body));
        self
    }
    fn fails(mut self, destination: &str, room: &RoomId) -> Self {
        self.answers.insert(
            (destination.to_owned(), room.to_owned()),
            Err("connection refused".to_owned()),
        );
        self
    }
    fn asked(&self) -> Vec<(String, OwnedRoomId, bool)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl RemoteHierarchy for FakeRemote {
    async fn fetch(
        &self,
        destination: &str,
        room_id: &RoomId,
        suggested_only: bool,
    ) -> Result<RemoteHierarchyPage, RoomError> {
        self.asked.lock().unwrap().push((
            destination.to_owned(),
            room_id.to_owned(),
            suggested_only,
        ));
        match self
            .answers
            .get(&(destination.to_owned(), room_id.to_owned()))
        {
            Some(Ok(body)) => RemoteHierarchyPage::from_json(body),
            Some(Err(error)) => Err(RoomError::BackfillFailed(error.clone())),
            None => Err(RoomError::BackfillFailed(format!(
                "{destination} does not know {room_id}"
            ))),
        }
    }
}

fn remote_summary(room: &RoomId, name: &str, space: bool, children: &[(&RoomId, &str)]) -> Value {
    let mut summary = json!({
        "room_id": room,
        "name": name,
        "join_rule": "public",
        "num_joined_members": 1,
        "world_readable": true,
        "guest_can_join": false,
        "children_state": children.iter().map(|(child, via)| json!({
            "type": "m.space.child",
            "state_key": child,
            "sender": "@bob:hs2",
            "content": { "via": [via] },
            "origin_server_ts": 5_000,
        })).collect::<Vec<_>>(),
    });
    if space {
        summary["room_type"] = json!("m.space");
    }
    summary
}

#[tokio::test]
async fn a_child_this_server_does_not_hold_is_asked_of_its_via_servers() {
    // Complement's `TestFederatedClientSpaces`: Root -> R1, SS1, r2; SS1 -> ss2, r3; ss2 -> R4,
    // with lower-case rooms on hs2.
    let registry = registry();
    let root = create(
        &registry,
        RoomSpec::public("Root").space().world_readable(),
        1_000,
    )
    .await;
    let r1 = create(&registry, RoomSpec::public("R1").world_readable(), 1_000).await;
    let ss1 = create(
        &registry,
        RoomSpec::public("SS1").space().world_readable(),
        1_000,
    )
    .await;
    let r4 = create(&registry, RoomSpec::public("R4").world_readable(), 1_000).await;
    let r2 = room_id!("!r2:hs2");
    let ss2 = room_id!("!ss2:hs2");
    let r3 = room_id!("!r3:hs2");
    link(&registry, &root, &r1, json!({ "via": [SERVER] }), 2_001).await;
    link(&registry, &root, &ss1, json!({ "via": [SERVER] }), 2_002).await;
    link(&registry, &root, r2, json!({ "via": ["hs2"] }), 2_003).await;
    link(&registry, &ss1, ss2, json!({ "via": ["hs2"] }), 2_004).await;
    link(&registry, &ss1, r3, json!({ "via": ["hs2"] }), 2_005).await;

    let remote = FakeRemote::default()
        .answers(
            "hs2",
            r2,
            json!({ "room": remote_summary(r2, "r2", false, &[]), "children": [], "inaccessible_children": [] }),
        )
        .answers(
            "hs2",
            ss2,
            json!({
                "room": remote_summary(ss2, "ss2", true, &[(&r4, SERVER)]),
                "children": [remote_summary(&r4, "R4 as hs2 sees it", false, &[])],
                "inaccessible_children": [],
            }),
        )
        .answers(
            "hs2",
            r3,
            json!({ "room": remote_summary(r3, "r3", false, &[]), "children": [], "inaccessible_children": [] }),
        );
    let remote = Arc::new(remote);
    registry.install_remote_hierarchy(remote.clone());

    let page = walk(&registry, &request(&root, &alice())).await.unwrap();
    assert_eq!(
        ids(&page),
        [
            root.to_string(),
            r1.to_string(),
            ss1.to_string(),
            ss2.to_string(),
            r4.to_string(),
            r3.to_string(),
            r2.to_string(),
        ]
    );
    // R4 is held here, so its own state was used, not hs2's description of it.
    let r4_json = page
        .rooms
        .iter()
        .find(|r| r["room_id"] == r4.as_str())
        .unwrap();
    assert_eq!(r4_json["name"], "R4");
    // Every remote room carries `children_state`, even the leaves.
    for room in &page.rooms {
        assert!(room["children_state"].is_array(), "{room}");
    }
    assert_eq!(page.stats.remote_fetches, 3);
    assert_eq!(page.stats.remote_failures, 0);
    assert_eq!(
        remote.asked(),
        [
            ("hs2".to_owned(), ss2.to_owned(), false),
            ("hs2".to_owned(), r3.to_owned(), false),
            ("hs2".to_owned(), r2.to_owned(), false),
        ]
    );
}

#[tokio::test]
async fn a_leaf_described_by_its_parents_server_is_not_asked_for_again() {
    let registry = registry();
    let root = create(&registry, RoomSpec::public("Root").space(), 1_000).await;
    let sub = room_id!("!sub:hs2");
    let leaf = room_id!("!leaf:hs2");
    let nested = room_id!("!nested:hs2");
    link(&registry, &root, sub, json!({ "via": ["hs2"] }), 2_000).await;
    let remote = Arc::new(
        FakeRemote::default()
            .answers(
                "hs2",
                sub,
                json!({
                    "room": remote_summary(sub, "sub", true, &[(leaf, "hs2"), (nested, "hs2")]),
                    "children": [
                        remote_summary(leaf, "leaf", false, &[]),
                        remote_summary(nested, "nested", true, &[]),
                    ],
                    "inaccessible_children": [],
                }),
            )
            .answers(
                "hs2",
                nested,
                json!({ "room": remote_summary(nested, "nested", true, &[]), "children": [], "inaccessible_children": [] }),
            ),
    );
    registry.install_remote_hierarchy(remote.clone());
    let page = walk(&registry, &request(&root, &alice())).await.unwrap();
    assert_eq!(
        ids(&page),
        [
            root.to_string(),
            sub.to_string(),
            leaf.to_string(),
            nested.to_string()
        ]
    );
    // The leaf came from sub's answer; the nested space had to be asked for its children.
    let asked: Vec<OwnedRoomId> = remote.asked().into_iter().map(|(_, r, _)| r).collect();
    assert_eq!(asked, [sub.to_owned(), nested.to_owned()]);

    // At the depth limit, even a space is taken from its parent's answer.
    let mut shallow = request(&root, &alice());
    shallow.max_depth = Some(2);
    let before = remote.asked().len();
    let page = walk(&registry, &shallow).await.unwrap();
    assert_eq!(page.rooms.len(), 4);
    assert_eq!(remote.asked().len(), before + 1, "only sub was asked");
}

#[tokio::test]
async fn a_failing_server_is_skipped_and_inaccessible_children_are_not_asked_about() {
    let registry = registry();
    let root = create(&registry, RoomSpec::public("Root").space(), 1_000).await;
    let sub = room_id!("!sub:hs2");
    let secret = room_id!("!secret:hs2");
    let gone = room_id!("!gone:hs4");
    link(
        &registry,
        &root,
        sub,
        json!({ "via": ["hs2", "hs3", "hs1", "hs5"] }),
        2_000,
    )
    .await;
    link(&registry, &root, gone, json!({ "via": ["hs4"] }), 2_001).await;
    let remote = Arc::new(
        FakeRemote::default()
            .fails("hs2", sub)
            .answers(
                "hs3",
                sub,
                json!({
                    "room": remote_summary(sub, "sub", true, &[(secret, "hs2")]),
                    "children": [],
                    "inaccessible_children": [secret],
                }),
            )
            .fails("hs4", gone),
    );
    registry.install_remote_hierarchy(remote.clone());
    let page = walk(&registry, &request(&root, &alice())).await.unwrap();
    assert_eq!(ids(&page), [root.to_string(), sub.to_string()]);
    assert_eq!(
        page.stats.remote_fetches, 3,
        "hs2 failed, hs3 answered, hs4 failed"
    );
    assert_eq!(page.stats.remote_failures, 2);
    assert_eq!(page.stats.remote_skipped, 1, "the secret child");
    let asked = remote.asked();
    assert!(!asked.iter().any(|(_, r, _)| r == secret));
    assert!(
        !asked.iter().any(|(d, _, _)| d == "hs1"),
        "this server is never asked about itself"
    );
    assert!(page.next_batch.is_none());
}

#[tokio::test]
async fn a_remote_restricted_room_is_judged_by_the_requesters_memberships() {
    // Complement's `TestRestrictedRoomsSpacesSummaryFederation`, from hs1's side once hs2 has
    // answered: the room allows the space, alice is in the space and bob is not.
    let registry = registry();
    let space = create(
        &registry,
        RoomSpec::public("Space").space().world_readable(),
        1_000,
    )
    .await;
    let room = room_id!("!room:hs2");
    link(&registry, &space, room, json!({ "via": ["hs2"] }), 2_000).await;
    let remote = Arc::new(FakeRemote::default().answers(
        "hs2",
        room,
        json!({
            "room": {
                "room_id": room,
                "name": "Room",
                "join_rule": "restricted",
                "allowed_room_ids": [space],
                "num_joined_members": 1,
                "world_readable": false,
                "guest_can_join": false,
                "children_state": [],
            },
            "children": [],
            "inaccessible_children": [],
        }),
    ));
    registry.install_remote_hierarchy(remote);
    let page = walk(&registry, &request(&space, &alice())).await.unwrap();
    assert_eq!(ids(&page), [space.to_string(), room.to_string()]);
    let page = walk(&registry, &request(&space, &bob())).await.unwrap();
    assert_eq!(ids(&page), [space.to_string()]);
    assert_eq!(page.stats.hidden, 1);
}

#[tokio::test]
async fn without_the_hook_a_remote_child_is_left_out() {
    let registry = registry();
    let root = create(&registry, RoomSpec::public("Root").space(), 1_000).await;
    link(
        &registry,
        &root,
        room_id!("!elsewhere:hs2"),
        json!({ "via": ["hs2"] }),
        2_000,
    )
    .await;
    let page = walk(&registry, &request(&root, &alice())).await.unwrap();
    assert_eq!(ids(&page), [root.to_string()]);
    assert_eq!(page.stats.remote_skipped, 1);
    assert_eq!(children_of(&page, &root), ["!elsewhere:hs2"]);
}

#[test]
fn a_remote_page_is_read_leniently() {
    let page = RemoteHierarchyPage::from_json(&json!({
        "room": { "room_id": "!a:hs2" },
        "children": [{ "room_id": "!b:hs2" }, { "name": "no id" }, "junk"],
        "inaccessible_children": ["!c:hs2", 7],
    }))
    .unwrap();
    assert_eq!(page.children.len(), 1);
    assert_eq!(page.inaccessible_children, ["!c:hs2"]);
    assert!(RemoteHierarchyPage::from_json(&json!({ "children": [] })).is_err());
    assert!(RemoteHierarchyPage::from_json(&json!({ "room": "not an object" })).is_err());
}

#[test]
fn an_owned_user_id_is_what_the_request_carries() {
    // Guards the type: the session compares the requester by string, so the request must
    // carry a full user ID rather than a localpart.
    let request = request(room_id!("!space:hs1"), &alice());
    let _: OwnedUserId = request.requester;
}
