//! The outbound federation sender: one queue per destination server, drained into
//! `PUT /_matrix/federation/v1/send/{txnId}` transactions through [`FederationClient`].
//!
//! # What this closes
//!
//! Before this module existed, nothing on this server ever sent a locally created event to
//! another server. The inbound half of `/send` (`crate::inbound`), the resident side of the join
//! handshake (`crate::join`) and the client side of it (`crate::outbound_join`) were all real, and
//! a remote server that joined a room here was then told nothing about anything that happened in
//! it afterwards. [`FederationSender`] is the missing half: hand it a PDU and the servers that
//! should receive it, and each of those servers gets it in a signed transaction, in order, retried
//! until it is accepted.
//!
//! # Shape
//!
//! - **One worker per destination**, spawned the first time anything is queued for it. The worker
//!   drains its queue into transactions of at most [`MAX_PDUS_PER_TRANSACTION`] PDUs (the spec's
//!   resource limit, the same constant `crate::inbound` enforces on receipt), each sent through
//!   [`FederationClient::send`] -- so discovery, TLS and CA trust, `X-Matrix` signing, the
//!   per-destination concurrency limit and the destination backoff records all apply exactly as
//!   they do to every other outbound call.
//! - **Per-destination ordering is preserved.** A transaction is retried, with the same
//!   transaction ID (so a receiver that did process it but whose response was lost replays its
//!   cached answer -- `crate::inbound::TransactionStore`'s contract), until it succeeds; nothing
//!   queued behind it is sent first. Destinations are independent: a failing destination delays
//!   nothing but its own queue.
//! - **Waiting, not spinning.** On [`ClientError::Backoff`] the worker sleeps until the
//!   destination store says the destination may be tried again (in slices of at most
//!   [`BACKOFF_POLL_INTERVAL`], so an administrator's reset takes effect promptly). On any other
//!   retryable failure -- a non-2xx status, a connection or discovery error -- it waits a capped,
//!   doubling delay ([`SenderConfig`]). Jitter is left to the destination store, which already
//!   jitters the connection-level backoff the client records.
//! - **A per-PDU `error` in a 200 response is final.** The receiver looked at that event and
//!   rejected it; sending it again would get the same answer. It is logged at `warn` and not
//!   retried, matching what the spec says a receiver's per-PDU result means.
//! - **Nothing is ever queued for this server's own name**, whatever a caller passes.
//!
//! # What this is not, said loudly
//!
//! **The queue is in memory only.** A restart, a crash, or [`FederationSender::shutdown`] loses
//! every PDU that has not yet been accepted by its destination, and there is no catch-up
//! afterwards: a remote server that was unreachable across a restart of this one will never be
//! told about the events it missed. `PLAN.md` section 5.2 item 6 wants per-destination queues
//! sharded across replicas by destination hash, each persisting its queue state so failover
//! resumes; Synapse's `destination_rooms` table (which remembers, per destination, the last
//! stream position successfully sent, so a whole outage can be caught up from the room's own
//! history) is the behavioural reference. A `KvBackend`-backed queue with that catch-up is the
//! next step; this module is the first one, and says so rather than pretending otherwise.
//!
//! **Only PDUs.** EDUs -- typing, presence, receipts, device-list updates, to-device messages,
//! signing-key updates -- are not sent. There is no `enqueue_edu`: adding one would have cost a
//! second queue with different batching and coalescing rules (typing notices supersede each
//! other; to-device messages must not be dropped), and an entry point that silently discarded
//! its argument would be worse than none.
//!
//! **Only `/send`.** Invites (`PUT /invite`), leaves and knocks against a remote resident
//! (`make_leave`/`send_leave`, `make_knock`/`send_knock`) are separate handshakes, not
//! transactions, and are not initiated here.
//!
//! **Not shard-gated.** Every process running a sender sends every PDU it is handed; nothing here
//! consults `hs-cluster` ownership. In a cluster this does not by itself duplicate traffic --
//! `hs-cli` feeds this sender from the room registry's update stream, and a room's actor is
//! resident on exactly the replica that owns its shard -- but a persisted, sharded sender will
//! need to own the "who sends for this destination" decision explicitly.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::client::{ClientError, FederationClient};
use crate::inbound::MAX_PDUS_PER_TRANSACTION;

