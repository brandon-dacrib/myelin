//! Federation writes in a cluster: what another server sends for a room reaches the replica
//! that owns the room's shard, whichever replica it arrived at
//! (`docs/decisions/0035-federation-requests-go-to-the-rooms-owner.md`).
//!
//! Two paths, one per shape of request:
//!
//! - **A request for one room** (`make_join`, `send_join`, `make_leave`, `send_leave`,
//!   `make_knock`, `send_knock`, `invite`, `exchange_third_party_invite`, and the room reads
//!   such as `get_missing_events`) names the room in its path. `crate::cluster::RoomShardGate`
//!   forwards it whole to the owner, exactly as it forwards a client's request: the owner replays
//!   it against its own router, whose `X-Matrix` layer verifies the signature again (the method,
//!   URI, body and `Authorization` header are the ones the sending server signed).
//! - **`/send`** carries PDUs of any number of rooms in one signed transaction, which cannot be
//!   split and re-signed. The replica that receives it verifies the transaction and each PDU as
//!   before, and [`ClusterWriteSink`] hands each write of a room another replica owns to that
//!   owner over the mesh (`federation.sink`, a `hs_cluster::mesh::Envelope` to the room's
//!   shard); [`SinkShardHandler`] applies it there through the owner's own
//!   [`RoomWriteSink`](hs_federation::inbound::RoomWriteSink). The mesh is authenticated (shared
//!   secret or mutual TLS), so the owner trusts the verification the receiving replica did, as it
//!   trusts any forwarded request's routing.
//!
//! Mid-handoff behaviour is decision 0017's: the forwarder waits out a shard with no owner or a
//! `421`/`503` from the believed one; a write fenced on this replica because its shard moved
//! away while it ran (nothing was stored) is sent on to the new owner; and an owner whose own
//! write is fenced answers `503`, which the forwarder retries against whoever owns the shard
//! next. Only when no owner could take the write within the deadline does `/send` answer `503`
//! (`hs_federation::inbound::TransactionError::NotOwner`) so the sending server retries.
//!
//! Each write forwarded is counted in `hs_cluster_forward_latency_seconds{kind="federation_pdu"}`
//! and logged at debug level.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use hs_cluster::mesh::{Envelope, Forwarder, IdempotencyKey, Reply, ShardHandler};
use hs_cluster::{Fence, Generation, Ownership, ReplicaId, ShardId, ShardLayout};
use hs_federation::inbound::{RoomWriteSink, WriteOutcome, WriteRejected};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The mesh route a forwarded federation write travels on, and the prefix [`install`] adds
/// [`SinkShardHandler`] for.
pub const SINK_ROUTE: &str = "federation.sink";

/// The `kind` label a forwarded `/send` write is counted under in
/// `hs_cluster_forward_latency_seconds`.
pub const FORWARD_KIND: &str = "federation_pdu";

/// The longest wait between two attempts at a write whose shard is moving.
const MAX_WAIT: Duration = Duration::from_millis(250);

/// One [`RoomWriteSink`] call, as it travels to the room's owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SinkCall {
    /// [`RoomWriteSink::accept_verified_event`].
    Accept {
        /// The room.
        room_id: String,
        /// The event's ID.
        event_id: String,
        /// The verified event.
        event: Value,
    },
    /// [`RoomWriteSink::accept_pushed_event`]: a PDU pushed over `/send`, which the owner may
    /// ignore because no user of this server is in its room.
    Pushed {
        /// The room.
        room_id: String,
        /// The event's ID.
        event_id: String,
        /// The verified event.
        event: Value,
    },
    /// [`RoomWriteSink::unknown_events`].
    Unknown {
        /// The room.
        room_id: String,
        /// The event IDs asked about.
        event_ids: Vec<String>,
    },
    /// [`RoomWriteSink::accept_auth_outliers`].
    AuthOutliers {
        /// The room.
        room_id: String,
        /// The verified auth events.
        events: Vec<Value>,
    },
    /// [`RoomWriteSink::accept_prev_event_with_state`].
    PrevWithState {
        /// The room.
        room_id: String,
        /// The prev event's ID.
        prev_event_id: String,
        /// The prev event.
        prev_event: Value,
        /// The state before it, as event IDs.
        state_before: Vec<String>,
        /// The events of that state this server lacked.
        fetched: Vec<Value>,
    },
}

