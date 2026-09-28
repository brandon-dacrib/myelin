//! Ephemeral data across servers: the adapters between `hs-federation` (transactions, which know
//! nothing about rooms or users), `hs-user` (typing, receipts, presence) and `hs-e2e` (device
//! lists, keys and to-device messages). Here for the same reason `crate::federation_sender` is:
//! this is the one crate that depends on all three.
//!
//! - [`SenderEduOutbox`]: `hs_user::edu::EduOutbox` and `hs_e2e::federation::ToDeviceOutbox` over
//!   the federation sender -- how a local user's typing, receipt or presence change, or a
//!   to-device message for a user of another server, is queued for other servers.
//! - [`EduDispatcher`]: `hs_federation::edu::InboundEduSink` -- where `/send`'s EDUs go:
//!   typing, receipts and presence to the session hub; device-list and signing-key updates to
//!   the device-list stream (so a local user who shares a room with the remote user is told, in
//!   `/sync`'s `device_lists.changed`, to query their keys again); to-device messages to the
//!   recipients' device inboxes, each `message_id` once (`hs_e2e::federation::
//!   receive_direct_to_device`).
//! - [`DeviceListAnnouncer`]: follows the local device-list stream and tells the servers of
//!   everyone a changed local user shares a room with: `m.device_list_update` for each device
//!   whose keys changed (or that was deleted), `m.signing_key_update` when the user's master or
//!   self-signing key changed.
//! - [`ClientRemoteKeys`]: `hs_e2e::federation::RemoteKeys` over the federation client -- how a
//!   local `/keys/query` or `/keys/claim` for a remote user reaches that user's server.
//!
//! # Observability
//!
//! Every EDU received is logged at debug level with its origin, type and outcome, and counted in
//! `hs_federation_edus_received_total{edu_type,outcome}`; every EDU sent is logged at debug level
//! by the sender once the destination accepts its transaction, and counted in
//! `hs_federation_edus_sent_total{edu_type}` (`hs_federation::metrics`).
//!
//! # What is not sent
//!
//! Device-list changes made while the server was down are not announced (the announcer starts at
//! the stream's position at start). In a cluster, an EDU for a destination whose federation shard
//! another replica owns is dropped by the sender rather than forwarded to that replica (see
//! `docs/status/06-federation.md`): the device-list announcer does not lose anything to that
//! (every replica follows the stream), but typing, receipts, presence and to-device messages
//! reach only the destinations the replica that took the request sends for.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hs_e2e::federation::{DIRECT_TO_DEVICE_EDU, InboundToDevice};
use hs_e2e::state::E2eState;
use hs_e2e::store::{CrossSigningKeyType, E2eStore};
use hs_federation::edu::{Edu, InboundEduSink};
use hs_federation::metrics::{EduMetrics, EduOutcome};
use hs_federation::sender::FederationSender;
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;
use hs_user::hub::SessionHub;
use ruma::{OwnedServerName, OwnedUserId, UserId};
use serde_json::{Value, json};

/// The session hub as `hs serve` builds it.
pub type Hub<B> = SessionHub<B, Arc<RoomRegistry<B>>>;

/// `hs_user::edu::EduOutbox` and `hs_e2e::federation::ToDeviceOutbox` over the federation
/// sender. See the module docs.
pub struct SenderEduOutbox {
    sender: Arc<FederationSender>,
}

impl SenderEduOutbox {
    /// An outbox that queues on `sender`.
    #[must_use]
    pub fn new(sender: Arc<FederationSender>) -> Self {
        Self { sender }
    }
}

impl hs_user::edu::EduOutbox for SenderEduOutbox {
    fn send_edu(
        &self,
        destinations: BTreeSet<String>,
        edu_type: &str,
        content: Value,
        coalesce_key: Option<String>,
    ) {
        self.sender
            .enqueue_edu(destinations, edu_type, content, coalesce_key);
    }
}