/// Where an accepted event goes to reach other servers. Implemented by [`FederationSender`];
/// defined as a trait so the transport server (`crate::transport::FederationState`) and the
/// resident side of the join handshake (`crate::join::send_join`) can be tested with a recording
/// sink and no real client.
pub trait OutboundPduSink: Send + Sync {
    /// Queues `pdu` for delivery to every server in `destinations`. Never blocks and never
    /// fails: a destination this server must not or cannot reach is dealt with by the worker
    /// that owns its queue, and logged there.
    fn enqueue_pdu(&self, destinations: Vec<String>, pdu: Value);
}

/// How a destination's worker waits between failed attempts at one transaction. See the module
/// docs for which failures this applies to and which are governed by the destination store
/// instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderConfig {
    /// The wait after the first failure; each further failure doubles it.
    pub initial_backoff: Duration,
    /// The longest wait between attempts. [`FederationSender::new`] sets this to the client's own
    /// `max_retry_backoff` (`hs-config::FederationConfig::max_retry_backoff`), so the two backoffs
    /// an operator can observe on a destination have the same ceiling.
    pub max_backoff: Duration,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(3600),
        }
    }
}

/// The longest a worker sleeps in one go while its destination is backing off, whatever the
/// destination store's `retry_at` says. The store is re-read after each slice: an administrator
/// who resets a destination's backoff (`hs_federation::admin_source`) gets a retry within this
/// long, not at the end of a possibly hour-long wait. Re-reading costs one store lookup and no
/// network traffic (`FederationClient::send` refuses before resolving anything).
pub const BACKOFF_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// The outbound sender. See the module docs. Cheap to share behind an `Arc`; every method takes
/// `&self`.
pub struct FederationSender {
    shared: Arc<Shared>,
    queues: Mutex<HashMap<String, DestinationQueue>>,
}

/// What every destination worker holds on to: everything but the queue map, so that dropping the
/// [`FederationSender`] (which owns the map, and with it every queue's sending half) ends each
/// worker's `recv` loop instead of keeping the sender alive through a reference cycle.
struct Shared {
    client: Arc<FederationClient>,
    own_server_name: String,
    config: SenderConfig,
    /// Transaction IDs are `{started_ms}-{counter}`: unique across restarts (a later start has a
    /// later prefix) and monotonic within one (the counter only grows), which is what the spec
    /// asks of a `txnId` per `(origin, destination)` pair.
    started_ms: u64,
    txn_counter: AtomicU64,
    /// PDUs queued and not yet accepted or dropped, across every destination.
    pending_total: AtomicUsize,
    shut_down: AtomicBool,
}

struct DestinationQueue {
    tx: mpsc::UnboundedSender<Arc<Value>>,
    pending: Arc<AtomicUsize>,
    worker: tokio::task::AbortHandle,
}

/// How one transaction's delivery ended.
enum Delivery {
    /// The destination answered 2xx (per-PDU rejections, if any, have been logged).
    Delivered,
    /// This server's own policy forbids the destination (federation disabled, domain not in the
    /// allowlist, resolved address in a blocked range); retrying cannot change that, so the
    /// transaction was dropped and logged.
    Dropped,
    /// [`FederationSender::shutdown`] was called while retrying.
    ShutDown,
}

impl FederationSender {
    /// A sender for `own_server_name` over `client`, waiting between failed attempts with
    /// [`SenderConfig::default`] capped at the client's own `max_retry_backoff`.
    #[must_use]
    pub fn new(client: Arc<FederationClient>, own_server_name: impl Into<String>) -> Self {
        let max_backoff = client.max_retry_backoff();
        Self::with_config(
            client,
            own_server_name,
            SenderConfig {
                max_backoff,
                ..SenderConfig::default()
            },
        )
    }

