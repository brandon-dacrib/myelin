//! Keys and to-device messages across servers: asking another server for its users' device keys
//! and one-time keys when a local client asks for them ([`RemoteKeys`], installed by `hs-cli`
//! over the federation client), answering the same two questions from another server about this
//! one's users ([`federation_keys_query`], [`federation_keys_claim`]), and to-device messages in
//! both directions ([`ToDeviceOutbox`] and [`direct_to_device_edus`] out,
//! [`receive_direct_to_device`] in).
//!
//! # To-device messages
//!
//! `PUT /sendToDevice` groups the messages addressed to users of other servers by server and
//! hands each server's share, as the content of one `m.direct_to_device` EDU (split if it would
//! not fit in one), to the installed [`ToDeviceOutbox`] -- in `hs serve`, the federation sender.
//! Each EDU carries a fresh `message_id`. A receiving server delivers each one once:
//! [`receive_direct_to_device`] marks `(sender, message_id)` in the same idempotency table
//! `/sendToDevice` uses for a client's `txnId` (under the pseudo-device
//! [`DIRECT_TO_DEVICE_TXN_DEVICE`], which no local sender's entries can collide with, since those
//! are keyed by the sender's own, local, user ID), so a transaction a remote server retries does
//! not deliver its messages twice. The table is durable: the dedupe holds across a restart.
//!
//! # Remote device lists: the copy this server keeps
//!
//! A `/keys/query` for a user of another server is answered from this server's own copy of their
//! device list ([`crate::store::RemoteDeviceListStore`]) when it holds one that is complete, and
//! otherwise by asking their server. Which way it asks depends on whether a local user shares a
//! room with them ([`RoomSharing`], installed by `hs-cli` over the session hub):
//!
//! - **Sharing a room** -- the user's server will send this one an `m.device_list_update` for
//!   every change, so a copy can be kept current. The copy is filled from
//!   `GET /user/devices/{userId}` (the whole list, its `stream_id`, and the master and
//!   self-signing keys), then each EDU whose `prev_id`s are all at or before the copy's
//!   `stream_id` is applied to it directly, and one that is not (a gap: an update was missed, or
//!   there is no copy yet) makes this server fetch the whole list again
//!   ([`receive_device_list_update`]), in the background ([`ResyncQueue`] says why). When the fetch fails the copy is marked stale and is not
//!   served until a fetch succeeds. A copy that is complete is served without asking anybody,
//!   which is what lets a client query keys while the other server is down.
//! - **Not sharing a room** -- no EDUs would come, so no copy is kept: the query goes to
//!   `POST /user/keys/query` on their server and the answer is passed on, uncached.
//!
//! Which is how Synapse does it and what Sytest's `50federation/40devicelists.pl` and
//! `41end-to-end-keys/06-device-lists.pl` check; it replaced an earlier design that cached
//! nothing and asked the user's server on every query. Complement's `TestDeviceListUpdates`
//! ("must not return a cached device list" after a user left and changed their keys) still holds:
//! a user who left shares no room, so their next change is a missed update, and the copy is
//! re-fetched before it is served again.
//!
//! The other direction is here too: [`federation_user_devices`] answers `/user/devices/{userId}`
//! for this server's own users, and [`device_list_update_edus`] works out the
//! `m.device_list_update` and `m.signing_key_update` EDUs that tell other servers what changed
//! (`hs-cli`'s announcer sends them to the servers sharing a room with the user).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ruma::{OwnedDeviceId, UserId};
use serde_json::{Map, Value, json};

use crate::error::E2eError;
use crate::store::{CrossSigningKeyType, RemoteDeviceRow, RemoteUserRow};

/// Reaches another server's `/user/keys/query` and `/user/keys/claim`. Implemented in `hs-cli`
/// over `hs_federation::client::FederationClient`; installed with
/// [`crate::state::E2eState::install_remote_keys`].
#[async_trait]
pub trait RemoteKeys: Send + Sync {
    /// `POST /_matrix/federation/v1/user/keys/query` to `server` with `{"device_keys":
    /// device_keys}`, returning the response body; `Err` describes why there is none.
    async fn query(&self, server: &str, device_keys: Value) -> Result<Value, String>;

    /// `POST /_matrix/federation/v1/user/keys/claim` to `server` with `{"one_time_keys":
    /// one_time_keys}`, returning the response body.
    async fn claim(&self, server: &str, one_time_keys: Value) -> Result<Value, String>;

    /// `GET /_matrix/federation/v1/user/devices/{user_id}` to `server`, returning the response
    /// body (the user's whole device list); `Err` describes why there is none.
    async fn devices(&self, server: &str, user_id: &str) -> Result<Value, String>;
}

/// Whether a user of another server shares a room with a user of this one -- the condition for
/// keeping a copy of their device list (see the module docs). Implemented in `hs-cli` over the
/// session hub's membership records; installed with
/// [`crate::state::E2eState::install_room_sharing`]. Without one, no copy is kept and every
/// remote query asks the user's server.
#[async_trait]
pub trait RoomSharing: Send + Sync {
    /// True if some user of this server is joined to a room `user_id` is joined to.
    async fn shares_a_room_with_a_local_user(&self, user_id: &UserId) -> bool;
}

/// The part of a request naming users of servers other than `own_server`, grouped by server:
/// `{server: {user_id: what was asked for them}}`.
pub(crate) fn remote_part(
    requested: &Map<String, Value>,
    own_server: &str,
) -> BTreeMap<String, Map<String, Value>> {
    let mut by_server: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for (user_id, asked) in requested {
        let Ok(parsed) = ruma::UserId::parse(user_id.as_str()) else {
            continue;
        };
        let server = parsed.server_name().as_str();
        if server != own_server {
            by_server
                .entry(server.to_owned())
                .or_default()
                .insert(user_id.clone(), asked.clone());
        }
    }
    by_server
}

/// Which remote call [`ask_servers`] makes.
#[derive(Clone, Copy)]
pub(crate) enum Ask {
    Query,
    Claim,
}