impl SinkCall {
    /// The room the call writes to.
    #[must_use]
    pub fn room_id(&self) -> &str {
        match self {
            Self::Accept { room_id, .. }
            | Self::Pushed { room_id, .. }
            | Self::Unknown { room_id, .. }
            | Self::AuthOutliers { room_id, .. }
            | Self::PrevWithState { room_id, .. } => room_id,
        }
    }

    /// The call's name, for logs.
    #[must_use]
    pub fn op(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept",
            Self::Pushed { .. } => "pushed",
            Self::Unknown { .. } => "unknown",
            Self::AuthOutliers { .. } => "auth_outliers",
            Self::PrevWithState { .. } => "prev_with_state",
        }
    }
}

/// A [`WriteRejected`] on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRejected {
    /// [`WriteRejected::error`].
    pub error: String,
    /// [`WriteRejected::missing_ancestors`].
    #[serde(default)]
    pub missing_ancestors: Vec<String>,
    /// [`WriteRejected::auth_rejected`].
    #[serde(default)]
    pub auth_rejected: bool,
    /// [`WriteRejected::not_owner`].
    #[serde(default)]
    pub not_owner: bool,
}

impl From<WriteRejected> for WireRejected {
    fn from(r: WriteRejected) -> Self {
        Self {
            error: r.error,
            missing_ancestors: r.missing_ancestors,
            auth_rejected: r.auth_rejected,
            not_owner: r.not_owner,
        }
    }
}

impl From<WireRejected> for WriteRejected {
    fn from(r: WireRejected) -> Self {
        let mut rejected = WriteRejected::missing_ancestors(r.missing_ancestors, r.error);
        rejected.auth_rejected = r.auth_rejected;
        rejected.not_owner = r.not_owner;
        rejected
    }
}

/// What the owner answers a [`SinkCall`] with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum SinkAnswer {
    /// The event was stored.
    Stored,
    /// The event was already held.
    AlreadyKnown,
    /// The event was ignored: no user of this server is in its room.
    NotInRoom,
    /// [`RoomWriteSink::unknown_events`]'s answer.
    Unknown {
        /// The event IDs the owner does not hold.
        event_ids: Vec<String>,
    },
    /// [`RoomWriteSink::accept_auth_outliers`]'s answer.
    Held {
        /// How many were held.
        count: usize,
    },
    /// The write was refused.
    Rejected(WireRejected),
}

impl SinkAnswer {
    fn from_outcome(result: Result<WriteOutcome, WriteRejected>) -> Self {
        match result {
            Ok(WriteOutcome::Stored) => Self::Stored,
            Ok(WriteOutcome::AlreadyKnown) => Self::AlreadyKnown,
            Ok(WriteOutcome::NotInRoom) => Self::NotInRoom,
            Err(rejected) => Self::Rejected(rejected.into()),
        }
    }

    /// Whether this is a refusal because the answering replica does not own the room.
    fn is_not_owner(&self) -> bool {
        matches!(self, Self::Rejected(r) if r.not_owner)
    }

    fn into_outcome(self) -> Result<WriteOutcome, WriteRejected> {
        match self {
            Self::Stored => Ok(WriteOutcome::Stored),
            Self::AlreadyKnown => Ok(WriteOutcome::AlreadyKnown),
            Self::NotInRoom => Ok(WriteOutcome::NotInRoom),
            Self::Rejected(rejected) => Err(rejected.into()),
            other => Err(WriteRejected::other(format!(
                "the room's owner answered a write with {other:?}"
            ))),
        }
    }
}

