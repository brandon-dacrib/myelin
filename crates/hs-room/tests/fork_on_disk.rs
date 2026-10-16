//! A tool, not a test: forks a room in a stopped `hs serve`'s embedded store, so the extremity
//! maintainer can be watched merging it on the real binary (`docs/status/04-room-and-events.md`,
//! 2026-10-16, `agent/dummy-event-chain`). A local server cannot fork its own rooms -- every
//! event it builds cites the room's extremities -- and a real fork needs other servers; this
//! writes `HS_FORK_COUNT` messages from the room's first joined local member, each citing the
//! room's head as it was, exactly as federation leaves a room many servers answered at once.
//!
//! ```sh
//! HS_FORK_DATA_DIR=<storage.data_dir> HS_FORK_ROOM_ID='!room:server' \
//!   HS_FORK_SERVER_NAME=server HS_FORK_COUNT=120 \
//!   cargo test -p hs-room --test fork_on_disk -- --ignored --nocapture
//! ```
//!
//! The events are signed with a throwaway key: fine for a server that does not federate them.

use hs_kv::fjall_backend::FjallBackend;
use hs_room::actor::RoomActor;
use hs_room::identity::HomeserverIdentity;
use hs_room::persist::Tables;
use ruma::OwnedRoomId;

#[test]
#[ignore = "a tool for real-binary verification: needs HS_FORK_* and a stopped server's store"]
fn fork_a_room_in_a_stopped_servers_store() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let data_dir = var("HS_FORK_DATA_DIR");
    let room_id: OwnedRoomId = var("HS_FORK_ROOM_ID").parse().expect("a room ID");
    let server_name = var("HS_FORK_SERVER_NAME");
    let count: i64 = var("HS_FORK_COUNT").parse().expect("a count");

    let backend = FjallBackend::open(&data_dir).expect("open the server's store (is it stopped?)");
    let tables = Tables::open(&backend).expect("tables");
    let identity = HomeserverIdentity::for_tests(&server_name);
    let mut actor = RoomActor::load(backend, tables, identity, &room_id)
        .expect("load")
        .expect("the room exists");
    let mut members: Vec<_> = actor
        .membership_list()
        .expect("members")
        .iter()
        .filter(|(user, membership)| {
            membership == "join" && user.server_name().as_str() == server_name
        })
        .map(|(user, _)| user.clone())
        .collect();
    members.sort();
    let sender = members.first().expect("a joined local member").clone();
    let head: Vec<_> = actor
        .forward_extremity_ids()
        .iter()
        .filter_map(|(id, _)| actor.event_sn_of(id))
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as i64;
    for n in 0..count {
        actor
            .send_event_citing(
                sender.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": format!("fork {n}")}),
                None,
                now + n,
                &head,
            )
            .expect("a fork");
    }
    println!(
        "{room_id}: {} forward extremities, forked by {sender}",
        actor.forward_extremity_count()
    );
}