/// Asks every server in `by_server` at once and returns each one's answer (or why there is
/// none), by server.
pub(crate) async fn ask_servers(
    remote: &Arc<dyn RemoteKeys>,
    ask: Ask,
    by_server: BTreeMap<String, Map<String, Value>>,
) -> Vec<(String, Result<Value, String>)> {
    let mut tasks = tokio::task::JoinSet::new();
    for (server, users) in by_server {
        let remote = Arc::clone(remote);
        tasks.spawn(async move {
            let answer = match ask {
                Ask::Query => remote.query(&server, Value::Object(users)).await,
                Ask::Claim => remote.claim(&server, Value::Object(users)).await,
            };
            (server, answer)
        });
    }
    let mut answers = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(answer) => answers.push(answer),
            Err(error) => tracing::warn!(%error, "a remote key request task failed"),
        }
    }
    answers
}

/// Copies from `answer[field]` into `into` only the entries for users of `server` that were
/// asked about: a server answers for its own users, and nothing it says about anybody else's
/// keys is taken.
pub(crate) fn merge_for_server(
    into: &mut Map<String, Value>,
    answer: &Value,
    field: &str,
    server: &str,
    asked: &Map<String, Value>,
) {
    let Some(entries) = answer.get(field).and_then(Value::as_object) else {
        return;
    };
    for (user_id, value) in entries {
        let belongs =
            ruma::UserId::parse(user_id.as_str()).is_ok_and(|u| u.server_name().as_str() == server);
        if belongs && asked.contains_key(user_id) {
            into.insert(user_id.clone(), value.clone());
        }
    }
}

/// The `failures` entry for a server that could not be asked, in Synapse's shape.
pub(crate) fn failure(reason: &str) -> Value {
    json!({"status": 503, "message": reason})
}

/// Answers another server's `POST /user/keys/query`: device keys, master keys and self-signing
/// keys for this server's own users named in `device_keys`, and never anybody's user-signing key
/// (who a user has verified is theirs alone). Users of other servers are ignored.
///
/// # Errors
/// Returns [`crate::error::E2eError::BadRequest`] for a malformed request, or a storage error.
pub async fn federation_keys_query<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    device_keys: &Value,
) -> Result<Value, crate::error::E2eError> {
    let local = crate::routes::keys_query::local_keys_query(state, None, device_keys).await?;
    Ok(json!({
        "device_keys": local.device_keys,
        "master_keys": local.master_keys,
        "self_signing_keys": local.self_signing_keys,
    }))
}

/// Answers another server's `POST /user/keys/claim`: one key per requested device of this
/// server's own users, each claimed atomically (a one-time key if any is left, else the fallback
/// key). Users of other servers are ignored.
///
/// # Errors
/// Returns [`crate::error::E2eError::BadRequest`] for a malformed request, or a storage error.
pub async fn federation_keys_claim<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    one_time_keys: &Value,
) -> Result<Value, crate::error::E2eError> {
    let claimed = crate::routes::keys_claim::local_keys_claim(state, one_time_keys).await?;
    Ok(json!({"one_time_keys": claimed}))
}

/// Where `PUT /sendToDevice` hands the messages addressed to users of another server: one call
/// per `m.direct_to_device` EDU, `content` being the EDU's content (`sender`, `type`,
/// `message_id`, `messages`). Implemented in `hs-cli` over the federation sender; installed with
/// [`crate::state::E2eState::install_to_device_outbox`]. Fire and forget, like the endpoint.
pub trait ToDeviceOutbox: Send + Sync {
    /// Queues one `m.direct_to_device` EDU for `destination`.
    fn send_direct_to_device(&self, destination: &str, content: Value);
}

/// The EDU type of a to-device message between servers.
pub const DIRECT_TO_DEVICE_EDU: &str = "m.direct_to_device";

/// The pseudo-device a remote sender's `message_id`s are marked under in the to-device
/// idempotency table. See the module docs.
pub const DIRECT_TO_DEVICE_TXN_DEVICE: &str = "m.direct_to_device";

/// The most bytes of serialized content one `m.direct_to_device` EDU is built with: the spec's
/// (and `hs_federation::edu::MAX_EDU_BYTES`') 65 535 for the whole EDU, less room for the
/// `edu_type`/`content` wrapper.
pub const MAX_DIRECT_TO_DEVICE_CONTENT_BYTES: usize = 65_000;

/// Builds the `m.direct_to_device` EDU contents for one destination: `messages` is
/// `{user_id: {device_id: content}}` for that server's users. One EDU when it fits in
/// [`MAX_DIRECT_TO_DEVICE_CONTENT_BYTES`]; otherwise one per recipient user, and one per device
/// for a user whose share still does not fit. A single message too large to send at all is left
/// out (logged). Every EDU gets its own `message_id` -- `message_id` itself when there is one EDU,
/// `message_id` with a `-n` suffix otherwise -- since a receiver delivers a `message_id` once.
#[must_use]
pub fn direct_to_device_edus(
    sender: &str,
    event_type: &str,
    message_id: &str,
    messages: Map<String, Value>,
) -> Vec<Value> {
    let build = |messages: Map<String, Value>| {
        json!({
            "sender": sender,
            "type": event_type,
            "message_id": message_id,
            "messages": messages,
        })
    };
    let fits = |content: &Value| {
        serde_json::to_vec(content).is_ok_and(|v| v.len() <= MAX_DIRECT_TO_DEVICE_CONTENT_BYTES)
    };
    let whole = build(messages.clone());
    if fits(&whole) {
        return vec![whole];
    }
    let mut parts = Vec::new();
    for (user_id, per_device) in messages {
        let one_user = build(Map::from_iter([(user_id.clone(), per_device.clone())]));
        if fits(&one_user) {
            parts.push(one_user);
            continue;
        }
        let Some(devices) = per_device.as_object() else {
            continue;
        };
        for (device_id, content) in devices {
            let one_device = build(Map::from_iter([(
                user_id.clone(),
                json!({device_id.clone(): content.clone()}),
            )]));
            if fits(&one_device) {
                parts.push(one_device);
            } else {
                tracing::warn!(
                    recipient = %user_id,
                    device_id = %device_id,
                    event_type,
                    "a to-device message is too large for a federation EDU; not sent"
                );
            }
        }
    }
    for (n, part) in parts.iter_mut().enumerate() {
        part["message_id"] = Value::String(format!("{message_id}-{n}"));
    }
    parts
}

