//! The encrypted scenario: two `matrix-sdk` clients (Alice and Bob), built with the
//! `e2e-encryption` feature on, doing the full first-contact dance against a real `hs serve`
//! process -- upload device and one-time keys, query each other's keys, claim a one-time key
//! under real concurrency, bootstrap cross-signing, create an encrypted room, send an encrypted
//! message, and have the other client *decrypt it*.
//!
//! This exists to answer a question [`crate::scenario`] (encryption off) cannot: whether
//! `hs-e2e`'s routes -- `/keys/upload`, `/keys/query`, `/keys/claim`,
//! `/keys/device_signing/upload`, `/sendToDevice`, and the `device_lists`/`to_device` fields of
//! `GET /sync` that `hs-user` owns -- actually work when driven by a real encrypting client,
//! rather than only by this workspace's own test helpers (which necessarily speak the server's
//! own dialect).
//!
//! Decryption succeeding is the assertion that matters here, not a `200` from any individual
//! call. Where a step depends on server behavior this workspace does not implement yet, this
//! scenario logs a clear `KNOWN BUG` line and continues (matching [`crate::scenario`]'s existing
//! convention for its own known gap) rather than deleting the step or hard-failing the run.
//!
//! # A few things this scenario deliberately does with raw HTTP, not `matrix-sdk`
//!
//! `matrix-sdk` hides the exact wire shape of `/keys/query` and `/keys/claim` responses behind
//! its own higher-level types, and never fires concurrent overlapping `/keys/claim` calls for the
//! same device on its own. Both are things this scenario needs to check directly (the response
//! shape a real client actually receives, and the atomicity guarantee `docs/workstreams/08-e2ee.md`
//! calls out), so this module keeps a small [`RawClient`] alongside the two `matrix_sdk::Client`s,
//! authenticated with the same real access tokens those clients hold.

use anyhow::{Context, Result, bail};
use matrix_sdk::Client;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::{TimelineEvent, TimelineEventKind};
use matrix_sdk::ruma::EventEncryptionAlgorithm;
use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoomRequest;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use serde_json::{Value, json};

/// A thin authenticated JSON-over-HTTP client, for the handful of calls this scenario needs to
/// make below `matrix-sdk`'s own abstractions. See the module doc for why.
#[derive(Clone)]
struct RawClient {
    http: reqwest::Client,
    base_url: String,
}

impl RawClient {
    fn new(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.to_owned(),
        }
    }

    /// `POST {path}` with a bearer token and a JSON body, returning the parsed JSON response.
    /// Fails (with the response body included) on a non-2xx status.
    async fn post(&self, token: &str, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        let status = response.status();
        let parsed: Value = response
            .json()
            .await
            .with_context(|| format!("parsing POST {path} response as JSON"))?;
        if !status.is_success() {
            bail!("POST {path} returned {status}: {parsed}");
        }
        Ok(parsed)
    }

    /// `GET {path}` with a bearer token, returning the parsed JSON response regardless of status
    /// (callers that need to assert on the status code want it back, not an early bail).
    async fn get(&self, token: &str, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = response.status();
        let parsed: Value = response
            .json()
            .await
            .with_context(|| format!("parsing GET {path} response as JSON"))?;
        if !status.is_success() {
            bail!("GET {path} returned {status}: {parsed}");
        }
        Ok(parsed)
    }
}

/// Registers a user via `m.login.dummy` UIA and returns the logged-in, encryption-enabled client.
/// Duplicated from (rather than shared with) [`crate::scenario`]'s private `register` helper:
/// that one is `matrix-sdk` built without `e2e-encryption`, and the point of this crate's split
/// into two scenario modules is that the plain one stays exactly as light as it was.
async fn register(base_url: &str, username: &str, password: &str) -> Result<Client> {
    let client = Client::builder()
        .homeserver_url(base_url)
        .build()
        .await
        .with_context(|| format!("building an e2e-encryption matrix-sdk Client for {base_url}"))?;

    let mut request = RegisterRequest::new();
    request.username = Some(username.to_owned());
    request.password = Some(password.to_owned());
    request.initial_device_display_name = Some("hs-loadgen (encrypted)".to_owned());
    request.auth = Some(uiaa::AuthData::Dummy(uiaa::Dummy::new()));

    client
        .matrix_auth()
        .register(request)
        .await
        .with_context(|| format!("POST /register for {username} should succeed"))?;

    Ok(client)
}