impl hs_e2e::federation::ToDeviceOutbox for SenderEduOutbox {
    fn send_direct_to_device(&self, destination: &str, content: Value) {
        let sender = content.get("sender").and_then(|s| s.as_str());
        let message_id = content.get("message_id").and_then(|m| m.as_str());
        tracing::debug!(
            destination,
            sender,
            message_id,
            "queueing a to-device message for another server"
        );
        // Never coalesced: every to-device message is delivered, in order.
        self.sender.enqueue_edu(
            [destination.to_owned()],
            DIRECT_TO_DEVICE_EDU,
            content,
            None,
        );
    }
}

/// Where `/send`'s EDUs go in `hs serve`. See the module docs.
pub struct EduDispatcher<B: KvBackend> {
    hub: Arc<Hub<B>>,
    e2e: E2eState<B>,
    metrics: EduMetrics,
}

impl<B: KvBackend + 'static> EduDispatcher<B> {
    /// A dispatcher applying typing, receipts and presence to `hub`, device-list and signing-key
    /// changes to `e2e`'s stream and to-device messages to `e2e`'s inboxes, counting each EDU in
    /// `metrics`.
    #[must_use]
    pub fn new(hub: Arc<Hub<B>>, e2e: E2eState<B>, metrics: EduMetrics) -> Self {
        Self { hub, e2e, metrics }
    }

    /// Records a device-list change for the user an `m.device_list_update` or
    /// `m.signing_key_update` names, if they are a user of `origin`.
    async fn device_list_changed(&self, origin: &str, edu: &Edu) -> EduOutcome {
        let Some(user_id) = edu
            .content
            .get("user_id")
            .and_then(Value::as_str)
            .and_then(|u| UserId::parse(u).ok())
        else {
            tracing::debug!(
                origin,
                edu_type = edu.edu_type,
                "dropping an EDU with no user_id"
            );
            return EduOutcome::Dropped;
        };
        if user_id.server_name().as_str() != origin {
            tracing::debug!(
                origin,
                %user_id,
                edu_type = edu.edu_type,
                "dropping an EDU about a user of another server"
            );
            return EduOutcome::Dropped;
        }
        match self.e2e.store.record_device_list_change(&user_id).await {
            Ok(_) => EduOutcome::Applied,
            Err(error) => {
                tracing::warn!(%user_id, %error, "could not record a remote device-list change");
                EduOutcome::Dropped
            }
        }
    }

    /// Delivers an `m.direct_to_device` EDU's messages to this server's devices.
    async fn direct_to_device(&self, origin: &str, edu: &Edu) -> EduOutcome {
        let message_id = edu.content.get("message_id").and_then(Value::as_str);
        match hs_e2e::federation::receive_direct_to_device(&self.e2e, origin, &edu.content).await {
            Ok(InboundToDevice::Delivered { messages }) => {
                tracing::debug!(origin, message_id, messages, "delivered to-device messages");
                EduOutcome::Applied
            }
            Ok(InboundToDevice::Duplicate) => {
                tracing::debug!(
                    origin,
                    message_id,
                    "a to-device message_id already delivered; not delivering it again"
                );
                EduOutcome::Duplicate
            }
            Ok(InboundToDevice::Dropped(reason)) => {
                tracing::debug!(origin, message_id, reason, "dropping a to-device EDU");
                EduOutcome::Dropped
            }
            Err(error) => {
                tracing::warn!(origin, message_id, %error, "could not deliver a to-device EDU");
                EduOutcome::Dropped
            }
        }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> InboundEduSink for EduDispatcher<B> {
    async fn receive_edu(&self, origin: &str, edu: Edu) {
        let outcome = match edu.edu_type.as_str() {
            "m.typing" | "m.receipt" | "m.presence" => {
                let applied = self
                    .hub
                    .receive_edu(origin, &edu.edu_type, &edu.content)
                    .await;
                if applied > 0 {
                    EduOutcome::Applied
                } else {
                    EduOutcome::Dropped
                }
            }
            "m.device_list_update" | "m.signing_key_update" => {
                self.device_list_changed(origin, &edu).await
            }
            DIRECT_TO_DEVICE_EDU => self.direct_to_device(origin, &edu).await,
            _ => EduOutcome::Dropped,
        };
        tracing::debug!(
            origin,
            edu_type = edu.edu_type,
            outcome = outcome.as_str(),
            "EDU received"
        );
        self.metrics.record_received(&edu.edu_type, outcome);
    }
}

/// How often [`DeviceListAnnouncer`] looks at the device-list stream. A client that uploads keys
/// and a remote server that is told about it are this far apart at most, plus the transaction.
pub const DEVICE_LIST_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Follows the local device-list stream and announces each local user's changes to the servers
/// that share a room with them. See the module docs.
///
/// Polling, not a hook: every device-list change -- a key upload, a device deleted through
/// `hs-auth`, a cross-signing key or signature -- already goes through `hs-e2e`'s stream, and
/// reading the stream catches all of them without either crate learning about federation. The
/// stream names the user, not what changed, so the announcer remembers what it last announced
/// for each user (their devices' keys, their master and self-signing keys) and sends only the
/// difference: an `m.device_list_update` per device added, changed or deleted, and an
/// `m.signing_key_update` when a cross-signing key changed. The first change it sees for a user
/// announces every device and any cross-signing keys, since it cannot tell what changed. It
/// starts from the stream's position at start: what changed while the server was down is not
/// announced (Synapse keeps an outbound table for that; a remote server here re-learns the list
/// on the next change, or when its user next queries).
///
/// In a cluster every replica follows the same stream, and each announces only to the
/// destinations it sends for (the sender drops an EDU for a destination another replica owns),
/// so every destination is told once.
pub struct DeviceListAnnouncer {
    task: tokio::task::JoinHandle<()>,
}

/// What [`DeviceListAnnouncer`] last announced for one user.
#[derive(Debug, Clone, Default, PartialEq)]
struct Announced {
    /// The device-list stream position of the last `m.device_list_update` sent, for `prev_id`.
    stream_id: Option<u64>,
    /// Each device's keys, as announced.
    devices: BTreeMap<String, Value>,
    /// The master key, as announced.
    master: Option<Value>,
    /// The self-signing key, as announced.
    self_signing: Option<Value>,
}

/// A user's device keys and cross-signing keys as they are now.
struct Current {
    devices: BTreeMap<String, Value>,
    master: Option<Value>,
    self_signing: Option<Value>,
}

/// The EDUs (type and content) that tell another server about the difference between `before`
/// (`None`: nothing is known to have been announced) and `now` for `user_id`, stamped with
/// device-list stream position `stream_id`. Returns them and what is announced afterwards.
fn updates_for(
    user_id: &UserId,
    before: Option<&Announced>,
    now: Current,
    stream_id: u64,
) -> (Vec<(&'static str, Value)>, Announced) {
    let mut edus = Vec::new();
    let unknown = Announced::default();
    let before_known = before.is_some();
    let before = before.unwrap_or(&unknown);
    let prev_id: Vec<u64> = before.stream_id.into_iter().collect();
    let mut sent_device_update = false;
    for (device_id, keys) in &now.devices {
        if before.devices.get(device_id) != Some(keys) {
            edus.push((
                "m.device_list_update",
                json!({
                    "user_id": user_id,
                    "device_id": device_id,
                    "stream_id": stream_id,
                    "prev_id": prev_id,
                    "deleted": false,
                    "keys": keys,
                }),
            ));
            sent_device_update = true;
        }
    }
    for device_id in before.devices.keys() {
        if !now.devices.contains_key(device_id) {
            edus.push((
                "m.device_list_update",
                json!({
                    "user_id": user_id,
                    "device_id": device_id,
                    "stream_id": stream_id,
                    "prev_id": prev_id,
                    "deleted": true,
                }),
            ));
            sent_device_update = true;
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
        stream_id: if sent_device_update {
            Some(stream_id)
        } else {
            before.stream_id
        },
        devices: now.devices,
        master: now.master,
        self_signing: now.self_signing,
    };
    (edus, announced)
}

impl DeviceListAnnouncer {
    /// Starts following `e2e`'s stream from its current position, announcing through `sender`
    /// to the servers `hub` says share a room with each changed user of `own_server`.
    #[must_use]
    pub fn start<B: KvBackend + 'static>(
        hub: Arc<Hub<B>>,
        e2e: Arc<dyn E2eStore>,
        sender: Arc<FederationSender>,
        own_server: OwnedServerName,
    ) -> Self {
        let task = tokio::spawn(async move {
            let mut last = match e2e.current_stream_pos().await {
                Ok(pos) => pos,
                Err(error) => {
                    tracing::error!(%error, "cannot read the device-list stream; device-list updates will not be sent");
                    return;
                }
            };
            let mut announced: HashMap<OwnedUserId, Announced> = HashMap::new();
            let mut interval = tokio::time::interval(DEVICE_LIST_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let now = match e2e.current_stream_pos().await {
                    Ok(pos) if pos > last => pos,
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::warn!(%error, "cannot read the device-list stream");
                        continue;
                    }
                };
                let changed = match e2e.changed_users_since(last, Some(now)).await {
                    Ok(users) => users,
                    Err(error) => {
                        tracing::warn!(%error, "cannot read the device-list changes");
                        continue;
                    }
                };
                last = now;
                for user_id in changed {
                    if user_id.server_name() != own_server {
                        continue;
                    }
                    let before = announced.get(&user_id);
                    if let Some(after) = announce(&hub, &*e2e, &sender, &user_id, now, before).await
                    {
                        announced.insert(user_id, after);
                    }
                }
            }
        });
        Self { task }
    }

    /// Stops following the stream.
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for DeviceListAnnouncer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Reads `user_id`'s keys now.
async fn current_keys(e2e: &dyn E2eStore, user_id: &UserId) -> Result<Current, String> {
    let devices = e2e
        .list_device_keys(user_id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(device_id, row)| (device_id.to_string(), row.keys))
        .collect();
    let master = e2e
        .get_cross_signing_key(user_id, CrossSigningKeyType::Master)
        .await
        .map_err(|e| e.to_string())?;
    let self_signing = e2e
        .get_cross_signing_key(user_id, CrossSigningKeyType::SelfSigning)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Current {
        devices,
        master,
        self_signing,
    })
}

/// Sends what changed for `user_id` since `before` to the servers of everyone they share a
/// joined room with, and returns what is announced now (`None`: nothing could be read, so
/// nothing changes).
async fn announce<B: KvBackend + 'static>(
    hub: &Hub<B>,
    e2e: &dyn E2eStore,
    sender: &FederationSender,
    user_id: &UserId,
    stream_id: u64,
    before: Option<&Announced>,
) -> Option<Announced> {
    let now = match current_keys(e2e, user_id).await {
        Ok(now) => now,
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot read a user's keys to announce them");
            return None;
        }
    };
    let (edus, after) = updates_for(user_id, before, now, stream_id);
    if edus.is_empty() {
        tracing::debug!(%user_id, "a device-list change with nothing another server is told about");
        return Some(after);
    }
    let audience = match hub.users_sharing_room_with(user_id).await {
        Ok(users) => users,
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot work out who to tell about a device-list change");
            return None;
        }
    };
    let destinations: BTreeSet<String> = audience
        .iter()
        .map(|u| u.server_name().to_string())
        .filter(|server| server != user_id.server_name().as_str())
        .collect();
    if destinations.is_empty() {
        return Some(after);
    }
    for (edu_type, content) in edus {
        let coalesce_key = match edu_type {
            "m.signing_key_update" => format!("signing {user_id}"),
            _ => format!(
                "device {user_id} {}",
                content["device_id"].as_str().unwrap_or_default()
            ),
        };
        tracing::debug!(
            %user_id,
            edu_type,
            destinations = destinations.len(),
            "announcing a key change to other servers"
        );
        sender.enqueue_edu(
            destinations.iter().cloned(),
            edu_type,
            content,
            Some(coalesce_key),
        );
    }
    Some(after)
}

