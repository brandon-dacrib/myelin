//! Ephemeral data across servers: the adapters between `hs-federation` (transactions, which know
//! nothing about rooms or users), `hs-user` (typing, receipts, presence) and `hs-e2e` (device
//! lists and keys). Here for the same reason `crate::federation_sender` is: this is the one
//! crate that depends on all three.
//!
//! - [`SenderEduOutbox`]: `hs_user::edu::EduOutbox` over the federation sender -- how a local
//!   user's typing, receipt or presence change is queued for other servers.
//! - [`EduDispatcher`]: `hs_federation::edu::InboundEduSink` -- where `/send`'s EDUs go:
//!   typing, receipts and presence to the session hub, device-list and signing-key updates to
//!   the device-list stream (so a local user who shares a room with the remote user is told, in
//!   `/sync`'s `device_lists.changed`, to query their keys again).
//! - [`DeviceListAnnouncer`]: follows the local device-list stream and sends
//!   `m.device_list_update` for each local user whose devices changed to the servers of everyone
//!   they share a room with.
//! - [`ClientRemoteKeys`]: `hs_e2e::federation::RemoteKeys` over the federation client -- how a
//!   local `/keys/query` or `/keys/claim` for a remote user reaches that user's server.
//!
//! # What is not sent
//!
//! To-device messages (`m.direct_to_device`) do not cross servers yet; `hs-e2e`'s
//! `/sendToDevice` still drops a message for a remote user. Cross-signing key changes are
//! announced as device-list updates, not as `m.signing_key_update`: this server's device-list
//! stream does not say which kind of change it recorded. A receiving Synapse resynchronises the
//! whole list on an update it cannot place, which picks the new cross-signing keys up too.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hs_e2e::store::E2eStore;
use hs_federation::edu::{Edu, InboundEduSink};
use hs_federation::sender::FederationSender;
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;
use hs_user::hub::SessionHub;
use ruma::{OwnedServerName, UserId};
use serde_json::{Value, json};

/// The session hub as `hs serve` builds it.
pub type Hub<B> = SessionHub<B, Arc<RoomRegistry<B>>>;

/// `hs_user::edu::EduOutbox` over the federation sender. See the module docs.
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

/// Where `/send`'s EDUs go in `hs serve`. See the module docs.
pub struct EduDispatcher<B: KvBackend> {
    hub: Arc<Hub<B>>,
    e2e: Arc<dyn E2eStore>,
}

impl<B: KvBackend + 'static> EduDispatcher<B> {
    /// A dispatcher applying typing, receipts and presence to `hub` and device-list changes to
    /// `e2e`'s stream.
    #[must_use]
    pub fn new(hub: Arc<Hub<B>>, e2e: Arc<dyn E2eStore>) -> Self {
        Self { hub, e2e }
    }

    /// Records a device-list change for the user an `m.device_list_update` or
    /// `m.signing_key_update` names, if they are a user of `origin`.
    async fn device_list_changed(&self, origin: &str, edu: &Edu) {
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
            return;
        };
        if user_id.server_name().as_str() != origin {
            tracing::debug!(
                origin,
                %user_id,
                edu_type = edu.edu_type,
                "dropping an EDU about a user of another server"
            );
            return;
        }
        if let Err(error) = self.e2e.record_device_list_change(&user_id).await {
            tracing::warn!(%user_id, %error, "could not record a remote device-list change");
        }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> InboundEduSink for EduDispatcher<B> {
    async fn receive_edu(&self, origin: &str, edu: Edu) {
        match edu.edu_type.as_str() {
            "m.typing" | "m.receipt" | "m.presence" => {
                let applied = self
                    .hub
                    .receive_edu(origin, &edu.edu_type, &edu.content)
                    .await;
                tracing::debug!(origin, edu_type = edu.edu_type, applied, "applied an EDU");
            }
            "m.device_list_update" | "m.signing_key_update" => {
                self.device_list_changed(origin, &edu).await;
            }
            other => {
                tracing::debug!(
                    origin,
                    edu_type = other,
                    "dropping an EDU this server does not handle"
                );
            }
        }
    }
}

/// How often [`DeviceListAnnouncer`] looks at the device-list stream. A client that uploads keys
/// and a remote server that is told about it are this far apart at most, plus the transaction.
pub const DEVICE_LIST_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Follows the local device-list stream and announces each local user's changes to the servers
/// that share a room with them. See the module docs.
///
/// Polling, not a hook: every device-list change -- a key upload, a device deleted through
/// `hs-auth`, a cross-signing key -- already goes through `hs-e2e`'s stream, and reading the
/// stream catches all of them without either crate learning about federation. It starts from
/// the stream's position at start: what changed while the server was down is not announced
/// (Synapse keeps an outbound table for that; a remote server here re-learns the list on the
/// next change, or when its user next queries).
///
/// In a cluster every replica follows the same stream, and each announces only to the
/// destinations it sends for (the sender drops an EDU for a destination another replica owns),
/// so every destination is told once.
pub struct DeviceListAnnouncer {
    task: tokio::task::JoinHandle<()>,
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
            // The stream position each user was last announced at, for `prev_id`.
            let mut announced: HashMap<String, u64> = HashMap::new();
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
                    let prev = announced.insert(user_id.to_string(), now);
                    announce(&hub, &*e2e, &sender, &user_id, now, prev).await;
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

/// Sends one `m.device_list_update` per device `user_id` has keys for, to the servers of everyone
/// they share a joined room with.
async fn announce<B: KvBackend + 'static>(
    hub: &Hub<B>,
    e2e: &dyn E2eStore,
    sender: &FederationSender,
    user_id: &UserId,
    stream_id: u64,
    prev: Option<u64>,
) {
    let audience = match hub.users_sharing_room_with(user_id).await {
        Ok(users) => users,
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot work out who to tell about a device-list change");
            return;
        }
    };
    let destinations: BTreeSet<String> = audience
        .iter()
        .map(|u| u.server_name().to_string())
        .filter(|server| server != user_id.server_name().as_str())
        .collect();
    if destinations.is_empty() {
        return;
    }
    let devices = match e2e.list_device_keys(user_id).await {
        Ok(devices) => devices,
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot read a user's device keys to announce them");
            return;
        }
    };
    if devices.is_empty() {
        tracing::debug!(%user_id, "a device-list change for a user with no device keys is not announced");
    }
    for (device_id, row) in devices {
        let content = json!({
            "user_id": user_id,
            "device_id": device_id,
            "stream_id": stream_id,
            "prev_id": prev.map(|p| vec![p]).unwrap_or_default(),
            "deleted": false,
            "keys": row.keys,
        });
        sender.enqueue_edu(
            destinations.iter().cloned(),
            "m.device_list_update",
            content,
            Some(format!("device {user_id} {device_id}")),
        );
    }
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
