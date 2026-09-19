//! The end-to-end scenario: two `matrix-sdk` clients (Alice and Bob) doing the everyday things a
//! real Matrix client does, against a real `hs serve` process reached over real HTTP.
//!
//! Every step asserts the actual response and, on failure, reports which HTTP call failed and
//! the server's actual body (via `anyhow::Context`, and `matrix-sdk`'s own `Error` Display, which
//! includes the parsed Matrix error or raw body for a failed request) rather than a bare
//! "assertion failed".

use anyhow::{Context, Result, bail};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoomRequest;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::{Client, RoomMemberships};

/// Registers a user via `m.login.dummy` UIA (the flow `hs-auth` accepts today, per
/// `crates/hs-cli/tests/e2e.rs`) and returns the logged-in client.
async fn register(base_url: &str, username: &str, password: &str) -> Result<Client> {
    let client = Client::builder()
        .homeserver_url(base_url)
        .build()
        .await
        .with_context(|| format!("building a matrix-sdk Client for {base_url}"))?;

    let mut request = RegisterRequest::new();
    request.username = Some(username.to_owned());
    request.password = Some(password.to_owned());
    request.initial_device_display_name = Some("hs-loadgen".to_owned());
    request.auth = Some(uiaa::AuthData::Dummy(uiaa::Dummy::new()));

    client
        .matrix_auth()
        .register(request)
        .await
        .with_context(|| format!("POST /register for {username} should succeed"))?;

    Ok(client)
}