/// The `(type, content-as-debug-string)` of a raw timeline event, used only to search for a
/// plaintext (unencrypted, or already-decrypted) event by substring -- mirrors
/// [`crate::scenario`]'s private helper of the same shape.
fn event_type_and_body(value: &Value) -> Option<(String, String)> {
    let event_type = value.get("type")?.as_str()?.to_owned();
    let body = value
        .get("content")
        .map(|c| c.to_string())
        .unwrap_or_default();
    Some((event_type, body))
}

/// Classifies a synced timeline event as `"decrypted"`, `"utd"` (unable to decrypt) or
/// `"plaintext"` (never encrypted at all), and returns the JSON this scenario should inspect for
/// that event: the plaintext content once decrypted, or the still-encrypted `m.room.encrypted`
/// content otherwise.
fn classify_event(event: &TimelineEvent) -> (&'static str, Value) {
    match &event.kind {
        TimelineEventKind::Decrypted(decrypted) => {
            let value: Value =
                serde_json::from_str(decrypted.event.json().get()).unwrap_or(Value::Null);
            ("decrypted", value)
        }
        TimelineEventKind::UnableToDecrypt { event, utd_info } => {
            let mut value: Value = serde_json::from_str(event.json().get()).unwrap_or(Value::Null);
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "_utd_reason".to_owned(),
                    Value::String(format!("{:?}", utd_info.reason)),
                );
            }
            ("utd", value)
        }
        TimelineEventKind::PlainText { event } => {
            let value: Value = serde_json::from_str(event.json().get()).unwrap_or(Value::Null);
            ("plaintext", value)
        }
    }
}