/// What [`receive_direct_to_device`] did with one `m.direct_to_device` EDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundToDevice {
    /// Queued this many messages for this server's devices (possibly none, when the EDU named
    /// only users of other servers or users with no devices).
    Delivered {
        /// How many device inboxes a message was added to.
        messages: usize,
    },
    /// The sender's `message_id` was delivered before: nothing was queued again.
    Duplicate,
    /// The EDU was malformed or spoke for a user of a server other than its origin; the reason.
    Dropped(&'static str),
}

/// Applies one `m.direct_to_device` EDU from `origin` (the transaction's authenticated sender):
/// checks that its `sender` is a user of `origin`, delivers each `(sender, message_id)` once (see
/// the module docs), and queues each message for the named devices of this server's users, `*`
/// meaning every device the user has. Messages for users of other servers are ignored.
///
/// # Errors
/// A storage error from the idempotency table, the device list or the to-device queue. A
/// malformed EDU is not an error: it is [`InboundToDevice::Dropped`].
pub async fn receive_direct_to_device<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    origin: &str,
    content: &Value,
) -> Result<InboundToDevice, crate::error::E2eError> {
    let Some(sender) = content
        .get("sender")
        .and_then(Value::as_str)
        .and_then(|s| ruma::UserId::parse(s).ok())
    else {
        return Ok(InboundToDevice::Dropped("no valid sender"));
    };
    if sender.server_name().as_str() != origin {
        return Ok(InboundToDevice::Dropped(
            "sender is not a user of the origin",
        ));
    }
    let Some(event_type) = content.get("type").and_then(Value::as_str) else {
        return Ok(InboundToDevice::Dropped("no type"));
    };
    let Some(message_id) = content
        .get("message_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(InboundToDevice::Dropped("no message_id"));
    };
    let Some(messages) = content.get("messages").and_then(Value::as_object) else {
        return Ok(InboundToDevice::Dropped("no messages"));
    };
    let txn_device: &ruma::DeviceId = DIRECT_TO_DEVICE_TXN_DEVICE.into();
    if state
        .store
        .check_and_mark_txn(&sender, txn_device, message_id)
        .await?
    {
        return Ok(InboundToDevice::Duplicate);
    }
    let own_server = state.auth.server_name();
    let mut delivered = 0;
    for (user_id, per_device) in messages {
        let Ok(recipient) = ruma::UserId::parse(user_id.as_str()) else {
            continue;
        };
        if recipient.server_name() != own_server {
            continue;
        }
        let Some(per_device) = per_device.as_object() else {
            continue;
        };
        for (device_id, message) in per_device {
            let devices: Vec<ruma::OwnedDeviceId> = if device_id == "*" {
                state
                    .auth
                    .store
                    .list_devices(&recipient)
                    .await
                    .map_err(|e| {
                        crate::error::E2eError::Store(crate::store::StoreError::Backend(
                            e.to_string(),
                        ))
                    })?
                    .into_iter()
                    .map(|device| device.device_id)
                    .collect()
            } else {
                vec![device_id.as_str().into()]
            };
            for device in devices {
                state
                    .store
                    .send_to_device(&sender, &recipient, &device, event_type, message.clone())
                    .await?;
                delivered += 1;
            }
        }
    }
    Ok(InboundToDevice::Delivered {
        messages: delivered,
    })
}

// ------------------------------------------------------------------------------------------
// This server's copy of remote users' device lists
// ------------------------------------------------------------------------------------------

/// Whether a copy of `user_id`'s device list is kept: true when a [`RoomSharing`] is installed
/// and says a local user shares a room with them.
async fn is_tracked<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    user_id: &UserId,
) -> bool {
    match state.room_sharing() {
        Some(sharing) => sharing.shares_a_room_with_a_local_user(user_id).await,
        None => false,
    }
}