/// `hs_e2e::federation::RemoteKeys` over the federation client. See the module docs.
pub struct ClientRemoteKeys {
    client: Arc<hs_federation::client::FederationClient>,
}

impl ClientRemoteKeys {
    /// Remote key access through `client`.
    #[must_use]
    pub fn new(client: Arc<hs_federation::client::FederationClient>) -> Self {
        Self { client }
    }

    async fn post(&self, server: &str, path: &str, body: Value) -> Result<Value, String> {
        match self.client.send(server, "POST", path, Some(&body)).await {
            Ok(response) if response.status == 200 => Ok(response.body),
            Ok(response) => Err(format!("HTTP {}: {}", response.status, response.body)),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[async_trait]
impl hs_e2e::federation::RemoteKeys for ClientRemoteKeys {
    async fn query(&self, server: &str, device_keys: Value) -> Result<Value, String> {
        self.post(
            server,
            "/_matrix/federation/v1/user/keys/query",
            json!({"device_keys": device_keys}),
        )
        .await
    }

    async fn claim(&self, server: &str, one_time_keys: Value) -> Result<Value, String> {
        self.post(
            server,
            "/_matrix/federation/v1/user/keys/claim",
            json!({"one_time_keys": one_time_keys}),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(devices: &[(&str, Value)], master: Option<Value>) -> Current {
        Current {
            devices: devices
                .iter()
                .map(|(d, k)| ((*d).to_owned(), k.clone()))
                .collect(),
            master,
            self_signing: None,
        }
    }

    fn types(edus: &[(&'static str, Value)]) -> Vec<&'static str> {
        edus.iter().map(|(t, _)| *t).collect()
    }

    #[test]
    fn the_first_change_seen_for_a_user_announces_everything() {
        let user = ruma::user_id!("@alice:a.example");
        let (edus, after) = updates_for(
            user,
            None,
            current(&[("D1", json!({"k": 1}))], Some(json!({"m": 1}))),
            7,
        );
        assert_eq!(
            types(&edus),
            ["m.device_list_update", "m.signing_key_update"]
        );
        assert_eq!(edus[0].1["prev_id"], json!([]));
        assert_eq!(edus[1].1["master_key"], json!({"m": 1}));
        assert_eq!(after.stream_id, Some(7));
    }

    #[test]
    fn a_cross_signing_change_is_a_signing_key_update_and_nothing_else() {
        let user = ruma::user_id!("@alice:a.example");
        let (_, before) = updates_for(user, None, current(&[("D1", json!({"k": 1}))], None), 3);
        let (edus, after) = updates_for(
            user,
            Some(&before),
            current(&[("D1", json!({"k": 1}))], Some(json!({"m": 1}))),
            4,
        );
        assert_eq!(types(&edus), ["m.signing_key_update"]);
        assert_eq!(edus[0].1["user_id"], user.as_str());
        // No device update went out, so the next one still follows the last that did.
        assert_eq!(after.stream_id, Some(3));
        // And the same keys again are nothing to announce.
        let (edus, _) = updates_for(
            user,
            Some(&after),
            current(&[("D1", json!({"k": 1}))], Some(json!({"m": 1}))),
            5,
        );
        assert!(edus.is_empty(), "{edus:?}");
    }

    #[test]
    fn only_changed_and_deleted_devices_are_announced_after_the_first_change() {
        let user = ruma::user_id!("@alice:a.example");
        let (_, before) = updates_for(
            user,
            None,
            current(&[("D1", json!({"k": 1})), ("D2", json!({"k": 2}))], None),
            3,
        );
        let (edus, after) = updates_for(
            user,
            Some(&before),
            current(&[("D1", json!({"k": 1})), ("D3", json!({"k": 3}))], None),
            9,
        );
        assert_eq!(edus.len(), 2, "{edus:?}");
        assert_eq!(edus[0].1["device_id"], "D3");
        assert_eq!(edus[0].1["deleted"], false);
        assert_eq!(edus[0].1["prev_id"], json!([3]));
        assert_eq!(edus[1].1["device_id"], "D2");
        assert_eq!(edus[1].1["deleted"], true);
        assert_eq!(after.stream_id, Some(9));
    }
}