    /// [`FederationSender::new`] with an explicit retry policy. Tests use this to make a failing
    /// destination retry in milliseconds rather than seconds.
    #[must_use]
    pub fn with_config(
        client: Arc<FederationClient>,
        own_server_name: impl Into<String>,
        config: SenderConfig,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                client,
                own_server_name: own_server_name.into(),
                config,
                started_ms: now_ms(),
                txn_counter: AtomicU64::new(0),
                pending_total: AtomicUsize::new(0),
                shut_down: AtomicBool::new(false),
            }),
            queues: Mutex::new(HashMap::new()),
        }
    }

    /// The server name every transaction this sender builds carries as `origin`.
    #[must_use]
    pub fn own_server_name(&self) -> &str {
        &self.shared.own_server_name
    }

    /// Queues `pdu` for each server in `destinations` (deduplicated; this server's own name is
    /// always skipped), starting a destination's worker the first time it is named.
    ///
    /// Must be called from within a Tokio runtime, since a new destination's worker is spawned on
    /// the current one; outside a runtime the PDU is logged and dropped rather than panicking.
    /// After [`FederationSender::shutdown`] every call is a logged no-op.
    pub fn enqueue_pdu(&self, destinations: impl IntoIterator<Item = String>, pdu: Value) {
        if self.shared.shut_down.load(Ordering::Acquire) {
            tracing::debug!("outbound federation sender is shut down; dropping a PDU");
            return;
        }
        let pdu = Arc::new(pdu);
        let mut seen = HashSet::new();
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        for destination in destinations {
            if destination.is_empty()
                || destination == self.shared.own_server_name
                || !seen.insert(destination.clone())
            {
                continue;
            }
            let queue = match queues.entry(destination.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    match spawn_worker(&self.shared, &destination) {
                        Some(queue) => slot.insert(queue),
                        None => continue,
                    }
                }
            };
            queue.pending.fetch_add(1, Ordering::AcqRel);
            self.shared.pending_total.fetch_add(1, Ordering::AcqRel);
            if queue.tx.send(pdu.clone()).is_err() {
                // The worker is gone (aborted by `shutdown`, racing this call). Undo the count;
                // the shut-down check at the top makes this a narrow window, not a normal path.
                queue.pending.fetch_sub(1, Ordering::AcqRel);
                self.shared.pending_total.fetch_sub(1, Ordering::AcqRel);
                tracing::debug!(
                    destination,
                    "outbound federation worker is gone; dropping a PDU"
                );
            }
        }
    }

    /// PDUs queued and not yet accepted by (or dropped for) their destination, summed over every
    /// destination. What the admin overview reports as pending.
    #[must_use]
    pub fn pending_pdus(&self) -> usize {
        self.shared.pending_total.load(Ordering::Acquire)
    }

    /// PDUs queued for one destination and not yet accepted or dropped. Zero for a destination
    /// nothing has been queued for.
    #[must_use]
    pub fn pending_pdus_for(&self, destination: &str) -> usize {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(destination)
            .map_or(0, |queue| queue.pending.load(Ordering::Acquire))
    }

    /// Every destination a worker exists for, with its pending count, sorted by name.
    #[must_use]
    pub fn pending_by_destination(&self) -> Vec<(String, usize)> {
        let mut all: Vec<(String, usize)> = self
            .queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, queue)| (name.clone(), queue.pending.load(Ordering::Acquire)))
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all
    }

    /// Stops every worker at once, abandoning whatever is queued (logged at `warn` with the
    /// count, because it is lost -- see the module docs). Idempotent; `enqueue_pdu` is a no-op
    /// afterwards.
    pub fn shutdown(&self) {
        self.shared.shut_down.store(true, Ordering::Release);
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, queue) in queues.drain() {
            queue.worker.abort();
        }
        let lost = self.shared.pending_total.swap(0, Ordering::AcqRel);
        if lost > 0 {
            tracing::warn!(
                lost,
                "outbound federation sender stopped with PDUs still queued; they are lost (the \
                 queue is in memory only)"
            );
        }
    }
}