/// Runs the full scenario against a server at `base_url`. Returns a human-readable log of every
/// step that succeeded, so a caller can print it even on success (the whole point of this crate
/// is to prove what actually happened, not just report a boolean).
pub async fn run(base_url: &str) -> Result<Vec<String>> {
    let mut log = Vec::new();
    macro_rules! step {
        ($($arg:tt)*) => {{
            let msg = format!($($arg)*);
            tracing::info!("{msg}");
            log.push(msg);
        }};
    }

    // 1. Register two users.
    let alice = register(base_url, "loadgen-alice", "correct horse battery staple")
        .await
        .context("registering alice")?;
    let alice_id = alice
        .user_id()
        .context("alice should have a user_id after registering")?
        .to_owned();
    step!("registered {alice_id}");

    let bob = register(base_url, "loadgen-bob", "another good passphrase")
        .await
        .context("registering bob")?;
    let bob_id = bob
        .user_id()
        .context("bob should have a user_id after registering")?
        .to_owned();
    step!("registered {bob_id}");

    // 2. Log in as alice again, on a second device, exercising POST /login explicitly (register
    //    already logs a session in, so this is the only way to hit /login for real).
    let alice_relogin = Client::builder()
        .homeserver_url(base_url)
        .build()
        .await
        .context("building alice's second matrix-sdk Client")?;
    let login_response = alice_relogin
        .matrix_auth()
        .login_username(alice_id.localpart(), "correct horse battery staple")
        .initial_device_display_name("hs-loadgen second device")
        .await
        .context("POST /login for alice's second device should succeed")?;
    if login_response.user_id != alice_id {
        bail!(
            "login returned user_id {} but registration used {alice_id}",
            login_response.user_id
        );
    }
    step!("logged in {alice_id} on a second device via POST /login");

    // 3. Create a room.
    let mut create_room_request = CreateRoomRequest::new();
    create_room_request.name = Some("hs-loadgen room".to_owned());
    let room = alice
        .create_room(create_room_request)
        .await
        .context("POST /createRoom should succeed")?;
    let room_id: OwnedRoomId = room.room_id().to_owned();
    step!("alice created room {room_id}");

    // 4. Invite the second user.
    room.invite_user_by_id(&bob_id)
        .await
        .with_context(|| format!("inviting {bob_id} into {room_id} should succeed"))?;
    step!("alice invited {bob_id}");

    // 5. Bob joins.
    let bob_room = bob
        .join_room_by_id(&room_id)
        .await
        .with_context(|| format!("{bob_id} joining {room_id} should succeed"))?;
    step!("{bob_id} joined {room_id}");

    // Baseline incremental-sync tokens for both clients, taken right after the join so the
    // message exchange below is observed incrementally (`since=...`), not via a first full sync.
    // `limited`/`prev_batch`/state-at-timeline-start semantics only show up on the incremental
    // path — a full initial sync would hide exactly the bugs this scenario exists to find.
    let alice_baseline = alice
        .sync_once(SyncSettings::default())
        .await
        .context("alice's baseline /sync (establishing a `since` token) should succeed")?;
    // Kept for step 11's forward-pagination proof, below: `alice_baseline.next_batch` itself gets
    // moved into a `SyncSettings::token(...)` call a few lines down, so this needs to be cloned
    // out now while it's still whole.
    let alice_baseline_token = alice_baseline.next_batch.clone();
    let bob_baseline = bob
        .sync_once(SyncSettings::default())
        .await
        .context("bob's baseline /sync (establishing a `since` token) should succeed")?;
    step!("both clients completed a baseline /sync");

    // 6. Send messages both ways.
    let alice_message = "hello bob, this is alice";
    let alice_send = room
        .send(RoomMessageEventContent::text_plain(alice_message))
        .await
        .context("alice sending a message should succeed")?;
    step!(
        "alice sent {} ({alice_message:?})",
        alice_send.response.event_id
    );

    let bob_message = "hi alice, bob here";
    let bob_send = bob_room
        .send(RoomMessageEventContent::text_plain(bob_message))
        .await
        .context("bob sending a message should succeed")?;
    step!("bob sent {} ({bob_message:?})", bob_send.response.event_id);

    // 7. Sync both clients incrementally and assert each sees the other's message.
    let bob_sync = bob
        .sync_once(SyncSettings::default().token(bob_baseline.next_batch))
        .await
        .context("bob's incremental /sync (since his baseline token) should succeed")?;
    assert_room_contains_body(&bob_sync, &room_id, alice_message)
        .context("bob's incremental /sync should contain alice's message")?;
    step!("bob's incremental /sync saw alice's message");

    let alice_sync = alice
        .sync_once(SyncSettings::default().token(alice_baseline.next_batch))
        .await
        .context("alice's incremental /sync (since her baseline token) should succeed")?;
    assert_room_contains_body(&alice_sync, &room_id, bob_message)
        .context("alice's incremental /sync should contain bob's message")?;
    step!("alice's incremental /sync saw bob's message");

    // 8. Set and read a display name.
    //
    // Known gap (see docs/status/05-sync.md "Bugs found in other tracks"): no crate mounts
    // `PUT`/`GET /_matrix/client/v3/profile/{userId}/displayname` anywhere in this workspace, so
    // this always 404s today. Reported as a soft failure rather than aborting the rest of the
    // scenario, per this crate's brief ("work around it in your script so the rest of the
    // scenario still runs").
    match alice
        .account()
        .set_display_name(Some("Alice Loadgen"))
        .await
    {
        Ok(()) => {
            let read_back_name = alice
                .account()
                .get_display_name()
                .await
                .context("reading alice's display name back should succeed")?;
            if read_back_name.as_deref() != Some("Alice Loadgen") {
                bail!(
                    "display name round-trip failed: set \"Alice Loadgen\", read back {read_back_name:?}"
                );
            }
            step!("alice's display name round-tripped through GET/PUT /profile");
        }
        Err(e) => {
            step!(
                "KNOWN BUG (not this track's crates): PUT /profile/{{userId}}/displayname failed: {e}"
            );
        }
    }

    // 9. Set and read room topic/name.
    room.set_name("hs-loadgen room (renamed)".to_owned())
        .await
        .context("renaming the room should succeed")?;
    room.set_room_topic("a topic set by hs-loadgen")
        .await
        .context("setting the room topic should succeed")?;
    // Read back through a fresh sync rather than the SDK's local cache, so this actually
    // exercises the server's `/sync` state section, not just what `set_name`/`set_room_topic`
    // already told the client locally.
    let alice_sync_2 = alice
        .sync_once(SyncSettings::default().token(alice_sync.next_batch))
        .await
        .context("alice's /sync after renaming the room should succeed")?;
    let synced_room = alice_sync_2
        .rooms
        .joined
        .get(&room_id)
        .context("the renamed room should still appear in alice's joined rooms")?;
    let has_name_event = synced_room.timeline.events.iter().any(|event| {
        event_type_and_body(event)
            .map(|(ty, body)| ty == "m.room.name" && body.contains("renamed"))
            .unwrap_or(false)
    });
    let has_topic_event = synced_room.timeline.events.iter().any(|event| {
        event_type_and_body(event)
            .map(|(ty, body)| ty == "m.room.topic" && body.contains("a topic set by hs-loadgen"))
            .unwrap_or(false)
    });
    if !has_name_event || !has_topic_event {
        bail!(
            "expected m.room.name and m.room.topic state events in the post-rename sync; \
             found name={has_name_event} topic={has_topic_event}"
        );
    }
    step!("room name and topic changes appeared in /sync's timeline");

    // 10. Read a room's members.
    let members = room
        .members(RoomMemberships::empty())
        .await
        .context("GET /members should succeed")?;
    let member_ids: Vec<_> = members.iter().map(|m| m.user_id().to_owned()).collect();
    if !member_ids.contains(&alice_id) || !member_ids.contains(&bob_id) {
        bail!(
            "expected both {alice_id} and {bob_id} in the member list, got {:?}",
            member_ids
        );
    }
    step!("room membership lists both {alice_id} and {bob_id}");

    // 11. Paginate /messages -- first with no `from` at all (the live end), then, the real point
    // of this step, with the token `/sync` handed back. This is the exact sequence every real
    // Matrix client performs (sync, then scroll back from the token that sync gave it), and
    // exactly what this session's cross-track fix (`hs-user`/`hs-room`) exists for: before it,
    // `hs-room`'s `/messages` only understood its own room-local pagination token and rejected
    // `hs-user`'s `/sync` token (`hsu1_...`) outright with `400 M_INVALID_PARAM`. Complement's own
    // `room_messages_test.go` (`TestSendAndFetchMessage` and siblings) does the same thing with a
    // bare `next_batch`.
    let page = room
        .messages(MessagesOptions::backward())
        .await
        .context("GET /messages should succeed")?;
    let bodies: Vec<String> = page
        .chunk
        .iter()
        .filter_map(|event| event_type_and_body(event).map(|(_, body)| body))
        .collect();
    if !bodies.iter().any(|b| b.contains(alice_message)) {
        bail!("expected alice's message in a backward /messages page, got bodies: {bodies:?}");
    }
    step!(
        "backward /messages page (no `from`, the live end) contains alice's message ({} events)",
        page.chunk.len()
    );

    // dir=b from `alice_sync_2.next_batch` (a `/sync` token, minted well after alice's original
    // message -- the rename/topic sync from step 9): must page backward far enough to find it.
    let sync_token_page = room
        .messages(MessagesOptions::backward().from(alice_sync_2.next_batch.as_str()))
        .await
        .context("GET /messages?dir=b with a token minted by /sync should succeed")?;
    let sync_token_bodies: Vec<String> = sync_token_page
        .chunk
        .iter()
        .filter_map(|event| event_type_and_body(event).map(|(_, body)| body))
        .collect();
    if !sync_token_bodies.iter().any(|b| b.contains(alice_message)) {
        bail!(
            "expected alice's message in a backward /messages page paginated from a /sync token, \
             got bodies: {sync_token_bodies:?}"
        );
    }
    step!(
        "backward /messages page, paginated from a token /sync handed back (not /messages \
         itself), contains alice's message ({} events)",
        sync_token_page.chunk.len()
    );

    // dir=f from `alice_baseline_token` (a `/sync` token minted *before* alice's message was ever
    // sent): must page forward far enough to find it -- the same shape as
    // `TestSendAndFetchMessage`'s `dir=f&from=<pre-message next_batch>`.
    let forward_page = room
        .messages(MessagesOptions::forward().from(alice_baseline_token.as_str()))
        .await
        .context("GET /messages?dir=f with a token minted by /sync should succeed")?;
    let forward_bodies: Vec<String> = forward_page
        .chunk
        .iter()
        .filter_map(|event| event_type_and_body(event).map(|(_, body)| body))
        .collect();
    if !forward_bodies.iter().any(|b| b.contains(alice_message)) {
        bail!(
            "expected alice's message in a forward /messages page paginated from a pre-message \
             /sync token, got bodies: {forward_bodies:?}"
        );
    }
    step!(
        "forward /messages page, paginated from a /sync token issued before any messages, \
         contains alice's message ({} events)",
        forward_page.chunk.len()
    );

    // 12. Log out.
    alice
        .matrix_auth()
        .logout()
        .await
        .context("alice logging out should succeed")?;
    bob.matrix_auth()
        .logout()
        .await
        .context("bob logging out should succeed")?;
    step!("both clients logged out");

    // Confirm logout actually invalidated the token: a call after logout should fail, not
    // silently succeed against a still-valid session.
    match alice.sync_once(SyncSettings::default()).await {
        Ok(_) => bail!("alice's access token should be rejected after logout, but /sync succeeded"),
        Err(e) => step!("post-logout /sync was correctly rejected: {e}"),
    }

    Ok(log)
}