/// A `/user/devices/{userId}` answer, parsed into what the store keeps. `None` if it is not
/// about `user_id` or not shaped like one.
fn parse_user_devices(
    user_id: &UserId,
    answer: &Value,
) -> Option<(RemoteUserRow, Vec<(OwnedDeviceId, RemoteDeviceRow)>)> {
    if answer.get("user_id").and_then(Value::as_str) != Some(user_id.as_str()) {
        return None;
    }
    let devices = answer.get("devices")?.as_array()?;
    let mut rows = Vec::with_capacity(devices.len());
    for device in devices {
        let Some(device_id) = device.get("device_id").and_then(Value::as_str) else {
            continue;
        };
        rows.push((
            OwnedDeviceId::from(device_id),
            RemoteDeviceRow {
                keys: device.get("keys").filter(|k| k.is_object()).cloned(),
                display_name: device
                    .get("device_display_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        ));
    }
    Some((
        RemoteUserRow {
            stream_id: answer.get("stream_id").and_then(Value::as_u64).unwrap_or(0),
            master: answer.get("master_key").filter(|k| k.is_object()).cloned(),
            self_signing: answer
                .get("self_signing_key")
                .filter(|k| k.is_object())
                .cloned(),
            stale: false,
        },
        rows,
    ))
}

/// Fetches `user_id`'s whole device list from their server and replaces the copy held. Returns
/// whether that succeeded; on failure the copy, if any, is marked stale (logged either way).
///
/// When what was fetched differs from the copy held (or there was none), a device-list change
/// is recorded for the user, so local users sharing a room with them are told in `/sync` to
/// query again -- as Synapse does after a resync. A first fetch made to answer a `/keys/query`
/// is such a change: Sytest's "Device list doesn't change if remote server is down" waits for
/// the remote user in `device_lists.changed` after its first query. A fetch that found the
/// copy current records nothing.
///
/// # Errors
/// A storage error.
pub async fn resync_remote_user<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    remote: &Arc<dyn RemoteKeys>,
    user_id: &UserId,
) -> Result<bool, E2eError> {
    let server = user_id.server_name().as_str();
    let parsed = match remote.devices(server, user_id.as_str()).await {
        Ok(answer) => parse_user_devices(user_id, &answer),
        Err(reason) => {
            tracing::warn!(%user_id, server, reason, "could not fetch a remote user's device list");
            None
        }
    };
    match parsed {
        Some((user, devices)) => {
            tracing::info!(
                %user_id,
                server,
                devices = devices.len(),
                stream_id = user.stream_id,
                "fetched a remote user's device list"
            );
            let changed = differs_from_copy(state, user_id, &user, &devices).await?;
            state
                .store
                .replace_remote_device_list(user_id, user, devices)
                .await?;
            if changed {
                state.store.record_device_list_change(user_id).await?;
            } else {
                tracing::debug!(%user_id, "a fetched device list matches the copy held");
            }
            Ok(true)
        }
        None => {
            state.store.mark_remote_user_stale(user_id).await?;
            Ok(false)
        }
    }
}

/// Whether a fetched list (`user`, `devices`) says anything a local client would see that the
/// copy held does not: a device, its keys or name, or a cross-signing key. No copy held
/// differs; a stale one is compared like any other (stale says it may be behind, not that it
/// is: Synapse's "our cache matches already").
async fn differs_from_copy<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    user_id: &UserId,
    user: &RemoteUserRow,
    devices: &[(OwnedDeviceId, RemoteDeviceRow)],
) -> Result<bool, E2eError> {
    let Some(held) = state.store.get_remote_user(user_id).await? else {
        return Ok(true);
    };
    if held.master != user.master || held.self_signing != user.self_signing {
        return Ok(true);
    }
    let held_devices: BTreeMap<OwnedDeviceId, RemoteDeviceRow> = state
        .store
        .list_remote_devices(user_id)
        .await?
        .into_iter()
        .collect();
    let fetched: BTreeMap<OwnedDeviceId, RemoteDeviceRow> = devices.iter().cloned().collect();
    Ok(held_devices != fetched)
}

/// What [`receive_device_list_update`] or [`receive_signing_key_update`] did with one EDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundDeviceList {
    /// The update followed the copy held and was applied to it.
    Applied,
    /// The update did not follow the copy held (or there was none), so the whole list is being
    /// fetched again from the user's server, in the background ([`ResyncQueue`]); the copy is
    /// stale until that is done. A fetch that fails leaves it stale, to be fetched again when
    /// next needed.
    ResyncScheduled,
    /// The user's list is not kept (no local user shares a room with them); the change was
    /// recorded so a client that does ask is told to query again, and nothing else was done.
    Noted,
    /// The update is at or before the copy's position: already known.
    AlreadyKnown,
    /// The EDU was malformed or spoke for a user of a server other than its origin; the reason.
    Dropped(&'static str),
}

/// The user an EDU from `origin` is about, if it names a valid user of `origin`.
fn edu_user(origin: &str, content: &Value) -> Result<ruma::OwnedUserId, &'static str> {
    let user_id = content
        .get("user_id")
        .and_then(Value::as_str)
        .and_then(|u| UserId::parse(u).ok())
        .ok_or("no valid user_id")?;
    if user_id.server_name().as_str() != origin {
        return Err("user is not of the origin server");
    }
    Ok(user_id)
}

/// Applies one `m.device_list_update` EDU from `origin` (the transaction's authenticated
/// sender). See the module docs for the rules; every outcome but
/// [`InboundDeviceList::Dropped`], [`InboundDeviceList::AlreadyKnown`] and a
/// [`InboundDeviceList::ResyncScheduled`] whose fetch found the copy current records a
/// device-list change for the user (the last when its fetch is done), so local clients that
/// share a room with them are told to query again.
///
/// # Errors
/// A storage error.
pub async fn receive_device_list_update<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    origin: &str,
    content: &Value,
) -> Result<InboundDeviceList, E2eError> {
    let user_id = match edu_user(origin, content) {
        Ok(user_id) => user_id,
        Err(reason) => return Ok(InboundDeviceList::Dropped(reason)),
    };
    let Some(device_id) = content.get("device_id").and_then(Value::as_str) else {
        return Ok(InboundDeviceList::Dropped("no device_id"));
    };
    let stream_id = content
        .get("stream_id")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prev_ids: Vec<u64> = content
        .get("prev_id")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();

    if let Some(held) = state.store.get_remote_user(&user_id).await?
        && !held.stale
    {
        if stream_id <= held.stream_id {
            tracing::debug!(%user_id, stream_id, held = held.stream_id, "a device-list update already known");
            return Ok(InboundDeviceList::AlreadyKnown);
        }
        if prev_ids.iter().all(|prev| *prev <= held.stream_id) {
            let device = if content.get("deleted").and_then(Value::as_bool) == Some(true) {
                None
            } else {
                Some(RemoteDeviceRow {
                    keys: content.get("keys").filter(|k| k.is_object()).cloned(),
                    display_name: content
                        .get("device_display_name")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            };
            state
                .store
                .apply_remote_device_update(&user_id, device_id.into(), device, stream_id)
                .await?;
            state.store.record_device_list_change(&user_id).await?;
            tracing::debug!(%user_id, device_id, stream_id, "applied a device-list update");
            return Ok(InboundDeviceList::Applied);
        }
        tracing::info!(
            %user_id,
            stream_id,
            ?prev_ids,
            held = held.stream_id,
            "a device-list update skipped something; fetching the list again"
        );
    }

    if !is_tracked(state, &user_id).await {
        state.store.record_device_list_change(&user_id).await?;
        return Ok(InboundDeviceList::Noted);
    }
    let Some(remote) = state.remote_keys() else {
        state.store.record_device_list_change(&user_id).await?;
        return Ok(InboundDeviceList::Noted);
    };
    // A fetch that changed the copy recorded the change itself; one that failed did not, and
    // local clients are told anyway, so that their next query fetches again.
    // The copy, if any, is not served while the fetch is under way: a query in between asks
    // the user's server itself.
    if state.store.get_remote_user(&user_id).await?.is_some() {
        state.store.mark_remote_user_stale(&user_id).await?;
    }
    schedule_resync(state, remote, user_id);
    Ok(InboundDeviceList::ResyncScheduled)
}

/// The remote users whose device list [`receive_device_list_update`] is fetching again in the
/// background, held by [`crate::state::E2eState`].
///
/// In the background, not in the transaction that brought the update: this server's federation
/// client sends to each destination one request at a time (Synapse's default too), so a
/// transaction from server B whose update made this server ask B for the user's devices waited
/// behind this server's own transaction to B -- and when B was doing the same, each server's
/// `/send` waited on the other's until both timed out, 30 s later, and B was then backed off
/// (Complement's `TestDeviceListUpdates`: every subtest after the first failed on the backoff).
/// Synapse resyncs in the background for the same reason. A user's fetch runs once at a time:
/// an update that arrives while one is under way asks for one more after it, not a second at
/// once.
#[derive(Debug)]
pub struct ResyncQueue {
    /// User -> whether another fetch was asked for while one is under way.
    users: std::sync::Mutex<BTreeMap<ruma::OwnedUserId, bool>>,
    /// How many users have a fetch under way, for [`crate::state::E2eState::resyncs_settled`].
    pub(crate) in_flight: tokio::sync::watch::Sender<usize>,
}

impl Default for ResyncQueue {
    fn default() -> Self {
        Self {
            users: std::sync::Mutex::new(BTreeMap::new()),
            in_flight: tokio::sync::watch::Sender::new(0),
        }
    }
}

impl ResyncQueue {
    /// Claims `user_id`'s fetch: `true` if the caller is to run it, `false` if one is under way
    /// (which is then asked to run once more).
    fn claim(&self, user_id: &UserId) -> bool {
        let mut users = self
            .users
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(again) = users.get_mut(user_id) {
            *again = true;
            return false;
        }
        users.insert(user_id.to_owned(), false);
        let count = users.len();
        drop(users);
        self.in_flight.send_replace(count);
        true
    }

    /// After a fetch of `user_id`'s: `true` if another was asked for meanwhile (and is now the
    /// caller's to run), `false` if the user is done with.
    fn finish(&self, user_id: &UserId) -> bool {
        let mut users = self
            .users
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(again) = users.get_mut(user_id)
            && *again
        {
            *again = false;
            return true;
        }
        users.remove(user_id);
        let count = users.len();
        drop(users);
        self.in_flight.send_replace(count);
        false
    }
}

/// Fetches `user_id`'s list again on a task of its own (see [`ResyncQueue`]). A fetch that
/// fails records a device-list change, so local clients query again and that query fetches;
/// one that changes the copy has recorded it ([`resync_remote_user`]).
fn schedule_resync<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    remote: &Arc<dyn RemoteKeys>,
    user_id: ruma::OwnedUserId,
) {
    if !state.resyncs.claim(&user_id) {
        tracing::debug!(%user_id, "a device-list fetch is under way; one more is asked for after it");
        return;
    }
    tracing::debug!(%user_id, "fetching a remote user's device list again in the background");
    let state = state.clone();
    let remote = remote.clone();
    tokio::spawn(async move {
        loop {
            match resync_remote_user(&state, &remote, &user_id).await {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(error) = state.store.record_device_list_change(&user_id).await {
                        tracing::warn!(%user_id, %error, "could not record a device-list change after a failed fetch");
                    }
                }
                Err(error) => {
                    tracing::warn!(%user_id, %error, "could not store a remote user's fetched device list");
                }
            }
            if !state.resyncs.finish(&user_id) {
                break;
            }
        }
    });
}