impl Drop for FederationSender {
    fn drop(&mut self) {
        // A worker mid-retry holds only `Shared`, not this struct, so without this it would keep
        // retrying its current transaction after the sender that owned it was gone.
        for (_, queue) in self
            .queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
        {
            queue.worker.abort();
        }
    }
}

impl OutboundPduSink for FederationSender {
    fn enqueue_pdu(&self, destinations: Vec<String>, pdu: Value) {
        FederationSender::enqueue_pdu(self, destinations, pdu);
    }
}

fn spawn_worker(shared: &Arc<Shared>, destination: &str) -> Option<DestinationQueue> {
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            tracing::error!(
                destination,
                "outbound federation sender used outside a Tokio runtime; dropping a PDU"
            );
            return None;
        }
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let pending = Arc::new(AtomicUsize::new(0));
    let worker = runtime
        .spawn(run_worker(
            shared.clone(),
            destination.to_owned(),
            pending.clone(),
            rx,
        ))
        .abort_handle();
    Some(DestinationQueue {
        tx,
        pending,
        worker,
    })
}

/// One destination's loop: take everything queued (up to a transaction's worth), deliver it,
/// repeat. Ends when the queue's sending half is dropped (the sender was dropped) or on
/// [`Delivery::ShutDown`].
async fn run_worker(
    shared: Arc<Shared>,
    destination: String,
    pending: Arc<AtomicUsize>,
    mut rx: mpsc::UnboundedReceiver<Arc<Value>>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < MAX_PDUS_PER_TRANSACTION {
            match rx.try_recv() {
                Ok(pdu) => batch.push(pdu),
                Err(_) => break,
            }
        }
        let count = batch.len();
        let txn_id = shared.next_txn_id();
        let outcome = shared.deliver(&destination, &txn_id, &batch).await;
        pending.fetch_sub(count, Ordering::AcqRel);
        shared.pending_total.fetch_sub(count, Ordering::AcqRel);
        if let Delivery::ShutDown = outcome {
            return;
        }
    }
}

impl Shared {
    fn next_txn_id(&self) -> String {
        let n = self.txn_counter.fetch_add(1, Ordering::AcqRel) + 1;
        format!("{}-{n}", self.started_ms)
    }

    /// The wait after `failures` consecutive failed attempts at one transaction:
    /// `initial_backoff * 2^(failures - 1)`, capped at `max_backoff`.
    fn backoff(&self, failures: u32) -> Duration {
        let exponent = failures.saturating_sub(1).min(20);
        self.config
            .initial_backoff
            .saturating_mul(1u32 << exponent)
            .min(self.config.max_backoff)
    }