/// Applies `call` to `sink`, the [`RoomWriteSink`] of this replica.
pub async fn apply(sink: &dyn RoomWriteSink, call: SinkCall) -> SinkAnswer {
    match call {
        SinkCall::Accept {
            room_id,
            event_id,
            event,
        } => SinkAnswer::from_outcome(
            sink.accept_verified_event(&room_id, &event_id, &event)
                .await,
        ),
        SinkCall::Pushed {
            room_id,
            event_id,
            event,
        } => SinkAnswer::from_outcome(sink.accept_pushed_event(&room_id, &event_id, &event).await),
        SinkCall::Unknown { room_id, event_ids } => SinkAnswer::Unknown {
            event_ids: sink.unknown_events(&room_id, &event_ids).await,
        },
        SinkCall::AuthOutliers { room_id, events } => {
            match sink.accept_auth_outliers(&room_id, &events).await {
                Ok(count) => SinkAnswer::Held { count },
                Err(rejected) => SinkAnswer::Rejected(rejected.into()),
            }
        }
        SinkCall::PrevWithState {
            room_id,
            prev_event_id,
            prev_event,
            state_before,
            fetched,
        } => SinkAnswer::from_outcome(
            sink.accept_prev_event_with_state(
                &room_id,
                &prev_event_id,
                &prev_event,
                &state_before,
                &fetched,
            )
            .await,
        ),
    }
}

/// How [`ClusterWriteSink`] reaches a room's owner. [`MeshOwner`] in a running server; a test
/// stands one in.
#[async_trait]
pub trait OwnerWrites: Send + Sync {
    /// Sends `call` to `shard`'s owner and returns its answer, or why none came.
    async fn send(&self, shard: ShardId, call: &SinkCall) -> Result<SinkAnswer, String>;
}

/// [`OwnerWrites`] over the mesh: one `federation.sink` envelope per call, with the forwarder's
/// retries (decision 0017).
pub struct MeshOwner {
    forwarder: Arc<Forwarder>,
    origin: ReplicaId,
    origin_generation: Generation,
    deadline: Duration,
}

impl MeshOwner {
    /// Sends from this replica (`origin`, `origin_generation`) through `forwarder`, each call
    /// within `deadline`.
    #[must_use]
    pub fn new(
        forwarder: Arc<Forwarder>,
        origin: ReplicaId,
        origin_generation: Generation,
        deadline: Duration,
    ) -> Self {
        Self {
            forwarder,
            origin,
            origin_generation,
            deadline,
        }
    }
}

#[async_trait]
impl OwnerWrites for MeshOwner {
    async fn send(&self, shard: ShardId, call: &SinkCall) -> Result<SinkAnswer, String> {
        let payload =
            serde_json::to_vec(call).map_err(|e| format!("encoding the forwarded write: {e}"))?;
        let env = Envelope {
            shard,
            route: SINK_ROUTE.to_owned(),
            idempotency_key: IdempotencyKey::generate(),
            requester: Value::Null,
            deadline: self.deadline,
            origin: self.origin.clone(),
            origin_generation: self.origin_generation,
            hops: 0,
            traceparent: None,
            payload: Bytes::from(payload),
        };
        let reply = self
            .forwarder
            .forward_as(FORWARD_KIND, env)
            .await
            .map_err(|e| format!("forwarding to the room's owner: {e}"))?;
        serde_json::from_slice::<SinkAnswer>(&reply.payload).map_err(|_| {
            format!(
                "the room's owner answered {}: {}",
                reply.status,
                String::from_utf8_lossy(&reply.payload)
            )
        })
    }
}

/// The [`RoomWriteSink`] `/send` writes through on a clustered replica: a write for a room this
/// replica owns goes to `local`, one for a room another replica owns is sent to it (see the
/// module docs).
pub struct ClusterWriteSink {
    local: Arc<dyn RoomWriteSink>,
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
    owners: Arc<dyn OwnerWrites>,
    deadline: Duration,
}

impl ClusterWriteSink {
    /// Writes through `local` when this replica owns a room's shard (by `ownership` and
    /// `layout`), and through `owners` otherwise; a write whose shard keeps moving is given up
    /// after `deadline`.
    #[must_use]
    pub fn new(
        local: Arc<dyn RoomWriteSink>,
        ownership: Arc<dyn Ownership>,
        layout: ShardLayout,
        owners: Arc<dyn OwnerWrites>,
        deadline: Duration,
    ) -> Self {
        Self {
            local,
            ownership,
            layout,
            owners,
            deadline,
        }
    }

