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
//!   `hs-e2e`'s copy of the remote user's device list and its device-list stream (so a local
//!   user who shares a room with the remote user is told, in `/sync`'s `device_lists.changed`,
//!   to query their keys again; `hs_e2e::federation::receive_device_list_update`); to-device
//!   messages to the recipients' device inboxes, each `message_id` once
//!   (`hs_e2e::federation::receive_direct_to_device`).
//! - [`DeviceListAnnouncer`]: follows the local device-list stream and tells the servers of
//!   everyone a changed local user shares a room with: `m.device_list_update` for each device
//!   added, changed (keys or display name) or deleted, `m.signing_key_update` when the user's
//!   master or self-signing key changed (`hs_e2e::federation::device_list_update_edus`).
//! - [`ClientRemoteKeys`]: `hs_e2e::federation::RemoteKeys` over the federation client -- how a
//!   local `/keys/query` or `/keys/claim` for a remote user reaches that user's server, and how
//!   their whole device list is fetched (`GET /user/devices/{userId}`).
//! - [`HubRoomSharing`]: `hs_e2e::federation::RoomSharing` over the session hub -- whether a
//!   remote user shares a room with a local one, which decides whether their device list is
//!   copied here.
//!
//! # Observability
//!
//! Every EDU received is logged at debug level with its origin, type and outcome, and counted in
//! `hs_federation_edus_received_total{edu_type,outcome}`; every EDU sent is logged at debug level
//! by the sender once the destination accepts its transaction, and counted in
//! `hs_federation_edus_sent_total{edu_type}` (`hs_federation::metrics`).
//!
//! # Across a restart
//!
//! To-device messages and device-list and signing-key updates are durable EDUs
//! (`hs_federation::sender::DURABLE_EDU_TYPES`, RFC 0023): the sender keeps them in its store
//! until the destination accepts them, so one queued for a server that is down survives a restart
//! of this one (Complement's `stopped_server` cases). The announcer stores how far it has read
//! the device-list stream ([`ANNOUNCER_POSITION`], in the sender's store) and resumes from there,
//! so a change committed while the process stopped is announced at the next start; what it had
//! announced before is not remembered, so the first change of each user after a start is
//! announced as their whole device list. Typing, receipts and presence stay in memory: they
//! describe a moment. In a cluster, an EDU for a destination whose federation shard another
//! replica owns is forwarded to that replica over the mesh (`crate::edu_forward`), except the
//! device-list announcer's: every replica follows the stream and produces those for itself, and
//! each queues them only for the destinations it sends for. The stored position is one for the
//! whole cluster (the furthest any replica read), so a replica down while others read on does
//! not catch up on its own destinations' share.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hs_e2e::federation::{
    Announced, DIRECT_TO_DEVICE_EDU, InboundDeviceList, InboundToDevice, device_list_update_edus,
    local_device_list,
};
use hs_e2e::state::E2eState;
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
        // Never coalesced: every to-device message is delivered, in order; durable, kept in the
        // sender's store until the destination accepts it (RFC 0023).
        self.sender.enqueue_durable_edu(
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

    /// Hands an `m.device_list_update` or `m.signing_key_update` to `hs-e2e`, which keeps the
    /// copy of the user's list and records the change.
    async fn device_list_changed(&self, origin: &str, edu: &Edu) -> EduOutcome {
        let result = if edu.edu_type == "m.signing_key_update" {
            hs_e2e::federation::receive_signing_key_update(&self.e2e, origin, &edu.content).await
        } else {
            hs_e2e::federation::receive_device_list_update(&self.e2e, origin, &edu.content).await
        };
        let user_id = edu.content.get("user_id").and_then(Value::as_str);
        match result {
            Ok(InboundDeviceList::Dropped(reason)) => {
                tracing::debug!(
                    origin,
                    user_id,
                    edu_type = edu.edu_type,
                    reason,
                    "dropping an EDU"
                );
                EduOutcome::Dropped
            }
            Ok(InboundDeviceList::AlreadyKnown) => EduOutcome::Duplicate,
            Ok(outcome) => {
                tracing::debug!(
                    origin,
                    user_id,
                    edu_type = edu.edu_type,
                    ?outcome,
                    "device-list EDU"
                );
                EduOutcome::Applied
            }
            Err(error) => {
                tracing::warn!(origin, user_id, %error, "could not apply a remote device-list change");
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

/// How often [`DeviceListAnnouncer`] looks at the device-list stream without being woken: for
/// a change another replica committed (one committed in this process wakes it at once). A
/// client of another replica that uploads keys and a remote server that is told about it are
/// this far apart at most, plus the transaction.
pub const DEVICE_LIST_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Follows the local device-list stream and announces each local user's changes to the servers
/// that share a room with them. See the module docs.
///
/// Following the stream, not a hook: every device-list change -- a key upload, a device added,
/// renamed or deleted through `hs-auth`, a cross-signing key or signature -- already goes
/// through `hs-e2e`'s stream, and reading the stream catches all of them without either crate
/// learning about federation. It looks as soon as a write in this process commits
/// (`hs_e2e::store::DeviceKeyStore::subscribe_device_list_stream`), because who is told is
/// worked out when it looks: the servers sharing a room with the user then. A look a poll
/// interval late also told a server that joined in between, of a change made before it did
/// (Sytest's "Local device key changes get to remote servers", which saw the previous test's
/// user's new device first), and a remote user's sync that returned before the EDU arrived
/// missed it (Sytest's "If remote user leaves room we no longer receive device updates"). It
/// still polls every [`DEVICE_LIST_POLL_INTERVAL`] for the writes another replica commits. The stream names the user, not what changed, so the announcer remembers
/// what it last announced for each user (`hs_e2e::federation::Announced`) and sends only the
/// difference (`hs_e2e::federation::device_list_update_edus`). The first change it sees for a
/// user announces every device and any cross-signing keys, since it cannot tell what changed. It
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

/// The name the device-list announcer stores its stream position under in the federation
/// sender's store (`FederationSender::store_position`).
pub const ANNOUNCER_POSITION: &str = "device_list_announcer";

impl DeviceListAnnouncer {
    /// Starts following `e2e`'s stream from its current position, announcing through `sender`
    /// to the servers `hub` says share a room with each changed user of `own_server`.
    #[must_use]
    pub fn start<B: KvBackend + 'static>(
        hub: Arc<Hub<B>>,
        e2e: E2eState<B>,
        sender: Arc<FederationSender>,
        own_server: OwnedServerName,
    ) -> Self {
        let task = tokio::spawn(async move {
            let current = match e2e.store.current_stream_pos().await {
                Ok(pos) => pos,
                Err(error) => {
                    tracing::error!(%error, "cannot read the device-list stream; device-list updates will not be sent");
                    return;
                }
            };
            // Where the previous run got to, if it said: what was committed since is announced
            // now. A stored position past the stream (a store reset under it) starts from now.
            let mut last = match sender.stored_position(ANNOUNCER_POSITION) {
                Ok(Some(stored)) if stored <= current => {
                    if stored < current {
                        tracing::info!(
                            from = stored,
                            to = current,
                            "announcing device-list changes committed while this server was stopped"
                        );
                    }
                    stored
                }
                Ok(_) => current,
                Err(error) => {
                    tracing::warn!(%error, "cannot read where the device-list announcer got to; starting from now");
                    current
                }
            };
            let mut announced: HashMap<OwnedUserId, Announced> = HashMap::new();
            let mut interval = tokio::time::interval(DEVICE_LIST_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Woken by this process's own writes as they commit; the poll stays for the writes
            // of other replicas on a shared backend, and for a store that cannot say.
            let mut woken = e2e.store.subscribe_device_list_stream();
            loop {
                next_look(&mut interval, &mut woken).await;
                let now = match e2e.store.current_stream_pos().await {
                    Ok(pos) if pos > last => pos,
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::warn!(%error, "cannot read the device-list stream");
                        continue;
                    }
                };
                let changed = match e2e.store.changed_users_since(last, Some(now)).await {
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
                    if let Some(after) = announce(&hub, &e2e, &sender, &user_id, before).await {
                        announced.insert(user_id, after);
                    }
                }
                // Handed over (the EDUs are durable in the sender's store): the next start
                // begins here.
                if let Err(error) = sender.store_position(ANNOUNCER_POSITION, last) {
                    tracing::warn!(%error, "cannot store where the device-list announcer got to");
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

/// Waits for the announcer's next look at the stream: a write committed in this process
/// (`woken`), or the poll interval for anything else. A closed `woken` falls back to the poll.
async fn next_look(
    interval: &mut tokio::time::Interval,
    woken: &mut Option<tokio::sync::watch::Receiver<u64>>,
) {
    let Some(rx) = woken.as_mut() else {
        interval.tick().await;
        return;
    };
    tokio::select! {
        _ = interval.tick() => {}
        changed = rx.changed() => {
            if changed.is_err() {
                *woken = None;
            } else {
                rx.borrow_and_update();
            }
        }
    }
}

/// Sends what changed for `user_id` since `before` to the servers of everyone they share a
/// joined room with, and returns what is announced now (`None`: nothing could be read, so
/// nothing changes).
async fn announce<B: KvBackend + 'static>(
    hub: &Hub<B>,
    e2e: &E2eState<B>,
    sender: &FederationSender,
    user_id: &UserId,
    before: Option<&Announced>,
) -> Option<Announced> {
    let now = match local_device_list(e2e, user_id).await {
        Ok(now) => now,
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot read a user's devices to announce them");
            return None;
        }
    };
    let (edus, after) = device_list_update_edus(user_id, before, now);
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
        // Said, because a server that should have been told and was not is otherwise silent.
        tracing::debug!(
            %user_id,
            audience = audience.len(),
            "a device-list change with no other server sharing a room with the user"
        );
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
        // Local only: every replica follows this stream, so the owner of each destination
        // announces to it already (`crate::edu_forward`). Durable, as a device-list or
        // signing-key update is (`hs_federation::sender::DURABLE_EDU_TYPES`).
        sender.enqueue_edu_local(
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

    async fn devices(&self, server: &str, user_id: &str) -> Result<Value, String> {
        // `@` and `:` are path characters; nothing in a user id needs escaping.
        let path = format!("/_matrix/federation/v1/user/devices/{user_id}");
        match self.client.send(server, "GET", &path, None).await {
            Ok(response) if response.status == 200 => Ok(response.body),
            Ok(response) => Err(format!("HTTP {}: {}", response.status, response.body)),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// `hs_e2e::federation::RoomSharing` over the session hub's membership records. See the module
/// docs.
pub struct HubRoomSharing<B: KvBackend> {
    hub: Arc<Hub<B>>,
    own_server: OwnedServerName,
}

impl<B: KvBackend> HubRoomSharing<B> {
    /// Answers from `hub` for users of `own_server`.
    #[must_use]
    pub fn new(hub: Arc<Hub<B>>, own_server: OwnedServerName) -> Self {
        Self { hub, own_server }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> hs_e2e::federation::RoomSharing for HubRoomSharing<B> {
    async fn shares_a_room_with_a_local_user(&self, user_id: &UserId) -> bool {
        match self.hub.users_sharing_room_with(user_id).await {
            Ok(users) => users.iter().any(|u| u.server_name() == self.own_server),
            Err(error) => {
                tracing::warn!(%user_id, %error, "cannot work out whether a remote user shares a room here");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_e2e::federation::{AnnouncedDevice, LocalDeviceList};

    fn list(devices: &[(&str, Value)], master: Option<Value>, stream_id: u64) -> LocalDeviceList {
        LocalDeviceList {
            devices: devices
                .iter()
                .map(|(d, k)| {
                    (
                        (*d).to_owned(),
                        AnnouncedDevice {
                            keys: Some(k.clone()),
                            display_name: None,
                        },
                    )
                })
                .collect(),
            master,
            self_signing: None,
            stream_id,
        }
    }

    fn types(edus: &[(&'static str, Value)]) -> Vec<&'static str> {
        edus.iter().map(|(t, _)| *t).collect()
    }

    /// The announcer's contract with `hs-e2e`'s diff: the first change seen for a user
    /// announces everything, and the next announcement names the first in `prev_id`.
    #[test]
    fn the_first_change_seen_for_a_user_announces_everything_and_the_next_follows_it() {
        let user = ruma::user_id!("@alice:a.example");
        let (edus, after) = device_list_update_edus(
            user,
            None,
            list(&[("D1", json!({"k": 1}))], Some(json!({"m": 1})), 7),
        );
        assert_eq!(
            types(&edus),
            ["m.device_list_update", "m.signing_key_update"]
        );
        assert_eq!(edus[0].1["prev_id"], json!([]));
        assert_eq!(edus[0].1["stream_id"], 7);
        assert_eq!(edus[1].1["master_key"], json!({"m": 1}));
        let (edus, _) = device_list_update_edus(
            user,
            Some(&after),
            list(&[("D1", json!({"k": 2}))], Some(json!({"m": 1})), 9),
        );
        assert_eq!(types(&edus), ["m.device_list_update"]);
        assert_eq!(edus[0].1["prev_id"], json!([7]));
        assert_eq!(edus[0].1["stream_id"], 9);
    }

    /// The announcer looks as soon as a write commits, not at its next poll: who is told is
    /// worked out when it looks, and a late look tells a server that joined in between (see
    /// [`DeviceListAnnouncer`]). Time is paused, so a look that waited for the interval would
    /// show it.
    #[tokio::test(start_paused = true)]
    async fn a_committed_write_wakes_the_announcer_before_its_poll_and_a_closed_watch_falls_back() {
        let mut interval = tokio::time::interval(DEVICE_LIST_POLL_INTERVAL);
        interval.tick().await;
        let (tx, rx) = tokio::sync::watch::channel(0_u64);
        let mut woken = Some(rx);
        let started = tokio::time::Instant::now();
        tx.send_replace(5);
        next_look(&mut interval, &mut woken).await;
        assert_eq!(started.elapsed(), Duration::ZERO, "woken at once");

        drop(tx);
        next_look(&mut interval, &mut woken).await;
        assert!(woken.is_none(), "a closed watch is let go");
        next_look(&mut interval, &mut woken).await;
        assert!(
            started.elapsed() >= DEVICE_LIST_POLL_INTERVAL,
            "then the poll paces it"
        );
    }
}