/// Applies one `m.signing_key_update` EDU from `origin`: the user's master and self-signing keys
/// replace those in the copy held, if one is; the change is recorded either way.
///
/// # Errors
/// A storage error.
pub async fn receive_signing_key_update<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    origin: &str,
    content: &Value,
) -> Result<InboundDeviceList, E2eError> {
    let user_id = match edu_user(origin, content) {
        Ok(user_id) => user_id,
        Err(reason) => return Ok(InboundDeviceList::Dropped(reason)),
    };
    let outcome = if state.store.get_remote_user(&user_id).await?.is_some() {
        state
            .store
            .set_remote_signing_keys(
                &user_id,
                content.get("master_key").filter(|k| k.is_object()).cloned(),
                content
                    .get("self_signing_key")
                    .filter(|k| k.is_object())
                    .cloned(),
            )
            .await?;
        InboundDeviceList::Applied
    } else {
        InboundDeviceList::Noted
    };
    state.store.record_device_list_change(&user_id).await?;
    Ok(outcome)
}

/// The keys for users of other servers that a `/keys/query` asked about, by field, plus the
/// servers that could not be asked -- the remote half of [`crate::routes::keys_query`]'s answer.
#[derive(Debug, Default)]
pub(crate) struct RemoteAnswer {
    pub(crate) device_keys: Map<String, Value>,
    pub(crate) master_keys: Map<String, Value>,
    pub(crate) self_signing_keys: Map<String, Value>,
    pub(crate) failures: Map<String, Value>,
}

/// Serves `user_id` from the copy held, honouring `wanted` (device ids; empty means all).
async fn answer_from_copy<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    user_id: &UserId,
    wanted: &[String],
    held: &RemoteUserRow,
    into: &mut RemoteAnswer,
) -> Result<(), E2eError> {
    let mut per_user = Map::new();
    for (device_id, row) in state.store.list_remote_devices(user_id).await? {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == device_id.as_str()) {
            continue;
        }
        let Some(mut keys) = row.keys else {
            continue;
        };
        if let Some(obj) = keys.as_object_mut() {
            let unsigned = obj
                .entry("unsigned")
                .or_insert_with(|| Value::Object(Map::new()));
            if let (Some(unsigned), Some(name)) = (unsigned.as_object_mut(), &row.display_name) {
                unsigned.insert(
                    "device_display_name".to_owned(),
                    Value::String(name.clone()),
                );
            }
        }
        per_user.insert(device_id.to_string(), keys);
    }
    into.device_keys
        .insert(user_id.to_string(), Value::Object(per_user));
    if let Some(master) = &held.master {
        into.master_keys.insert(user_id.to_string(), master.clone());
    }
    if let Some(self_signing) = &held.self_signing {
        into.self_signing_keys
            .insert(user_id.to_string(), self_signing.clone());
    }
    Ok(())
}