    /// Makes `call` where the room's owner is: here, or at the owner over the mesh. A write
    /// fenced here because the shard moved away while it ran is made again at the new owner;
    /// one fenced while this replica still believes it owns the shard is tried again here once
    /// ownership has had a moment to settle, until `deadline`.
    async fn call(&self, call: SinkCall) -> SinkAnswer {
        let shard = self.layout.room_shard(call.room_id());
        let started = std::time::Instant::now();
        let mut wait = Duration::from_millis(25);
        loop {
            let answer = if self.ownership.is_mine(shard) {
                let answer = apply(self.local.as_ref(), call.clone()).await;
                if !answer.is_not_owner() {
                    return answer;
                }
                if !self.ownership.is_mine(shard) {
                    tracing::info!(
                        room_id = call.room_id(),
                        %shard,
                        op = call.op(),
                        "a federation write was fenced here because the room's shard moved; \
                         sending it on to the new owner"
                    );
                    continue;
                }
                answer
            } else {
                tracing::debug!(
                    room_id = call.room_id(),
                    %shard,
                    op = call.op(),
                    owner = ?self.ownership.owner_of(shard),
                    "handing a federation write to the room's owner"
                );
                match self.owners.send(shard, &call).await {
                    Ok(answer) if answer.is_not_owner() => answer,
                    Ok(answer) => return answer,
                    Err(reason) => {
                        tracing::warn!(
                            room_id = call.room_id(),
                            %shard,
                            op = call.op(),
                            %reason,
                            "could not hand a federation write to the room's owner"
                        );
                        return SinkAnswer::Rejected(WriteRejected::not_owner(reason).into());
                    }
                }
            };
            if started.elapsed() + wait >= self.deadline {
                return answer;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(MAX_WAIT);
        }
    }
}

#[async_trait]
impl RoomWriteSink for ClusterWriteSink {
    async fn accept_verified_event(
        &self,
        room_id: &str,
        event_id: &str,
        event_json: &Value,
    ) -> Result<WriteOutcome, WriteRejected> {
        self.call(SinkCall::Accept {
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            event: event_json.clone(),
        })
        .await
        .into_outcome()
    }

    async fn accept_pushed_event(
        &self,
        room_id: &str,
        event_id: &str,
        event_json: &Value,
    ) -> Result<WriteOutcome, WriteRejected> {
        self.call(SinkCall::Pushed {
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            event: event_json.clone(),
        })
        .await
        .into_outcome()
    }

    async fn unknown_events(&self, room_id: &str, event_ids: &[String]) -> Vec<String> {
        match self
            .call(SinkCall::Unknown {
                room_id: room_id.to_owned(),
                event_ids: event_ids.to_vec(),
            })
            .await
        {
            SinkAnswer::Unknown { event_ids } => event_ids,
            // Nobody could say: everything is fetched, as a sink that knows nothing answers.
            _ => event_ids.to_vec(),
        }
    }

    async fn accept_auth_outliers(
        &self,
        room_id: &str,
        events: &[Value],
    ) -> Result<usize, WriteRejected> {
        match self
            .call(SinkCall::AuthOutliers {
                room_id: room_id.to_owned(),
                events: events.to_vec(),
            })
            .await
        {
            SinkAnswer::Held { count } => Ok(count),
            SinkAnswer::Rejected(rejected) => Err(rejected.into()),
            other => Err(WriteRejected::other(format!(
                "the room's owner answered a write with {other:?}"
            ))),
        }
    }