/// Extracts `(type, content-as-debug-string)` from a raw timeline event, for the body/type
/// assertions above. Returns `None` for an event this scenario doesn't need to inspect closely
/// (never treated as a hard failure by itself; callers report a clear error listing what *was*
/// found instead).
fn event_type_and_body(
    event: &matrix_sdk::deserialized_responses::TimelineEvent,
) -> Option<(String, String)> {
    let raw = event.raw();
    let value: serde_json::Value = serde_json::from_str(raw.json().get()).ok()?;
    let event_type = value.get("type")?.as_str()?.to_owned();
    let body = value
        .get("content")
        .map(|c| c.to_string())
        .unwrap_or_default();
    Some((event_type, body))
}

fn assert_room_contains_body(
    sync: &matrix_sdk::sync::SyncResponse,
    room_id: &OwnedRoomId,
    expected_substring: &str,
) -> Result<()> {
    let Some(joined) = sync.rooms.joined.get(room_id) else {
        bail!(
            "expected room {room_id} in this sync's joined rooms, but it was absent; \
             joined rooms present: {:?}",
            sync.rooms.joined.keys().collect::<Vec<_>>()
        );
    };
    let bodies: Vec<String> = joined
        .timeline
        .events
        .iter()
        .filter_map(|event| event_type_and_body(event).map(|(_, body)| body))
        .collect();
    if bodies.iter().any(|b| b.contains(expected_substring)) {
        Ok(())
    } else {
        bail!(
            "expected a timeline event containing {expected_substring:?} in room {room_id}, \
             got bodies: {bodies:?} (limited={}, prev_batch={:?})",
            joined.timeline.limited,
            joined.timeline.prev_batch
        )
    }
}