/// The device ids a `/keys/query` entry asks for (`[]`, or anything that is not a list of
/// strings, meaning every device).
fn wanted_devices(asked: &Value) -> Vec<String> {
    asked
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Answers the remote part of a `/keys/query`: `requested` is the request's `device_keys`
/// map, of which only users of other servers are considered. Each is served from the copy held
/// when it is complete; otherwise their list is fetched from their server and kept if a local
/// user shares a room with them, else their server is asked `POST /user/keys/query` and the
/// answer passed on. A server that could not be asked is in `failures`. See the module docs.
///
/// # Errors
/// A storage error.
pub(crate) async fn remote_keys_query<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    requested: &Map<String, Value>,
) -> Result<RemoteAnswer, E2eError> {
    let mut answer = RemoteAnswer::default();
    let Some(remote) = state.remote_keys() else {
        return Ok(answer);
    };
    let own_server = state.auth.server_name();
    let mut to_ask: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for (user_id_str, asked) in requested {
        let Ok(user_id) = UserId::parse(user_id_str.as_str()) else {
            continue;
        };
        if user_id.server_name() == own_server {
            continue;
        }
        let wanted = wanted_devices(asked);
        let held = state.store.get_remote_user(&user_id).await?;
        let tracked = is_tracked(state, &user_id).await;
        if let Some(held) = held {
            if !tracked {
                // No room is shared any more, so no update has been coming: whatever is held
                // may be behind (Complement's `TestDeviceListUpdates`: a user who left and
                // changed their keys must not be served the old ones). Dropped, and the
                // user's server asked.
                tracing::info!(%user_id, "a remote user shares no room here any more; dropping the copy of their device list");
                state.store.forget_remote_user(&user_id).await?;
            } else if !held.stale {
                answer_from_copy(state, &user_id, &wanted, &held, &mut answer).await?;
                continue;
            }
        }
        if tracked
            && resync_remote_user(state, remote, &user_id).await?
            && let Some(held) = state.store.get_remote_user(&user_id).await?
        {
            answer_from_copy(state, &user_id, &wanted, &held, &mut answer).await?;
            continue;
        }
        to_ask
            .entry(user_id.server_name().to_string())
            .or_default()
            .insert(user_id_str.clone(), asked.clone());
    }
    let asked = to_ask.clone();
    for (server, result) in ask_servers(remote, Ask::Query, to_ask).await {
        let Some(asked) = asked.get(&server) else {
            continue;
        };
        match result {
            Ok(body) => {
                for (field, into) in [
                    ("device_keys", &mut answer.device_keys),
                    ("master_keys", &mut answer.master_keys),
                    ("self_signing_keys", &mut answer.self_signing_keys),
                ] {
                    merge_for_server(into, &body, field, &server, asked);
                }
            }
            Err(reason) => {
                tracing::info!(server, reason, "could not query a server for device keys");
                answer.failures.insert(server, failure(&reason));
            }
        }
    }
    Ok(answer)
}

// ------------------------------------------------------------------------------------------
// This server's own users' device lists, as other servers see them
// ------------------------------------------------------------------------------------------

/// One of a local user's devices as announced to other servers: what
/// [`device_list_update_edus`] compares to find what changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncedDevice {
    /// The device's `device_keys`, if it has uploaded any.
    pub keys: Option<Value>,
    /// The device's display name, if it has one.
    pub display_name: Option<String>,
}

/// A local user's device list as it is now: every device `hs-auth` knows (with its keys, when
/// uploaded, and display name), the cross-signing keys other servers may see, and the user's
/// device-list stream position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDeviceList {
    /// By device id.
    pub devices: BTreeMap<String, AnnouncedDevice>,
    /// The master key, if any.
    pub master: Option<Value>,
    /// The self-signing key, if any.
    pub self_signing: Option<Value>,
    /// [`crate::store::DeviceKeyStore::user_stream_pos`] for the user.
    pub stream_id: u64,
}

/// Reads `user_id`'s [`LocalDeviceList`] now.
///
/// # Errors
/// A storage error.
pub async fn local_device_list<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    user_id: &UserId,
) -> Result<LocalDeviceList, E2eError> {
    let mut devices: BTreeMap<String, AnnouncedDevice> = state
        .auth
        .store
        .list_devices(user_id)
        .await
        .map_err(|e| crate::store::StoreError::Backend(e.to_string()))?
        .into_iter()
        .map(|device| {
            (
                device.device_id.to_string(),
                AnnouncedDevice {
                    keys: None,
                    display_name: device.display_name,
                },
            )
        })
        .collect();
    for (device_id, row) in state.store.list_device_keys(user_id).await? {
        devices
            .entry(device_id.to_string())
            .or_insert_with(|| AnnouncedDevice {
                keys: None,
                display_name: None,
            })
            .keys = Some(row.keys);
    }
    Ok(LocalDeviceList {
        devices,
        master: state
            .store
            .get_cross_signing_key(user_id, CrossSigningKeyType::Master)
            .await?,
        self_signing: state
            .store
            .get_cross_signing_key(user_id, CrossSigningKeyType::SelfSigning)
            .await?,
        stream_id: state.store.user_stream_pos(user_id).await?,
    })
}