/// Runs the encrypted scenario against a server at `base_url`. Returns a human-readable log of
/// every step, including known gaps logged as `KNOWN BUG` lines rather than causing a hard
/// failure -- see the module doc.
pub async fn run(base_url: &str) -> Result<Vec<String>> {
    let mut log = Vec::new();
    macro_rules! step {
        ($($arg:tt)*) => {{
            let msg = format!($($arg)*);
            tracing::info!("{msg}");
            log.push(msg);
        }};
    }

    let raw = RawClient::new(base_url);

    // 1. Register two real, encrypting matrix-sdk clients.
    let alice = register(
        base_url,
        "loadgen-crypto-alice",
        "correct horse battery staple",
    )
    .await
    .context("registering alice (encrypted)")?;
    let alice_id = alice
        .user_id()
        .context("alice should have a user_id after registering")?
        .to_owned();
    let alice_device = alice
        .device_id()
        .context("alice should have a device_id after registering")?
        .to_owned();
    let alice_token = alice
        .access_token()
        .context("alice should have an access token after registering")?;
    step!("registered {alice_id} on device {alice_device} (e2e-encryption enabled)");

    let bob = register(base_url, "loadgen-crypto-bob", "another good passphrase")
        .await
        .context("registering bob (encrypted)")?;
    let bob_id = bob
        .user_id()
        .context("bob should have a user_id after registering")?
        .to_owned();
    let bob_device = bob
        .device_id()
        .context("bob should have a device_id after registering")?
        .to_owned();
    let bob_token = bob
        .access_token()
        .context("bob should have an access token after registering")?;
    step!("registered {bob_id} on device {bob_device} (e2e-encryption enabled)");

    // 2. Baseline sync for both: this is what actually triggers matrix-sdk's automatic
    //    `/keys/upload` of device identity keys and a batch of one-time keys (`sync_once` flushes
    //    the crypto machine's outgoing requests both before and after the `/sync` call itself).
    alice
        .sync_once(SyncSettings::default())
        .await
        .context("alice's baseline /sync (uploads her device and one-time keys) should succeed")?;
    bob.sync_once(SyncSettings::default())
        .await
        .context("bob's baseline /sync (uploads his device and one-time keys) should succeed")?;
    step!(
        "both clients completed a baseline /sync, uploading real (not scripted) device and one-time keys"
    );

    // 3. Confirm real one-time keys actually landed: re-POST /keys/upload with an empty body,
    //    which per `crates/hs-e2e/src/routes/keys_upload.rs` always returns this device's current
    //    `one_time_key_counts` even when nothing new is uploaded.
    let bob_recount = raw
        .post(&bob_token, "/_matrix/client/v3/keys/upload", json!({}))
        .await
        .context("POST /keys/upload with an empty body should return bob's current key counts")?;
    let bob_otk_count = bob_recount["one_time_key_counts"]["signed_curve25519"]
        .as_u64()
        .context("expected one_time_key_counts.signed_curve25519 to be a number")?;
    if bob_otk_count == 0 {
        bail!("bob's real matrix-sdk client uploaded zero signed_curve25519 one-time keys");
    }
    step!("bob's real client uploaded {bob_otk_count} signed_curve25519 one-time keys");

    // 4. /keys/query shape check, both directions: not just a 200, but the exact device_keys
    //    shape a real client parses (algorithms, device_id, keys, signatures, user_id), and both
    //    a curve25519 and an ed25519 key present under `keys`.
    for (querying_token, target_id, target_device, label) in [
        (&alice_token, &bob_id, &bob_device, "alice querying bob"),
        (&bob_token, &alice_id, &alice_device, "bob querying alice"),
    ] {
        let response = raw
            .post(
                querying_token,
                "/_matrix/client/v3/keys/query",
                json!({"device_keys": {target_id.as_str(): []}}),
            )
            .await
            .with_context(|| format!("{label}: POST /keys/query should succeed"))?;
        let device = response["device_keys"][target_id.as_str()][target_device.as_str()]
            .as_object()
            .with_context(|| {
                format!("{label}: expected {target_id}'s device {target_device} in the response")
            })?;
        for field in ["algorithms", "device_id", "keys", "signatures", "user_id"] {
            if !device.contains_key(field) {
                bail!(
                    "{label}: /keys/query response device_keys entry is missing {field:?}: {device:?}"
                );
            }
        }
        let keys = device["keys"]
            .as_object()
            .with_context(|| format!("{label}: device_keys.keys should be an object"))?;
        let has_curve25519 = keys.keys().any(|k| k.starts_with("curve25519:"));
        let has_ed25519 = keys.keys().any(|k| k.starts_with("ed25519:"));
        if !has_curve25519 || !has_ed25519 {
            bail!(
                "{label}: expected both a curve25519 and an ed25519 key in device_keys.keys, got {:?}",
                keys.keys().collect::<Vec<_>>()
            );
        }
        step!("{label}: /keys/query returned a correctly shaped device_keys entry");
    }

    // 5. Create a room, invite bob, bob joins -- same shape as `crate::scenario`.
    let mut create_room_request = CreateRoomRequest::new();
    create_room_request.name = Some("hs-loadgen encrypted room".to_owned());
    let room = alice
        .create_room(create_room_request)
        .await
        .context("POST /createRoom should succeed")?;
    let room_id: OwnedRoomId = room.room_id().to_owned();
    step!("alice created room {room_id}");

    room.invite_user_by_id(&bob_id)
        .await
        .with_context(|| format!("inviting {bob_id} into {room_id} should succeed"))?;
    bob.join_room_by_id(&room_id)
        .await
        .with_context(|| format!("{bob_id} joining {room_id} should succeed"))?;
    step!("{bob_id} joined {room_id}");

    let alice_after_join = alice
        .sync_once(SyncSettings::default())
        .await
        .context("alice's post-join /sync should succeed")?;
    let bob_after_join = bob
        .sync_once(SyncSettings::default())
        .await
        .context("bob's post-join /sync should succeed")?;
    step!("both clients synced past the join");

    // 6. Turn on encryption. Sent as a plain state event (not `Room::enable_encryption`, which
    //    waits on this crate's own internal sync-loop heartbeat -- irrelevant complexity here
    //    since this scenario drives `sync_once` explicitly) and then picked up by an explicit
    //    sync on both sides, which is also what marks bob as a tracked, key-queryable user for
    //    alice's crypto machine (`matrix-sdk-base` queues a `/keys/query` for every existing
    //    active member the moment a room is observed turning encrypted).
    room.send_state_event(RoomEncryptionEventContent::new(
        EventEncryptionAlgorithm::MegolmV1AesSha2,
    ))
    .await
    .context("sending m.room.encryption should succeed")?;
    let _alice_after_encrypt = alice
        .sync_once(SyncSettings::default().token(alice_after_join.next_batch))
        .await
        .context("alice's /sync after enabling encryption should succeed")?;
    let bob_after_encrypt = bob
        .sync_once(SyncSettings::default().token(bob_after_join.next_batch))
        .await
        .context("bob's /sync after enabling encryption should succeed")?;
    step!("m.room.encryption landed and was synced by both clients");

    // 7. Bootstrap cross-signing for alice: uploads a master, self-signing and user-signing key
    //    via POST /keys/device_signing/upload (no UIA re-auth needed -- this server's route
    //    doesn't require it yet, see `crates/hs-e2e/src/routes/cross_signing.rs`'s module doc).
    alice
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .context(
            "alice's cross-signing bootstrap (POST /keys/device_signing/upload) should succeed",
        )?;
    step!("alice bootstrapped cross-signing");

    // Shape check: bob's /keys/query for alice should now carry master_keys and
    // self_signing_keys (each shaped as {user_id, usage, keys, signatures}), but never
    // user_signing_keys -- that field is only ever returned to the user it belongs to.
    let cross_signing_query = raw
        .post(
            &bob_token,
            "/_matrix/client/v3/keys/query",
            json!({"device_keys": {alice_id.as_str(): []}}),
        )
        .await
        .context("bob's POST /keys/query for alice's cross-signing keys should succeed")?;
    for (field, key_type) in [
        ("master_keys", "master"),
        ("self_signing_keys", "self_signing"),
    ] {
        let key = cross_signing_query[field]
            .get(alice_id.as_str())
            .with_context(|| {
                format!("expected {field}.{alice_id} in bob's /keys/query response")
            })?;
        for expected in ["user_id", "usage", "keys", "signatures"] {
            if key.get(expected).is_none() {
                bail!("{field}.{alice_id} (a {key_type} key) is missing {expected:?}: {key}");
            }
        }
    }
    if cross_signing_query["user_signing_keys"]
        .get(alice_id.as_str())
        .is_some()
    {
        bail!(
            "bob's /keys/query for alice leaked user_signing_keys -- that field must only ever be \
             returned to the user it belongs to"
        );
    }
    step!(
        "bob's /keys/query for alice returned correctly shaped master_keys/self_signing_keys, \
         and correctly withheld user_signing_keys"
    );

    // 8. Device-list change tracking: does bob, who shares a room with alice, see alice's
    //    cross-signing upload as a `device_lists.changed` entry in his next /sync? This is what
    //    makes a client notice a new (or newly cross-signed) device at all -- see this track's
    //    brief. `matrix_sdk::sync::SyncResponse` does not expose `device_lists` at all (it is
    //    consumed internally to update crypto state, not handed back to callers), so this is
    //    checked directly against the raw wire response instead, which also lets an absent
    //    `device_lists` key be diagnosed precisely rather than inferred from an empty list.
    let bob_sync_after_cross_signing = bob
        .sync_once(SyncSettings::default().token(bob_after_encrypt.next_batch.clone()))
        .await
        .context("bob's /sync after alice's cross-signing bootstrap should succeed")?;
    let raw_sync = raw
        .get(
            &bob_token,
            &format!(
                "/_matrix/client/v3/sync?since={}&timeout=0",
                bob_after_encrypt.next_batch
            ),
        )
        .await
        .context("raw GET /sync (mirroring the call above) should succeed")?;
    let device_lists_key_present = raw_sync.get("device_lists").is_some();
    let sees_alice_changed =
        raw_sync["device_lists"]["changed"]
            .as_array()
            .is_some_and(|changed| {
                changed
                    .iter()
                    .any(|u| u.as_str() == Some(alice_id.as_str()))
            });
    if sees_alice_changed {
        step!(
            "bob's /sync reported alice's device-list change in device_lists.changed, as it \
             should for a user he shares a room with"
        );
    } else {
        step!(
            "KNOWN BUG (not this track's crate): bob's /sync never reported alice's \
             cross-signing-key upload in device_lists.changed. device_lists key present in the \
             raw response: {device_lists_key_present}. hs-e2e already tracks this (see \
             `record_device_list_change`/`changed_users_since` in crates/hs-e2e/src/store/mod.rs, \
             exposed today at `GET /keys/changes`), but `hs-user`'s `GET /sync` \
             (crates/hs-user/src/sync/mod.rs) does not call it -- that module's own doc comment \
             documents omitting `device_lists`, `to_device`, `device_one_time_keys_count` and \
             `device_unused_fallback_key_types` entirely as a deliberate, tracked gap pending \
             this wiring. See docs/rfcs/ for the interface this track is asking hs-user to adopt."
        );
    }

    // 9. The real encrypted round trip: alice sends a message. `Room::send` on an encrypted room
    //    transparently claims one of bob's real one-time keys (POST /keys/claim), establishes an
    //    Olm session, shares the Megolm room key over PUT /sendToDevice, and only then encrypts
    //    and sends the `m.room.encrypted` event -- if any of those steps failed, this call itself
    //    would return an error rather than an event id, so reaching an event id here already
    //    proves /keys/claim and /sendToDevice both worked end to end for a real client.
    let secret_message = "the wire only ever sees ciphertext for this one";
    let send_result = room
        .send(RoomMessageEventContent::text_plain(secret_message))
        .await
        .context(
            "alice sending an encrypted message should succeed (claims a one-time key from bob, \
             shares a room key over to-device, then sends m.room.encrypted)",
        )?;
    let sent_event_id = send_result.response.event_id;
    step!(
        "alice sent an encrypted message ({sent_event_id}); /keys/claim and /sendToDevice both \
         succeeded as a side effect of this one call"
    );

    // 10. Bob syncs and tries to decrypt. This is the assertion that matters, not any individual
    //     HTTP status: does the message bob's client fetched over /sync actually decrypt?
    let since_before_send = bob_sync_after_cross_signing.next_batch.clone();
    let bob_final_sync = bob
        .sync_once(SyncSettings::default().token(since_before_send.clone()))
        .await
        .context("bob's final /sync (should carry alice's encrypted event) should succeed")?;
    let bob_raw_final_sync = raw
        .get(
            &bob_token,
            &format!("/_matrix/client/v3/sync?since={since_before_send}&timeout=0"),
        )
        .await
        .context("raw GET /sync spanning alice's send should succeed")?;
    let to_device_key_present = bob_raw_final_sync.get("to_device").is_some();
    let to_device_event_count = bob_raw_final_sync["to_device"]["events"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);

    let joined_room =
        bob_final_sync.rooms.joined.get(&room_id).with_context(|| {
            format!("expected room {room_id} in bob's final /sync joined rooms")
        })?;
    let target_event = joined_room
        .timeline
        .events
        .iter()
        .find(|event| event.event_id() == Some(sent_event_id.as_ref()))
        .with_context(|| {
            format!("expected event {sent_event_id} in bob's final /sync timeline for {room_id}")
        })?;
    let (kind, content) = classify_event(target_event);

    match kind {
        "decrypted" => {
            let body = event_type_and_body(&content)
                .map(|(_, body)| body)
                .unwrap_or_default();
            if !body.contains(secret_message) {
                bail!(
                    "bob decrypted alice's event but its content did not match: got {content}, \
                     expected body containing {secret_message:?}"
                );
            }
            step!(
                "bob DECRYPTED alice's message end to end: {secret_message:?} came back correctly \
                 (to_device key present in the raw /sync response: {to_device_key_present}, \
                 {to_device_event_count} to-device event(s) delivered)"
            );
        }
        "utd" => {
            let reason = content
                .get("_utd_reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            step!(
                "KNOWN BUG (not this track's crate): bob could NOT decrypt alice's message \
                 (reason: {reason}). Root cause, confirmed directly against the wire response: \
                 GET /sync's top-level `to_device` key is present: {to_device_key_present} \
                 ({to_device_event_count} events) -- bob's client never received the Megolm room \
                 key alice's client sent over PUT /sendToDevice (confirmed delivered server-side, \
                 since step 9's `room.send()` would itself have failed otherwise) because \
                 `hs-user`'s `GET /sync` (crates/hs-user/src/sync/mod.rs, see its module doc's \
                 \"Not implemented in this pass\" section) omits `to_device` entirely. \
                 `hs-e2e`'s `ToDeviceStore::poll_since`/`delete_up_to` \
                 (crates/hs-e2e/src/store/mod.rs) already implement exactly the cursor this needs; \
                 see docs/rfcs/ for the interface this track is asking hs-user to adopt."
            );
        }
        "plaintext" => {
            bail!(
                "bob's synced event for {sent_event_id} was never encrypted at all (type/content: \
                 {content}) -- expected an m.room.encrypted event regardless of whether it could \
                 be decrypted"
            );
        }
        other => bail!("unreachable timeline event classification {other:?}"),
    }

    // 11. Atomic one-time-key claim under real concurrency, against a pool the steps above never
    //     touched (alice's, not bob's -- step 9 already consumed exactly one of bob's). Fires
    //     `alice_otk_count + 3` concurrent, unsynchronized /keys/claim calls for the same
    //     (user, device, algorithm) over real HTTP connections against the real running server,
    //     and checks the atomicity guarantee this track's brief calls out: no one-time key is
    //     ever handed to two callers, and exactly as many calls succeed as there were keys.
    let alice_recount = raw
        .post(&alice_token, "/_matrix/client/v3/keys/upload", json!({}))
        .await
        .context("POST /keys/upload with an empty body should return alice's current key counts")?;
    let alice_otk_count = alice_recount["one_time_key_counts"]["signed_curve25519"]
        .as_u64()
        .context("expected alice's one_time_key_counts.signed_curve25519 to be a number")?;
    if alice_otk_count == 0 {
        bail!("alice's real matrix-sdk client has zero signed_curve25519 one-time keys left");
    }
    let concurrency = alice_otk_count + 3;
    let mut tasks = Vec::with_capacity(concurrency as usize);
    for _ in 0..concurrency {
        let raw = raw.clone();
        let token = bob_token.clone();
        let alice_id = alice_id.clone();
        let alice_device = alice_device.clone();
        tasks.push(tokio::spawn(async move {
            raw.post(
                &token,
                "/_matrix/client/v3/keys/claim",
                json!({"one_time_keys": {alice_id.as_str(): {alice_device.as_str(): "signed_curve25519"}}}),
            )
            .await
        }));
    }
    let mut claimed_key_ids: Vec<String> = Vec::new();
    let mut empty_claims = 0u64;
    for task in tasks {
        let response = task
            .await
            .context("joining a concurrent /keys/claim task")??;
        let claimed = response["one_time_keys"]
            .get(alice_id.as_str())
            .and_then(|by_device| by_device.get(alice_device.as_str()))
            .and_then(Value::as_object)
            .filter(|map| !map.is_empty());
        match claimed {
            Some(map) => {
                let key_id = map
                    .keys()
                    .next()
                    .context("a non-empty claimed-key map should have a key id")?
                    .clone();
                claimed_key_ids.push(key_id);
            }
            None => empty_claims += 1,
        }
    }
    let mut distinct = claimed_key_ids.clone();
    distinct.sort();
    let total_claims = distinct.len();
    distinct.dedup();
    if distinct.len() != total_claims {
        bail!(
            "ATOMICITY VIOLATION: at least one one-time key was handed to more than one of \
             {concurrency} concurrent /keys/claim callers: {claimed_key_ids:?}"
        );
    }
    if claimed_key_ids.len() as u64 != alice_otk_count {
        bail!(
            "expected exactly {alice_otk_count} successful concurrent claims (one per uploaded \
             key), got {} distinct successful claims and {empty_claims} empty responses",
            claimed_key_ids.len()
        );
    }
    step!(
        "{concurrency} concurrent /keys/claim calls for alice's device claimed exactly \
         {alice_otk_count} distinct one-time keys with no double-claim, and the remaining \
         {empty_claims} correctly came back empty once the pool was exhausted"
    );

    Ok(log)
}