    async fn accept_prev_event_with_state(
        &self,
        room_id: &str,
        prev_event_id: &str,
        prev_event: &Value,
        state_before: &[String],
        fetched: &[Value],
    ) -> Result<WriteOutcome, WriteRejected> {
        self.call(SinkCall::PrevWithState {
            room_id: room_id.to_owned(),
            prev_event_id: prev_event_id.to_owned(),
            prev_event: prev_event.clone(),
            state_before: state_before.to_vec(),
            fetched: fetched.to_vec(),
        })
        .await
        .into_outcome()
    }
}

/// Answers `federation.sink` on the room's owner: applies the write through this replica's own
/// [`RoomWriteSink`]. A write fenced here (the shard moved on while it ran) is answered `503`,
/// which the forwarding replica's forwarder retries against the shard's next owner.
pub struct SinkShardHandler {
    local: Arc<dyn RoomWriteSink>,
    layout: ShardLayout,
}

impl SinkShardHandler {
    /// Applies forwarded writes through `local`, checking each names a room of the envelope's
    /// shard by `layout`.
    #[must_use]
    pub fn new(local: Arc<dyn RoomWriteSink>, layout: ShardLayout) -> Self {
        Self { local, layout }
    }
}

#[async_trait]
impl ShardHandler for SinkShardHandler {
    async fn handle(&self, env: Envelope, _fence: Fence) -> Reply {
        let call: SinkCall = match serde_json::from_slice(&env.payload) {
            Ok(call) => call,
            Err(e) => {
                return Reply {
                    status: 400,
                    payload: Bytes::from(format!("bad forwarded federation write: {e}")),
                };
            }
        };
        if self.layout.room_shard(call.room_id()) != env.shard {
            return Reply {
                status: 400,
                payload: Bytes::from(format!(
                    "the forwarded write's room {} is not on shard {}",
                    call.room_id(),
                    env.shard
                )),
            };
        }
        let (room_id, op) = (call.room_id().to_owned(), call.op());
        let answer = apply(self.local.as_ref(), call).await;
        let status = if answer.is_not_owner() { 503 } else { 200 };
        tracing::debug!(
            from = %env.origin,
            %room_id,
            op,
            status,
            "applied a federation write handed over by another replica"
        );
        match serde_json::to_vec(&answer) {
            Ok(payload) => Reply {
                status,
                payload: Bytes::from(payload),
            },
            Err(e) => Reply {
                status: 500,
                payload: Bytes::from(format!("encoding the answer: {e}")),
            },
        }
    }
}

/// Makes the federation transport write `/send`'s PDUs where their rooms' owners are, and
/// answers the same from peers. Call it before
/// [`crate::cluster::ClusterHandles::spawn_mesh`]. A no-op in single-node mode (no forwarder:
/// every room is this replica's).
pub fn install(
    handles: &crate::cluster::ClusterHandles,
    state: &mut hs_federation::transport::FederationState,
) {
    let Some(forwarder) = handles.forwarder.clone() else {
        return;
    };
    let local = state.write_sink.clone();
    handles.add_shard_handler(
        SINK_ROUTE,
        Arc::new(SinkShardHandler::new(local.clone(), handles.layout)),
    );
    let owners = Arc::new(MeshOwner::new(
        forwarder,
        handles.origin().clone(),
        handles.origin_generation(),
        handles.default_deadline(),
    ));
    state.write_sink = Arc::new(ClusterWriteSink::new(
        local,
        handles.cluster.ownership().clone(),
        handles.layout,
        owners,
        handles.default_deadline(),
    ));
    tracing::info!(
        "federation writes for rooms another replica owns are handed to it over the mesh"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// A sink that records the rooms it was asked to write to, refusing as `not_owner` while
    /// `fenced` is set.
    #[derive(Default)]
    struct RecordingSink {
        writes: Mutex<Vec<String>>,
        fenced: AtomicBool,
    }

    #[async_trait]
    impl RoomWriteSink for RecordingSink {
        async fn accept_verified_event(
            &self,
            room_id: &str,
            _event_id: &str,
            _event_json: &Value,
        ) -> Result<WriteOutcome, WriteRejected> {
            if self.fenced.load(Ordering::SeqCst) {
                return Err(WriteRejected::not_owner("fenced: moved"));
            }
            self.writes.lock().unwrap().push(room_id.to_owned());
            Ok(WriteOutcome::Stored)
        }

        async fn accept_pushed_event(
            &self,
            room_id: &str,
            event_id: &str,
            event_json: &Value,
        ) -> Result<WriteOutcome, WriteRejected> {
            if room_id.starts_with("!out") {
                return Ok(WriteOutcome::NotInRoom);
            }
            self.accept_verified_event(room_id, event_id, event_json)
                .await
        }

        async fn unknown_events(&self, _room_id: &str, event_ids: &[String]) -> Vec<String> {
            event_ids.iter().skip(1).cloned().collect()
        }
    }

    /// Ownership of every shard, switchable.
    struct Toggle(AtomicBool);

    impl Ownership for Toggle {
        fn me(&self) -> &ReplicaId {
            static ME: std::sync::OnceLock<ReplicaId> = std::sync::OnceLock::new();
            ME.get_or_init(|| ReplicaId::new("me"))
        }
        fn is_mine(&self, _shard: ShardId) -> bool {
            self.0.load(Ordering::SeqCst)
        }
        fn owner_of(&self, _shard: ShardId) -> Option<ReplicaId> {
            None
        }
        fn fence(&self, _shard: ShardId) -> Option<Fence> {
            None
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<hs_cluster::OwnershipEvent> {
            tokio::sync::broadcast::channel(1).1
        }
        fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<hs_cluster::ShardMap>> {
            tokio::sync::watch::channel(Arc::new(hs_cluster::ShardMap::default())).1
        }
    }

    /// The owner, reached through [`SinkShardHandler`] the way the mesh reaches it: the call
    /// encoded, the reply decoded.
    struct ViaHandler {
        handler: SinkShardHandler,
        layout: ShardLayout,
        sent: AtomicUsize,
    }

    #[async_trait]
    impl OwnerWrites for ViaHandler {
        async fn send(&self, shard: ShardId, call: &SinkCall) -> Result<SinkAnswer, String> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            assert_eq!(shard, self.layout.room_shard(call.room_id()));
            let env = Envelope {
                shard,
                route: SINK_ROUTE.to_owned(),
                idempotency_key: IdempotencyKey::generate(),
                requester: Value::Null,
                deadline: Duration::from_secs(1),
                origin: ReplicaId::new("edge"),
                origin_generation: Generation::fresh(None),
                hops: 1,
                traceparent: None,
                payload: Bytes::from(serde_json::to_vec(call).unwrap()),
            };
            let reply = self.handler.handle(env, Fence::inert(shard)).await;
            serde_json::from_slice(&reply.payload).map_err(|e| e.to_string())
        }
    }

    fn sink(
        mine: bool,
        local: Arc<RecordingSink>,
        owner: Arc<RecordingSink>,
    ) -> (ClusterWriteSink, Arc<ViaHandler>, Arc<Toggle>) {
        let layout = ShardLayout::default();
        let ownership = Arc::new(Toggle(AtomicBool::new(mine)));
        let owners = Arc::new(ViaHandler {
            handler: SinkShardHandler::new(owner, layout),
            layout,
            sent: AtomicUsize::new(0),
        });
        (
            ClusterWriteSink::new(
                local,
                ownership.clone(),
                layout,
                owners.clone(),
                Duration::from_millis(500),
            ),
            owners,
            ownership,
        )
    }

    #[tokio::test]
    async fn a_write_for_a_room_this_replica_owns_is_made_here() {
        let (local, owner) = (Arc::default(), Arc::<RecordingSink>::default());
        let (sink, owners, _) = sink(true, Arc::clone(&local), owner.clone());
        let outcome = sink
            .accept_verified_event("!r:here", "$e", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Stored);
        assert_eq!(*local.writes.lock().unwrap(), vec!["!r:here".to_owned()]);
        assert!(owner.writes.lock().unwrap().is_empty());
        assert_eq!(owners.sent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_write_for_a_room_another_replica_owns_is_made_there() {
        let (local, owner) = (Arc::<RecordingSink>::default(), Arc::default());
        let (sink, owners, _) = sink(false, local.clone(), Arc::clone(&owner));
        let outcome = sink
            .accept_verified_event("!r:here", "$e", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Stored);
        assert!(local.writes.lock().unwrap().is_empty());
        assert_eq!(*owner.writes.lock().unwrap(), vec!["!r:here".to_owned()]);
        assert_eq!(owners.sent.load(Ordering::SeqCst), 1);
        // The other calls travel and come back as well.
        let unknown = sink
            .unknown_events("!r:here", &["$a".to_owned(), "$b".to_owned()])
            .await;
        assert_eq!(unknown, vec!["$b".to_owned()]);
    }

    /// A pushed PDU travels as a pushed PDU, so the owner decides whether this server is in
    /// its room (`WriteOutcome::NotInRoom`), not the replica that received it.
    #[tokio::test]
    async fn a_pushed_pdu_is_judged_by_the_owner() {
        let (local, owner) = (Arc::<RecordingSink>::default(), Arc::default());
        let (sink, owners, _) = sink(false, local.clone(), Arc::clone(&owner));
        let outcome = sink
            .accept_pushed_event("!out:here", "$e", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(outcome, WriteOutcome::NotInRoom);
        let outcome = sink
            .accept_pushed_event("!in:here", "$e", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Stored);
        assert!(local.writes.lock().unwrap().is_empty());
        assert_eq!(*owner.writes.lock().unwrap(), vec!["!in:here".to_owned()]);
        assert_eq!(owners.sent.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_write_fenced_here_because_the_shard_moved_is_made_at_the_new_owner() {
        let (local, owner) = (Arc::<RecordingSink>::default(), Arc::default());
        let (sink, owners, ownership) = sink(true, local.clone(), Arc::clone(&owner));
        local.fenced.store(true, Ordering::SeqCst);
        // The handoff completes while the write runs here: the next look at ownership says
        // the shard is elsewhere.
        let flip = {
            let ownership = ownership.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(60)).await;
                ownership.0.store(false, Ordering::SeqCst);
            })
        };
        let outcome = sink
            .accept_verified_event("!r:here", "$e", &serde_json::json!({}))
            .await
            .unwrap();
        flip.await.unwrap();
        assert_eq!(outcome, WriteOutcome::Stored);
        assert_eq!(*owner.writes.lock().unwrap(), vec!["!r:here".to_owned()]);
        assert_eq!(owners.sent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_write_no_owner_takes_in_time_is_refused_as_not_owner() {
        let (local, owner) = (
            Arc::<RecordingSink>::default(),
            Arc::<RecordingSink>::default(),
        );
        owner.fenced.store(true, Ordering::SeqCst);
        let (sink, owners, _) = sink(false, local, owner);
        let rejected = sink
            .accept_verified_event("!r:here", "$e", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(rejected.not_owner, "{rejected:?}");
        assert!(
            owners.sent.load(Ordering::SeqCst) > 1,
            "retried until the deadline"
        );
    }

    #[tokio::test]
    async fn the_owner_refuses_a_write_for_a_room_of_another_shard() {
        let layout = ShardLayout {
            rooms: 64,
            ..ShardLayout::default()
        };
        let handler = SinkShardHandler::new(Arc::new(RecordingSink::default()), layout);
        let call = SinkCall::Accept {
            room_id: "!r:here".to_owned(),
            event_id: "$e".to_owned(),
            event: serde_json::json!({}),
        };
        let shard = layout.room_shard("!r:here");
        let other = ShardId::new(shard.kind, (shard.index + 1) % 64);
        let reply = handler
            .handle(
                Envelope {
                    shard: other,
                    route: SINK_ROUTE.to_owned(),
                    idempotency_key: IdempotencyKey::generate(),
                    requester: Value::Null,
                    deadline: Duration::from_secs(1),
                    origin: ReplicaId::new("edge"),
                    origin_generation: Generation::fresh(None),
                    hops: 1,
                    traceparent: None,
                    payload: Bytes::from(serde_json::to_vec(&call).unwrap()),
                },
                Fence::inert(other),
            )
            .await;
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn a_rejection_round_trips_through_the_wire() {
        let mut rejected = WriteRejected::missing_ancestors(vec!["$a".to_owned()], "gap");
        rejected.auth_rejected = true;
        let answer = SinkAnswer::Rejected(rejected.into());
        let back: SinkAnswer =
            serde_json::from_slice(&serde_json::to_vec(&answer).unwrap()).unwrap();
        let back: WriteRejected = back.into_outcome().unwrap_err();
        assert_eq!(back.missing_ancestors, vec!["$a".to_owned()]);
        assert!(back.auth_rejected);
        assert!(!back.not_owner);
        assert_eq!(back.error, "gap");
    }
}