/// Answers another server's `GET /user/devices/{userId}` for one of this server's users:
/// their devices (keys when uploaded, display names), the user's device-list stream position,
/// and their master and self-signing keys. `None` for a user this server does not have.
/// The caller applies `allow_device_name_lookup_over_federation` (the federation route strips
/// the names when it is off).
///
/// # Errors
/// A storage error.
pub async fn federation_user_devices<B: hs_kv::KvBackend + 'static>(
    state: &crate::state::E2eState<B>,
    user_id: &UserId,
) -> Result<Option<Value>, E2eError> {
    if user_id.server_name() != state.auth.server_name()
        || state
            .auth
            .store
            .get_user(user_id)
            .await
            .map_err(|e| crate::store::StoreError::Backend(e.to_string()))?
            .is_none()
    {
        return Ok(None);
    }
    let list = local_device_list(state, user_id).await?;
    let devices: Vec<Value> = list
        .devices
        .into_iter()
        .map(|(device_id, device)| {
            let mut entry = json!({"device_id": device_id});
            if let Some(keys) = device.keys {
                entry["keys"] = keys;
            }
            if let Some(name) = device.display_name {
                entry["device_display_name"] = Value::String(name);
            }
            entry
        })
        .collect();
    let mut answer = json!({
        "user_id": user_id,
        "stream_id": list.stream_id,
        "devices": devices,
    });
    if let Some(master) = list.master {
        answer["master_key"] = master;
    }
    if let Some(self_signing) = list.self_signing {
        answer["self_signing_key"] = self_signing;
    }
    Ok(Some(answer))
}

/// What has been announced to other servers for one local user, as [`device_list_update_edus`]
/// left it: the next call compares against it. `Default` is "nothing is known to have been
/// announced".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announced {
    /// The stream position stamped on the last announcement, for the next one's `prev_id`.
    pub stream_id: Option<u64>,
    /// Each device as announced.
    pub devices: BTreeMap<String, AnnouncedDevice>,
    /// The master key as announced.
    pub master: Option<Value>,
    /// The self-signing key as announced.
    pub self_signing: Option<Value>,
}

