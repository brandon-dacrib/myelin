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
//! # No cache
//!
//! Every `/keys/query` for a remote user asks that user's server. Synapse caches a remote user's
//! device list while it shares a room with them and keeps the cache fresh from the
//! `m.device_list_update` EDUs it receives; this server records those EDUs as device-list changes
//! (so clients are told to re-query) but keeps no copy of the keys, so the re-query is always
//! answered by the only server that knows. That costs one federation request per query and can
//! never serve a stale key -- Complement's `TestDeviceListUpdates` checks exactly that a server
//! "must not return a cached device list" after a user left and changed their keys.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

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

#[cfg(test)]
mod tests {
    use super::*;

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