    /// Sends one transaction until the destination accepts it, this server's own policy refuses
    /// it, or the sender is shut down. See the module docs for the retry rules.
    async fn deliver(&self, destination: &str, txn_id: &str, pdus: &[Arc<Value>]) -> Delivery {
        let path = format!("/_matrix/federation/v1/send/{txn_id}");
        let body = serde_json::json!({
            "origin": self.own_server_name,
            "origin_server_ts": now_ms(),
            "pdus": pdus.iter().map(|pdu| (**pdu).clone()).collect::<Vec<Value>>(),
            "edus": [],
        });
        let mut failures: u32 = 0;
        loop {
            if self.shut_down.load(Ordering::Acquire) {
                return Delivery::ShutDown;
            }
            let delay = match self
                .client
                .send(destination, "PUT", &path, Some(&body))
                .await
            {
                Ok(response) if (200..300).contains(&response.status) => {
                    self.log_rejections(destination, txn_id, &response.body);
                    tracing::debug!(
                        destination,
                        txn_id,
                        pdus = pdus.len(),
                        "federation transaction accepted"
                    );
                    return Delivery::Delivered;
                }
                Ok(response) => {
                    failures += 1;
                    let delay = self.backoff(failures);
                    tracing::warn!(
                        destination,
                        txn_id,
                        status = response.status,
                        failures,
                        retry_in_ms = delay.as_millis() as u64,
                        "federation transaction rejected; will retry"
                    );
                    delay
                }
                Err(ClientError::Backoff { retry_at_ms, .. }) => {
                    // The destination store's judgement, not this loop's: wait it out (in
                    // slices, so a reset is noticed) without counting it as another failure of
                    // this transaction.
                    let remaining = Duration::from_millis(retry_at_ms.saturating_sub(now_ms()));
                    let floor = self.config.initial_backoff;
                    let ceiling = BACKOFF_POLL_INTERVAL.max(floor);
                    let delay = remaining.max(floor).min(ceiling);
                    tracing::debug!(
                        destination,
                        txn_id,
                        retry_at_ms,
                        sleeping_ms = delay.as_millis() as u64,
                        "destination is backing off; waiting"
                    );
                    delay
                }
                Err(
                    error @ (ClientError::Disabled
                    | ClientError::DomainDenied(_)
                    | ClientError::IpDenied(_)),
                ) => {
                    tracing::error!(
                        destination,
                        txn_id,
                        pdus = pdus.len(),
                        %error,
                        "federation transaction dropped: this server's own policy forbids the \
                         destination"
                    );
                    return Delivery::Dropped;
                }
                Err(error) => {
                    failures += 1;
                    let delay = self.backoff(failures);
                    tracing::warn!(
                        destination,
                        txn_id,
                        failures,
                        %error,
                        retry_in_ms = delay.as_millis() as u64,
                        "federation transaction failed; will retry"
                    );
                    delay
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// Logs every per-PDU `error` in an accepted transaction's response. Final, not retried: the
    /// receiver examined the event and refused it.
    fn log_rejections(&self, destination: &str, txn_id: &str, response: &Value) {
        let Some(results) = response.get("pdus").and_then(Value::as_object) else {
            return;
        };
        for (event_id, result) in results {
            if let Some(error) = result.get("error").and_then(Value::as_str)
                && !error.is_empty()
            {
                tracing::warn!(
                    destination,
                    txn_id,
                    event_id,
                    error,
                    "destination rejected a PDU; it will not be sent to that server again"
                );
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientConfig;
    use crate::destination_store::{DestinationState, DestinationStore, InMemoryDestinationStore};
    use crate::discovery::{AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome};
    use async_trait::async_trait;
    use axum::extract::{Request, State};
    use axum::middleware::Next;
    use axum::response::Response;
    use hs_model::signing::SigningKeyPair;
    use hs_testkit::fake_federation::{CannedResponse, FakeFederationPeer};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;
    use tokio::net::TcpListener;

    /// Answers every hostname with `127.0.0.1` and no SRV records, so an explicit-port
    /// destination (`localhost:{port}`) reaches a fake peer bound on loopback -- the same shape
    /// `crate::client` and `crate::outbound_join`'s tests use.
    struct Loopback;
    #[async_trait]
    impl AddrResolver for Loopback {
        async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        }
    }
    #[async_trait]
    impl SrvResolver for Loopback {
        async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
            Vec::new()
        }
    }
    struct NoWellKnown;
    #[async_trait]
    impl WellKnownFetcher for NoWellKnown {
        async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
            WellKnownOutcome::Absent {
                cache_for: Duration::from_secs(60),
            }
        }
    }

    const US: &str = "us.example.org";

    fn client_with(destinations: Arc<dyn DestinationStore>) -> Arc<FederationClient> {
        Arc::new(FederationClient::new(
            US,
            SigningKeyPair::generate("a_1"),
            ClientConfig {
                scheme: "http",
                ..ClientConfig::default()
            },
            destinations,
            Arc::new(NoWellKnown),
            Arc::new(Loopback),
            Arc::new(Loopback),
        ))
    }

    fn client() -> Arc<FederationClient> {
        client_with(Arc::new(InMemoryDestinationStore::new()))
    }

    fn fast() -> SenderConfig {
        SenderConfig {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
        }
    }

    type AuthLog = Arc<Mutex<Vec<Option<String>>>>;

    /// `FakeFederationPeer` records method, path and body but not headers; this layer in front
    /// of it keeps every request's `Authorization` header so the tests can see the `X-Matrix`
    /// signature went out.
    async fn record_authorization(
        State(log): State<AuthLog>,
        request: Request,
        next: Next,
    ) -> Response {
        let header = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        log.lock().unwrap().push(header);
        next.run(request).await
    }

    /// Binds `peer` on a fresh loopback port; returns the destination string a sender should use
    /// for it and the recorded `Authorization` headers.
    async fn spawn_peer(peer: &FakeFederationPeer) -> (String, AuthLog) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: AuthLog = Arc::new(Mutex::new(Vec::new()));
        let app = peer.router().layer(axum::middleware::from_fn_with_state(
            log.clone(),
            record_authorization,
        ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("localhost:{port}"), log)
    }

    /// Polls `condition` every few milliseconds until it holds or `deadline` passes.
    async fn wait_for(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while !condition() {
            if start.elapsed() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }

    fn pdu(i: usize) -> Value {
        serde_json::json!({ "type": "m.room.message", "room_id": "!r:us.example.org", "i": i })
    }

    fn txn_id_of(path: &str) -> &str {
        path.rsplit('/').next().unwrap()
    }

    #[tokio::test]
    async fn three_pdus_go_out_as_one_signed_transaction() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        for i in 0..3 {
            sender.enqueue_pdu([destination.clone()], pdu(i));
        }
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 1).await);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);

        let requests = peer.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let request = &requests[0];
        assert_eq!(request.method, "PUT");
        assert!(
            request.path.starts_with("/_matrix/federation/v1/send/"),
            "{}",
            request.path
        );
        assert_eq!(request.body["origin"], US);
        assert!(request.body["origin_server_ts"].as_u64().unwrap() > 0);
        let pdus = request.body["pdus"].as_array().unwrap();
        assert_eq!(pdus.len(), 3);
        for (i, pdu) in pdus.iter().enumerate() {
            assert_eq!(pdu["i"], i);
        }
        assert_eq!(request.body["edus"], serde_json::json!([]));

        let headers = auth.lock().unwrap().clone();
        assert_eq!(headers.len(), 1);
        let header = headers[0].as_deref().expect("an Authorization header");
        assert!(header.starts_with("X-Matrix "), "{header}");
        assert!(header.contains(&format!("origin=\"{US}\"")), "{header}");
    }

    #[tokio::test]
    async fn sixty_pdus_split_into_transactions_of_fifty_then_ten_in_order() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        // A current-thread runtime (`#[tokio::test]`'s default) cannot run the worker until this
        // task yields, so all sixty are queued before the first transaction is built.
        for i in 0..60 {
            sender.enqueue_pdu([destination.clone()], pdu(i));
        }
        assert_eq!(sender.pending_pdus(), 60);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);