/// The EDUs (type and content) that tell another server the difference between `before`
/// (`None`: nothing is known to have been announced) and `now` for `user_id`: an
/// `m.device_list_update` for each device added, changed (keys or display name) or deleted,
/// stamped with `now.stream_id` and naming the previous announcement in `prev_id`, and an
/// `m.signing_key_update` when a cross-signing key changed. The first announcement for a user
/// names every device and any cross-signing keys, since nothing is known to have been sent.
/// Returns them and what is announced afterwards.
#[must_use]
pub fn device_list_update_edus(
    user_id: &UserId,
    before: Option<&Announced>,
    now: LocalDeviceList,
) -> (Vec<(&'static str, Value)>, Announced) {
    let mut edus = Vec::new();
    let unknown = Announced::default();
    let before_known = before.is_some();
    let before = before.unwrap_or(&unknown);
    let prev_id: Vec<u64> = before.stream_id.into_iter().collect();
    let update = |device_id: &str, device: Option<&AnnouncedDevice>| {
        let mut content = json!({
            "user_id": user_id,
            "device_id": device_id,
            "stream_id": now.stream_id,
            "prev_id": prev_id,
            "deleted": device.is_none(),
        });
        if let Some(device) = device {
            if let Some(keys) = &device.keys {
                content["keys"] = keys.clone();
            }
            if let Some(name) = &device.display_name {
                content["device_display_name"] = Value::String(name.clone());
            }
        }
        ("m.device_list_update", content)
    };
    for (device_id, device) in &now.devices {
        if before.devices.get(device_id) != Some(device) {
            edus.push(update(device_id, Some(device)));
        }
    }
    for device_id in before.devices.keys() {
        if !now.devices.contains_key(device_id) {
            edus.push(update(device_id, None));
        }
    }
    let cross_signing_changed =
        now.master != before.master || now.self_signing != before.self_signing;
    let has_cross_signing = now.master.is_some() || now.self_signing.is_some();
    if has_cross_signing && (cross_signing_changed || !before_known) {
        let mut content = json!({"user_id": user_id});
        if let Some(master) = &now.master {
            content["master_key"] = master.clone();
        }
        if let Some(self_signing) = &now.self_signing {
            content["self_signing_key"] = self_signing.clone();
        }
        edus.push(("m.signing_key_update", content));
    }
    let announced = Announced {
        stream_id: Some(now.stream_id),
        devices: now.devices,
        master: now.master,
        self_signing: now.self_signing,
    };
    (edus, announced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(keys: Option<Value>, name: Option<&str>) -> AnnouncedDevice {
        AnnouncedDevice {
            keys,
            display_name: name.map(str::to_owned),
        }
    }

    fn list(devices: Vec<(&str, AnnouncedDevice)>, stream_id: u64) -> LocalDeviceList {
        LocalDeviceList {
            devices: devices
                .into_iter()
                .map(|(d, a)| (d.to_owned(), a))
                .collect(),
            master: None,
            self_signing: None,
            stream_id,
        }
    }

    /// Sytest's "Can query remote device keys using POST after notification" renames a device
    /// and waits for the other server to hear: a display-name change alone is an update, and a
    /// device with no keys is announced (with none) rather than left out.
    #[test]
    fn a_renamed_or_keyless_device_is_announced_and_an_unchanged_one_is_not() {
        let user = ruma::user_id!("@alice:a.example");
        let (edus, after) = device_list_update_edus(
            user,
            None,
            list(
                vec![
                    ("D1", named(Some(json!({"k": 1})), None)),
                    ("D2", named(None, Some("phone"))),
                ],
                4,
            ),
        );
        assert_eq!(edus.len(), 2, "{edus:?}");
        assert!(edus[0].1.get("device_display_name").is_none());
        assert!(edus[1].1.get("keys").is_none());
        assert_eq!(edus[1].1["device_display_name"], "phone");

        let (edus, after) = device_list_update_edus(
            user,
            Some(&after),
            list(
                vec![
                    ("D1", named(Some(json!({"k": 1})), Some("laptop"))),
                    ("D2", named(None, Some("phone"))),
                ],
                6,
            ),
        );
        assert_eq!(edus.len(), 1, "{edus:?}");
        assert_eq!(edus[0].1["device_id"], "D1");
        assert_eq!(edus[0].1["device_display_name"], "laptop");
        assert_eq!(edus[0].1["keys"], json!({"k": 1}));
        assert_eq!(edus[0].1["prev_id"], json!([4]));
        assert_eq!(edus[0].1["stream_id"], 6);

        let (edus, after) = device_list_update_edus(
            user,
            Some(&after),
            list(
                vec![("D1", named(Some(json!({"k": 1})), Some("laptop")))],
                7,
            ),
        );
        assert_eq!(edus.len(), 1, "{edus:?}");
        assert_eq!(edus[0].1["device_id"], "D2");
        assert_eq!(edus[0].1["deleted"], true);
        assert_eq!(after.stream_id, Some(7));

        let (edus, _) = device_list_update_edus(
            user,
            Some(&after),
            list(
                vec![("D1", named(Some(json!({"k": 1})), Some("laptop")))],
                8,
            ),
        );
        assert!(edus.is_empty(), "{edus:?}");
    }

    #[test]
    fn a_cross_signing_change_alone_is_a_signing_key_update() {
        let user = ruma::user_id!("@alice:a.example");
        let (_, before) = device_list_update_edus(user, None, list(vec![], 1));
        let mut now = list(vec![], 2);
        now.master = Some(json!({"m": 1}));
        let (edus, after) = device_list_update_edus(user, Some(&before), now.clone());
        assert_eq!(edus.len(), 1, "{edus:?}");
        assert_eq!(edus[0].0, "m.signing_key_update");
        assert_eq!(edus[0].1["master_key"], json!({"m": 1}));
        let (edus, _) = device_list_update_edus(user, Some(&after), now);
        assert!(edus.is_empty(), "{edus:?}");
    }

    #[test]
    fn a_user_devices_answer_is_believed_only_about_the_user_asked_for() {
        let bob = ruma::user_id!("@bob:there.example");
        let good = json!({
            "user_id": bob, "stream_id": 5,
            "devices": [
                {"device_id": "D1", "keys": {"k": 1}, "device_display_name": "one"},
                {"device_id": "D2"},
                {"no": "device_id"},
            ],
            "master_key": {"usage": ["master"]},
        });
        let (user, devices) = parse_user_devices(bob, &good).expect("well formed");
        assert_eq!(user.stream_id, 5);
        assert!(!user.stale);
        assert_eq!(user.master, Some(json!({"usage": ["master"]})));
        assert_eq!(user.self_signing, None);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].1.keys, Some(json!({"k": 1})));
        assert_eq!(devices[0].1.display_name.as_deref(), Some("one"));
        assert_eq!(devices[1].1.keys, None);

        let other = json!({"user_id": "@mallory:there.example", "stream_id": 1, "devices": []});
        assert!(parse_user_devices(bob, &other).is_none());
        assert!(parse_user_devices(bob, &json!({"user_id": bob})).is_none());
    }

    #[test]
    fn a_small_share_is_one_edu_with_the_message_id_as_given() {
        let messages = json!({"@b:there.example": {"D": {"x": 1}}});
        let edus = direct_to_device_edus(
            "@a:here.example",
            "m.test",
            "abc",
            messages.as_object().unwrap().clone(),
        );
        assert_eq!(
            edus,
            [json!({
                "sender": "@a:here.example",
                "type": "m.test",
                "message_id": "abc",
                "messages": messages,
            })]
        );
    }

    #[test]
    fn a_share_too_large_for_one_edu_is_split_by_user_then_device_and_each_part_has_its_own_id() {
        let big = "x".repeat(MAX_DIRECT_TO_DEVICE_CONTENT_BYTES / 3);
        let too_big = "y".repeat(MAX_DIRECT_TO_DEVICE_CONTENT_BYTES + 1);
        let messages = json!({
            "@b:there.example": {"D1": {"p": big}},
            "@c:there.example": {"D1": {"p": big}, "D2": {"p": big}},
            "@d:there.example": {"D1": {"p": big}, "D2": {"p": big}, "D3": {"p": big}},
            "@e:there.example": {"D1": {"p": too_big}},
        });
        let edus = direct_to_device_edus(
            "@a:here.example",
            "m.test",
            "abc",
            messages.as_object().unwrap().clone(),
        );
        // b and c each fit alone; d's three devices do not, so they go one by one; e's message
        // cannot be sent at all.
        assert_eq!(edus.len(), 5, "{:?}", edus.len());
        let ids: Vec<&str> = edus
            .iter()
            .map(|e| e["message_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["abc-0", "abc-1", "abc-2", "abc-3", "abc-4"]);
        assert!(edus.iter().all(|e| serde_json::to_vec(e).unwrap().len()
            <= MAX_DIRECT_TO_DEVICE_CONTENT_BYTES));
        assert!(
            edus.iter()
                .all(|e| e["messages"].get("@e:there.example").is_none())
        );
    }

    #[test]
    fn only_users_of_other_servers_are_grouped_by_server() {
        let requested = json!({
            "@a:here.example": [],
            "@b:there.example": ["D1"],
            "@c:there.example": [],
            "@d:third.example": [],
            "not a user": [],
        });
        let grouped = remote_part(requested.as_object().unwrap(), "here.example");
        assert_eq!(grouped.len(), 2);
        assert_eq!(
            Value::Object(grouped["there.example"].clone()),
            json!({"@b:there.example": ["D1"], "@c:there.example": []})
        );
        assert!(grouped.contains_key("third.example"));
    }

    #[test]
    fn a_server_is_believed_only_about_its_own_users_that_were_asked_about() {
        let asked = json!({"@b:there.example": []});
        let answer = json!({"device_keys": {
            "@b:there.example": {"D1": {"k": 1}},
            "@z:there.example": {"D9": {"k": 9}},
            "@a:here.example": {"EVIL": {"k": 0}},
        }});
        let mut into = Map::new();
        merge_for_server(
            &mut into,
            &answer,
            "device_keys",
            "there.example",
            asked.as_object().unwrap(),
        );
        assert_eq!(
            Value::Object(into),
            json!({"@b:there.example": {"D1": {"k": 1}}})
        );
    }
}
