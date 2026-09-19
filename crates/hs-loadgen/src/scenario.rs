//! The end-to-end scenario: two `matrix-sdk` clients (Alice and Bob) doing the everyday things a
//! real Matrix client does, against a real `hs serve` process reached over real HTTP.
//!
//! Every step asserts the actual response and, on failure, reports which HTTP call failed and
//! the server's actual body (via `anyhow::Context`, and `matrix-sdk`'s own `Error` Display, which
//! includes the parsed Matrix error or raw body for a failed request) rather than a bare
//! "assertion failed".

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::{MessagesOptions, Receipts};
use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
use matrix_sdk::ruma::api::client::filter::{
    FilterDefinition, RoomEventFilter, RoomFilter, create_filter, get_filter,
};
use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoomRequest;
use matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter as SyncFilterKind;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::events::receipt::ReceiptThread;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::uint;
use matrix_sdk::sync::SyncResponse;
use matrix_sdk::{Client, RoomMemberships};

/// The Complement `MustSyncUntil` pattern in miniature: repeatedly `/sync`s `client` (chaining
/// each response's `next_batch` into the next request's `since`, exactly like a real client's
/// sync loop) until `predicate` accepts a response or `max_wait` elapses. Returns the last
/// response either way -- the caller decides whether running out the clock is a hard failure
/// (typing, invites: this crate's own code, expected to work) or a documented, soft-failed gap
/// (profile-into-membership propagation: diagnosed as another track's bug, not this one's --
/// see `docs/status/05-sync.md`).
///
/// This is the actual point of this session's extension to this scenario: unit tests inside
/// `hs-user` proved the *pieces* (the typing registry, the wake hook, the token cursor) work in
/// isolation, but only a real client doing exactly what Complement's `MustSyncUntil` does --
/// bounded polling of the real long-poll endpoint over a real socket -- can prove the *wake*
/// actually reaches a second, independent client's blocked `/sync` call.
async fn sync_until(
    client: &Client,
    since: String,
    max_wait: Duration,
    mut predicate: impl FnMut(&SyncResponse) -> bool,
) -> Result<(bool, SyncResponse)> {
    let deadline = Instant::now() + max_wait;
    let mut token = since;
    loop {
        let response = client
            .sync_once(
                SyncSettings::default()
                    .token(token)
                    .timeout(Duration::from_millis(1500)),
            )
            .await
            .context("bounded-wait /sync should succeed")?;
        if predicate(&response) {
            return Ok((true, response));
        }
        if Instant::now() >= deadline {
            return Ok((false, response));
        }
        token = response.next_batch.clone();
    }
}

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

    // `hs-user` (track 05) always carries `m.push_rules` as global account data on every sync
    // once `hs-push`'s (track 10) ruleset store is installed on the session hub
    // (`crate::hub::SessionHub::install_push_rules_store`) -- see `docs/status/05-sync.md`. That
    // install call is `hs-cli`'s to make (`crates/hs-cli/src/serve.rs`'s
    // `build_session_mounts`), not this crate's, so this is a soft check like the profile-
    // propagation one below: it proves the seam end to end once wired, and names the gap rather
    // than failing the whole scenario while it is still open.
    let saw_push_rules = alice_baseline.account_data.iter().any(|raw| {
        json_type_and_body(raw.json().get()).is_some_and(|(ty, _)| ty == "m.push_rules")
    });
    if saw_push_rules {
        step!("alice's baseline /sync carried m.push_rules global account data");
    } else {
        step!(
            "KNOWN BUG (not this track's crates -- see docs/status/05-sync.md): hs-user's \
             m.push_rules support is implemented but hs-cli's build_session_mounts has not yet \
             wired hs-push's ruleset store onto the session hub, so alice's baseline /sync did \
             not carry m.push_rules"
        );
    }

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
    // Kept for the receipts step, below: a stable, already-visible-to-both-clients event id to
    // put a read receipt and a fully-read marker on.
    let alice_message_event_id = alice_send.response.event_id.clone();

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

    // 12. Typing: alice sends a typing notice; bob's *next* `/sync` (bounded wait, the same shape
    // Complement's `MustSyncUntil` uses for `TestTyping`) must see it in `ephemeral.events` for
    // the room. This is this track's own new work this session (`hs_user::routes::typing`,
    // `hs_user::hub::SessionHub::set_typing`) -- a hard failure here is a real regression, not a
    // documented gap, so this step bails like any other.
    room.typing_notice(true)
        .await
        .context("alice's typing notice should succeed")?;
    let (saw_typing, typing_sync) = sync_until(
        &bob,
        bob_sync.next_batch.clone(),
        Duration::from_secs(10),
        |response| {
            response.rooms.joined.get(&room_id).is_some_and(|joined| {
                joined.ephemeral.iter().any(|raw| {
                    json_type_and_body(raw.json().get())
                        .map(|(ty, body)| ty == "m.typing" && body.contains(alice_id.as_str()))
                        .unwrap_or(false)
                })
            })
        },
    )
    .await
    .context("bob's bounded-wait /sync for alice's typing notice should succeed")?;
    if !saw_typing {
        bail!(
            "bob's /sync never saw alice's typing notice within the bounded wait \
             (this crate's own code -- not a documented cross-track gap)"
        );
    }
    step!("bob's /sync saw alice's typing notice within the bounded wait");

    // 13. Invite visible in sync *before* the invitee ever joins -- register a third user who
    // only ever gets invited, and prove their `/sync` shows the room under `rooms.invite` with
    // stripped state, within a bounded wait. Complement's `TestRoomsInvite` shape.
    let carol = register(base_url, "loadgen-carol", "a third good passphrase")
        .await
        .context("registering carol")?;
    let carol_id = carol
        .user_id()
        .context("carol should have a user_id after registering")?
        .to_owned();
    let carol_baseline = carol
        .sync_once(SyncSettings::default())
        .await
        .context("carol's baseline /sync should succeed")?;
    room.invite_user_by_id(&carol_id)
        .await
        .with_context(|| format!("inviting {carol_id} into {room_id} should succeed"))?;
    let (saw_invite, _) = sync_until(
        &carol,
        carol_baseline.next_batch,
        Duration::from_secs(10),
        |response| response.rooms.invited.contains_key(&room_id),
    )
    .await
    .context("carol's bounded-wait /sync for her invite should succeed")?;
    if !saw_invite {
        bail!(
            "carol's /sync never saw her invite to {room_id} within the bounded wait \
             (this crate's own code -- not a documented cross-track gap)"
        );
    }
    step!("carol's /sync saw her invite to {room_id} within the bounded wait");

    // 14. Profile change propagation: alice changes her display name (the `PUT /profile` call
    // itself, step 8 above); does that change ever reach bob's `/sync` as an updated
    // `m.room.member` event for alice in the room they share? Per the spec, a profile change
    // propagates by the server re-stamping the user's `m.room.member` event in every room they
    // are joined to -- Synapse does this; nothing in this workspace does yet (confirmed: neither
    // `hs-auth`'s `PUT /profile/{userId}/displayname` nor `hs-room`'s membership code touches an
    // *existing* membership event on a profile change, only a *new* join/invite/knock reads the
    // profile at all). This is exactly the shared root cause behind Complement's
    // `TestDisplayNameUpdate`/`TestAvatarUrlUpdate` cluster -- diagnosed, not fixed here (out of
    // this track's crates; see `docs/status/05-sync.md` for the full writeup for track 04/07).
    // Soft-failed like step 8's own profile gap, so this scenario stays green while the gap
    // remains open elsewhere.
    match alice
        .account()
        .set_display_name(Some("Alice Renamed"))
        .await
    {
        Ok(()) => {
            let (saw_profile_in_membership, _) = sync_until(
                &bob,
                typing_sync.next_batch,
                Duration::from_secs(5),
                |response| {
                    response.rooms.joined.get(&room_id).is_some_and(|joined| {
                        let (matrix_sdk::sync::State::Before(state_events)
                        | matrix_sdk::sync::State::After(state_events)) = &joined.state;
                        state_events.iter().any(|raw| {
                            json_type_and_body(raw.json().get())
                                .map(|(ty, body)| {
                                    ty == "m.room.member" && body.contains("Alice Renamed")
                                })
                                .unwrap_or(false)
                        }) || joined.timeline.events.iter().any(|event| {
                            event_type_and_body(event)
                                .map(|(ty, body)| {
                                    ty == "m.room.member" && body.contains("Alice Renamed")
                                })
                                .unwrap_or(false)
                        })
                    })
                },
            )
            .await
            .context("bob's bounded-wait /sync after alice's profile change should succeed")?;
            if saw_profile_in_membership {
                step!(
                    "bob's /sync saw alice's profile change reflected in her m.room.member event"
                );
            } else {
                step!(
                    "KNOWN BUG (not this track's crates, see docs/status/05-sync.md): alice's \
                     profile change never reached bob's /sync as an updated m.room.member event \
                     within the bounded wait"
                );
            }
        }
        Err(e) => {
            step!(
                "KNOWN BUG (not this track's crates): PUT /profile/{{userId}}/displayname failed: {e}"
            );
        }
    }

    // 15. Filters: `POST`/`GET /user/{userId}/filter` round-trip a filter definition, then
    // `/sync?filter=<filter_id>` actually *honours* `room.timeline.limit` -- not merely accepts
    // and ignores it, which this track's own brief calls out as its own bug
    // (`hs_user::filter`'s module doc names exactly what this crate applies vs. only parses).
    let mut timeline_filter = RoomEventFilter::default();
    timeline_filter.limit = Some(uint!(1));
    let mut filtered_room_filter = RoomFilter::default();
    filtered_room_filter.timeline = timeline_filter;
    let mut filter_def = FilterDefinition::default();
    filter_def.room = filtered_room_filter;

    let filter_id = alice
        .send(create_filter::v3::Request::new(
            alice_id.clone(),
            filter_def,
        ))
        .await
        .context("POST /user/{userId}/filter should succeed")?
        .filter_id;
    step!("alice uploaded a sync filter capping room.timeline.limit to 1, filter id {filter_id}");

    let round_tripped = alice
        .send(get_filter::v3::Request::new(
            alice_id.clone(),
            filter_id.clone(),
        ))
        .await
        .context("GET /user/{userId}/filter/{filterId} should succeed")?
        .filter;
    if round_tripped.room.timeline.limit != Some(uint!(1)) {
        bail!(
            "GET /user/{{userId}}/filter/{{filterId}} did not round-trip the uploaded \
             room.timeline.limit (this crate's own filter store/parsing -- a real regression)"
        );
    }
    step!("the uploaded filter round-tripped through GET /user/{{userId}}/filter/{{filterId}}");

    // Two more messages since alice's last sync token (`alice_sync_2`, minted in step 9), so
    // there is more than the filter's `limit: 1` new timeline event for a single filtered sync
    // response to actually have to cap.
    room.send(RoomMessageEventContent::text_plain(
        "filter test message one",
    ))
    .await
    .context("alice sending filter test message one should succeed")?;
    room.send(RoomMessageEventContent::text_plain(
        "filter test message two",
    ))
    .await
    .context("alice sending filter test message two should succeed")?;

    let filtered_sync = alice
        .sync_once(
            SyncSettings::default()
                .token(alice_sync_2.next_batch.clone())
                .filter(SyncFilterKind::FilterId(filter_id)),
        )
        .await
        .context("alice's filtered incremental /sync should succeed")?;
    let filtered_room = filtered_sync
        .rooms
        .joined
        .get(&room_id)
        .context("alice's filtered sync should still include the room")?;
    if filtered_room.timeline.events.len() > 1 {
        bail!(
            "room.timeline.limit: 1 was not honoured: got {} timeline events in one filtered \
             sync response",
            filtered_room.timeline.events.len()
        );
    }
    if !filtered_room.timeline.limited {
        bail!(
            "expected `limited: true` once more events exist than the filter's limit allows, \
             but the filtered sync reported limited=false"
        );
    }
    step!(
        "a sync filter's room.timeline.limit was honoured: {} event(s) returned, limited=true",
        filtered_room.timeline.events.len()
    );

    // 16. Read receipts and the fully-read marker. Bob posts a public `m.read` receipt on
    // alice's very first message (`POST /rooms/{roomId}/receipt/m.read/{eventId}`); alice's next
    // bounded-wait `/sync` (the same `MustSyncUntil` shape steps 12/13 already use) must see it
    // as an `m.receipt` ephemeral event naming both bob and that event id. This is this track's
    // own new work this session (`hs_user::routes::receipts`,
    // `hs_user::hub::SessionHub::set_receipt`) -- a hard failure here is a real regression, not a
    // documented cross-track gap.
    bob_room
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            alice_message_event_id.clone(),
        )
        .await
        .context("bob sending a public read receipt should succeed")?;
    let (saw_receipt, _) = sync_until(
        &alice,
        filtered_sync.next_batch.clone(),
        Duration::from_secs(10),
        |response| {
            response.rooms.joined.get(&room_id).is_some_and(|joined| {
                joined.ephemeral.iter().any(|raw| {
                    json_type_and_body(raw.json().get())
                        .map(|(ty, body)| {
                            ty == "m.receipt"
                                && body.contains(bob_id.as_str())
                                && body.contains(alice_message_event_id.as_str())
                        })
                        .unwrap_or(false)
                })
            })
        },
    )
    .await
    .context("alice's bounded-wait /sync for bob's read receipt should succeed")?;
    if !saw_receipt {
        bail!(
            "alice's /sync never saw bob's m.read receipt on {alice_message_event_id} within \
             the bounded wait (this crate's own code -- not a documented cross-track gap)"
        );
    }
    step!("alice's /sync saw bob's public read receipt on {alice_message_event_id}");

    // The fully-read marker (`POST /rooms/{roomId}/read_markers`'s `m.fully_read` field) is
    // private room account data, not an ephemeral event -- checked via a fresh, un-tokened sync
    // for bob himself (current data, no wake-latency race to account for; the wake path itself
    // is already covered by the read-receipt check just above and by
    // `hs_user::sync::tests::a_receipt_wakes_a_long_poll_and_appears_as_m_receipt`).
    bob_room
        .send_multiple_receipts(
            Receipts::new().fully_read_marker(Some(alice_message_event_id.clone())),
        )
        .await
        .context("bob setting the fully-read marker via POST .../read_markers should succeed")?;
    let bob_fresh_sync = bob
        .sync_once(SyncSettings::default())
        .await
        .context("bob's fresh /sync after setting the fully-read marker should succeed")?;
    let bob_saw_fully_read = bob_fresh_sync
        .rooms
        .joined
        .get(&room_id)
        .is_some_and(|joined| {
            joined.account_data.iter().any(|raw| {
                json_type_and_body(raw.json().get())
                    .map(|(ty, body)| {
                        ty == "m.fully_read" && body.contains(alice_message_event_id.as_str())
                    })
                    .unwrap_or(false)
            })
        });
    if !bob_saw_fully_read {
        bail!(
            "bob's own /sync never reported his m.fully_read marker on {alice_message_event_id} \
             as room account data"
        );
    }
    step!("bob's /sync reported his own m.fully_read marker on {alice_message_event_id}");

    // 17. Log out.
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

/// Extracts `(type, content-as-debug-string)` from a raw event's JSON text. Shared by every
/// event-shape check in this file (timeline events, `ephemeral` events, `state` events -- three
/// distinct `matrix-sdk` types that all wrap the same underlying `Raw<T>` JSON), so a single
/// substring check (`body.contains(...)`) works uniformly regardless of which section of a sync
/// response an event came from. Returns `None` for an event this scenario doesn't need to inspect
/// closely (never treated as a hard failure by itself; callers report a clear error listing what
/// *was* found instead).
fn json_type_and_body(raw_json: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(raw_json).ok()?;
    let event_type = value.get("type")?.as_str()?.to_owned();
    let body = value
        .get("content")
        .map(|c| c.to_string())
        .unwrap_or_default();
    Some((event_type, body))
}

/// [`json_type_and_body`], specialized to a timeline event (the shape every pre-existing step in
/// this scenario already used).
fn event_type_and_body(
    event: &matrix_sdk::deserialized_responses::TimelineEvent,
) -> Option<(String, String)> {
    json_type_and_body(event.raw().json().get())
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