        let requests = peer.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        let first = requests[0].body["pdus"].as_array().unwrap();
        let second = requests[1].body["pdus"].as_array().unwrap();
        assert_eq!(first.len(), MAX_PDUS_PER_TRANSACTION);
        assert_eq!(second.len(), 10);
        let order: Vec<u64> = first
            .iter()
            .chain(second.iter())
            .map(|p| p["i"].as_u64().unwrap())
            .collect();
        assert_eq!(order, (0..60).collect::<Vec<u64>>());
        assert_ne!(
            txn_id_of(&requests[0].path),
            txn_id_of(&requests[1].path),
            "transaction IDs must differ"
        );
        assert!(txn_id_of(&requests[0].path) < txn_id_of(&requests[1].path));
    }

    #[tokio::test]
    async fn a_failing_destination_is_retried_in_order_after_waiting() {
        let peer = FakeFederationPeer::new("peer.example.org");
        peer.queue_response(CannedResponse::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"errcode": "M_UNKNOWN"}),
        ));
        peer.queue_response(CannedResponse::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"errcode": "M_UNKNOWN"}),
        ));
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        let started = Instant::now();
        sender.enqueue_pdu([destination.clone()], pdu(0));
        sender.enqueue_pdu([destination.clone()], pdu(1));
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        let elapsed = started.elapsed();

        let requests = peer.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        // The same transaction, same ID, until accepted: a receiver that did process an earlier
        // attempt replays its cached answer rather than applying the PDUs twice.
        assert_eq!(requests[0].path, requests[1].path);
        assert_eq!(requests[1].path, requests[2].path);
        assert_eq!(requests[0].body, requests[2].body);
        let pdus = requests[2].body["pdus"].as_array().unwrap();
        assert_eq!(pdus.len(), 2);
        assert_eq!(pdus[0]["i"], 0);
        assert_eq!(pdus[1]["i"], 1);
        // Waited, not spun: 100ms after the first failure, 200ms after the second.
        assert!(
            elapsed >= Duration::from_millis(300),
            "third attempt came too soon: {elapsed:?}"
        );
        // And once accepted, nothing more.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer.request_count(), 3);
    }

    #[tokio::test]
    async fn destinations_are_served_independently() {
        let slow = FakeFederationPeer::new("slow.example.org");
        slow.queue_response(CannedResponse::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({}),
        ));
        let quick = FakeFederationPeer::new("quick.example.org");
        let (slow_dest, _) = spawn_peer(&slow).await;
        let (quick_dest, _) = spawn_peer(&quick).await;
        let sender = FederationSender::with_config(
            client(),
            US,
            SenderConfig {
                initial_backoff: Duration::from_millis(500),
                max_backoff: Duration::from_secs(1),
            },
        );

        sender.enqueue_pdu([slow_dest.clone()], pdu(1));
        sender.enqueue_pdu([quick_dest.clone()], pdu(2));

        // The quick destination is done while the slow one is still waiting out its first
        // failure: one queue's trouble is not the other's.
        assert!(
            wait_for(Duration::from_secs(10), || sender
                .pending_pdus_for(&quick_dest)
                == 0)
            .await
        );
        assert_eq!(sender.pending_pdus_for(&slow_dest), 1);
        assert_eq!(quick.request_count(), 1);

        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        let slow_requests = slow.requests();
        assert_eq!(slow_requests.len(), 2, "{slow_requests:?}");
        assert_eq!(slow_requests[1].body["pdus"][0]["i"], 1);
        let quick_requests = quick.requests();
        assert_eq!(quick_requests.len(), 1, "{quick_requests:?}");
        assert_eq!(quick_requests[0].body["pdus"][0]["i"], 2);
        let mut expected = vec![(quick_dest.clone(), 0), (slow_dest.clone(), 0)];
        expected.sort();
        assert_eq!(sender.pending_by_destination(), expected);
    }

    #[tokio::test]
    async fn nothing_is_ever_sent_to_our_own_server_name() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        // Our own name alone: no queue, no worker, nothing pending.
        sender.enqueue_pdu([US.to_owned()], pdu(0));
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());

        // Our own name mixed in with (and duplicated alongside) a real destination: exactly one
        // copy goes to the real one and none anywhere else.
        sender.enqueue_pdu(
            [
                US.to_owned(),
                destination.clone(),
                US.to_owned(),
                destination.clone(),
            ],
            pdu(1),
        );
        assert_eq!(sender.pending_pdus(), 1);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        assert_eq!(
            sender.pending_by_destination(),
            vec![(destination.clone(), 0)]
        );
        let requests = peer.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].body["pdus"].as_array().unwrap().len(), 1);
    }

    /// A destination store whose answer for one destination is "not before `retry_at_ms`", so
    /// the test can assert the worker honoured it without depending on the real store's jitter.
    struct FixedBackoff {
        destination: String,
        retry_at_ms: u64,
        inner: InMemoryDestinationStore,
    }
    #[async_trait]
    impl DestinationStore for FixedBackoff {
        async fn get(&self, destination: &str) -> DestinationState {
            if destination == self.destination && now_ms() < self.retry_at_ms {
                return DestinationState {
                    failure_count: 1,
                    retry_at_ms: Some(self.retry_at_ms),
                    ..DestinationState::default()
                };
            }
            self.inner.get(destination).await
        }
        async fn record_failure(&self, destination: &str, max_backoff_ms: u64) {
            self.inner.record_failure(destination, max_backoff_ms).await;
        }
        async fn record_success(&self, destination: &str) {
            self.inner.record_success(destination).await;
        }
        async fn list(&self) -> Vec<(String, DestinationState)> {
            self.inner.list().await
        }
        async fn reset(&self, destination: &str) {
            self.inner.reset(destination).await;
        }
    }

    #[tokio::test]
    async fn a_destination_in_backoff_is_not_contacted_before_its_retry_time() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let retry_at_ms = now_ms() + 400;
        let store = Arc::new(FixedBackoff {
            destination: destination.clone(),
            retry_at_ms,
            inner: InMemoryDestinationStore::new(),
        });
        let sender = FederationSender::with_config(
            client_with(store),
            US,
            SenderConfig {
                initial_backoff: Duration::from_millis(50),
                max_backoff: Duration::from_secs(1),
            },
        );

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 1).await);
        assert!(
            now_ms() >= retry_at_ms,
            "the destination was contacted before its retry time"
        );
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        assert_eq!(peer.request_count(), 1);
    }

    #[tokio::test]
    async fn a_per_pdu_rejection_is_final_not_retried() {
        let peer = FakeFederationPeer::new("peer.example.org");
        peer.queue_response(CannedResponse::ok(serde_json::json!({
            "pdus": { "$rejected": { "error": "event failed authorization" }, "$fine": {} }
        })));
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer.request_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_stops_the_workers_and_drops_the_queue() {
        // A port nothing listens on: every attempt fails at connect and the worker would retry
        // for as long as it lived.
        let unused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let destination = format!("localhost:{}", unused.local_addr().unwrap().port());
        drop(unused);
        let sender = FederationSender::with_config(client(), US, fast());

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert_eq!(sender.pending_pdus(), 1);
        sender.shutdown();
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());

        // Queuing after shutdown is a no-op, not a new worker.
        sender.enqueue_pdu([destination.clone()], pdu(1));
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());
    }

    #[test]
    fn backoff_doubles_from_the_initial_delay_and_is_capped() {
        let shared = Shared {
            client: client(),
            own_server_name: US.to_owned(),
            config: SenderConfig {
                initial_backoff: Duration::from_millis(100),
                max_backoff: Duration::from_millis(1000),
            },
            started_ms: 0,
            txn_counter: AtomicU64::new(0),
            pending_total: AtomicUsize::new(0),
            shut_down: AtomicBool::new(false),
        };
        assert_eq!(shared.backoff(1), Duration::from_millis(100));
        assert_eq!(shared.backoff(2), Duration::from_millis(200));
        assert_eq!(shared.backoff(4), Duration::from_millis(800));
        assert_eq!(shared.backoff(5), Duration::from_millis(1000));
        assert_eq!(shared.backoff(40), Duration::from_millis(1000));
        assert_eq!(shared.next_txn_id(), "0-1");
        assert_eq!(shared.next_txn_id(), "0-2");
    }
}
