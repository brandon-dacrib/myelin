//! EDU metrics, registered into the shared `hs_telemetry::metrics::Metrics` registry (by
//! `hs-cli`, through `Metrics::with_registry`) per `docs/decisions/0004-telemetry-conventions.md`:
//!
//! - `hs_federation_edus_sent_total{edu_type}`: EDUs in transactions a destination accepted,
//!   counted by [`crate::sender::FederationSender`] once the destination answers `200`. An EDU
//!   dropped before that (a full queue, a destination this server's policy forbids) is not
//!   counted.
//! - `hs_federation_edus_received_total{edu_type,outcome}`: EDUs that arrived in `/send`,
//!   counted by whoever applies them (`hs-cli`'s dispatcher), with what became of each:
//!   `applied`, `duplicate` (a to-device `message_id` already delivered) or `dropped` (malformed,
//!   or speaking for a user of another server, or of a type this server does not handle).
//! - `hs_federation_edus_forwarded_total{edu_type,outcome}`: in a cluster, EDUs for a
//!   destination another replica sends for. On the replica that took the request: `forwarded`
//!   (the owning replica took it over the mesh), `failed` (it could not be reached or refused
//!   it) or `dropped` (no forwarder installed); on the owning replica: `received` (a peer
//!   forwarded it and it was queued here).
//!
//! `edu_type` is bounded: an EDU type that is not one of [`KNOWN_EDU_TYPES`] is counted as
//! `other`, since the type is whatever a remote server wrote.
//!
//! The transport's own counters (server ACL refusals, notary queries) are process-wide and
//! registered by [`register_transport_metrics`].

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// The EDU types counted under their own name; anything else is `other`.
pub const KNOWN_EDU_TYPES: &[&str] = &[
    "m.typing",
    "m.receipt",
    "m.presence",
    "m.device_list_update",
    "m.signing_key_update",
    "m.direct_to_device",
];

/// Labels of `hs_federation_edus_sent_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct EduSentLabels {
    /// The EDU type, or `other`.
    pub edu_type: String,
}

/// Labels of `hs_federation_edus_received_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct EduReceivedLabels {
    /// The EDU type, or `other`.
    pub edu_type: String,
    /// `applied`, `duplicate` or `dropped`.
    pub outcome: String,
}

/// Labels of `hs_federation_edus_forwarded_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct EduForwardedLabels {
    /// The EDU type, or `other`.
    pub edu_type: String,
    /// `forwarded`, `failed`, `dropped` or `received`.
    pub outcome: String,
}

/// What became of an EDU for a destination another replica sends for, for
/// [`EduMetrics::record_forwarded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EduForwardOutcome {
    /// The owning replica took it.
    Forwarded,
    /// The owning replica could not be reached, or refused it.
    Failed,
    /// Nothing to forward it with (no mesh): dropped.
    Dropped,
    /// Counted by the owning replica: a peer forwarded it, and it was queued here.
    Received,
}

impl EduForwardOutcome {
    /// The label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
            Self::Received => "received",
        }
    }
}

/// What became of a received EDU, for [`EduMetrics::record_received`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EduOutcome {
    /// Applied: it reached `/sync`, the device-list stream or a device's inbox.
    Applied,
    /// A to-device message whose `message_id` was delivered before.
    Duplicate,
    /// Malformed, speaking for a user of another server, or of a type not handled here.
    Dropped,
}

impl EduOutcome {
    /// The label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Duplicate => "duplicate",
            Self::Dropped => "dropped",
        }
    }
}

/// The EDU metric families. Cheap to clone; every clone counts into the same families.
#[derive(Clone, Default)]
pub struct EduMetrics {
    /// `hs_federation_edus_sent_total{edu_type}`.
    pub sent_total: Family<EduSentLabels, Counter>,
    /// `hs_federation_edus_received_total{edu_type,outcome}`.
    pub received_total: Family<EduReceivedLabels, Counter>,
    /// `hs_federation_edus_forwarded_total{edu_type,outcome}`.
    pub forwarded_total: Family<EduForwardedLabels, Counter>,
}

impl EduMetrics {
    /// Registers the families into `registry` (the shared one, in `hs serve`).
    #[must_use]
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self::default();
        // Registered without `_total`: the text encoder appends it to every counter
        // (`hs_telemetry::metrics`' module docs).
        registry.register(
            "hs_federation_edus_sent",
            "EDUs in federation transactions a destination accepted, by EDU type",
            metrics.sent_total.clone(),
        );
        registry.register(
            "hs_federation_edus_received",
            "EDUs received over federation, by EDU type and outcome (applied, duplicate, dropped)",
            metrics.received_total.clone(),
        );
        registry.register(
            "hs_federation_edus_forwarded",
            "EDUs for a destination another replica sends for, by EDU type and outcome \
             (forwarded, failed, dropped; received on the replica that sends)",
            metrics.forwarded_total.clone(),
        );
        metrics
    }

    /// Counts one EDU of `edu_type` for a destination another replica sends for, with what
    /// became of it.
    pub fn record_forwarded(&self, edu_type: &str, outcome: EduForwardOutcome) {
        self.forwarded_total
            .get_or_create(&EduForwardedLabels {
                edu_type: bounded(edu_type).to_owned(),
                outcome: outcome.as_str().to_owned(),
            })
            .inc();
    }

    /// How many EDUs of `edu_type` have been counted with forwarding `outcome`.
    #[must_use]
    pub fn forwarded(&self, edu_type: &str, outcome: EduForwardOutcome) -> u64 {
        self.forwarded_total
            .get_or_create(&EduForwardedLabels {
                edu_type: bounded(edu_type).to_owned(),
                outcome: outcome.as_str().to_owned(),
            })
            .get()
    }

    /// Counts one EDU of `edu_type` sent.
    pub fn record_sent(&self, edu_type: &str) {
        self.sent_total
            .get_or_create(&EduSentLabels {
                edu_type: bounded(edu_type).to_owned(),
            })
            .inc();
    }

    /// Counts one EDU of `edu_type` received, with its outcome.
    pub fn record_received(&self, edu_type: &str, outcome: EduOutcome) {
        self.received_total
            .get_or_create(&EduReceivedLabels {
                edu_type: bounded(edu_type).to_owned(),
                outcome: outcome.as_str().to_owned(),
            })
            .inc();
    }

    /// How many EDUs of `edu_type` have been counted as sent.
    #[must_use]
    pub fn sent(&self, edu_type: &str) -> u64 {
        self.sent_total
            .get_or_create(&EduSentLabels {
                edu_type: bounded(edu_type).to_owned(),
            })
            .get()
    }

    /// How many EDUs of `edu_type` have been counted as received with `outcome`.
    #[must_use]
    pub fn received(&self, edu_type: &str, outcome: EduOutcome) -> u64 {
        self.received_total
            .get_or_create(&EduReceivedLabels {
                edu_type: bounded(edu_type).to_owned(),
                outcome: outcome.as_str().to_owned(),
            })
            .get()
    }
}

/// Labels of `hs_federation_catch_up_started_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct CatchUpStartedLabels {
    /// `queue_full` or `requested` (`crate::outbound_store::CatchUpMark::reason`).
    pub reason: String,
}

/// The catch-up metric families (`crate::sender`'s module docs, "Catch-up"). Not labelled by
/// destination: there can be as many destinations as servers in the federation. The admin API's
/// destination list says which destination is catching up (`catch_up_since`).
///
/// - `hs_federation_catch_up_started_total{reason}`: destinations put in catch-up mode.
/// - `hs_federation_catch_up_completed_total`: destinations caught up and back on their queue.
/// - `hs_federation_catch_up_rooms_total`: rooms whose latest event catch-up sent.
/// - `hs_federation_outbound_pdus_dropped_total`: queued PDUs dropped because their destination
///   was caught up from the rooms instead.
#[derive(Clone, Default)]
pub struct CatchUpMetrics {
    /// `hs_federation_catch_up_started_total{reason}`.
    pub started_total: Family<CatchUpStartedLabels, Counter>,
    /// `hs_federation_catch_up_completed_total`.
    pub completed_total: Counter,
    /// `hs_federation_catch_up_rooms_total`.
    pub rooms_total: Counter,
    /// `hs_federation_outbound_pdus_dropped_total`.
    pub dropped_total: Counter,
}

impl CatchUpMetrics {
    /// Registers the families into `registry` (the shared one, in `hs serve`).
    #[must_use]
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self::default();
        registry.register(
            "hs_federation_catch_up_started",
            "Destinations put in catch-up mode, by reason (queue_full, requested)",
            metrics.started_total.clone(),
        );
        registry.register(
            "hs_federation_catch_up_completed",
            "Destinations caught up from the rooms and returned to their queue",
            metrics.completed_total.clone(),
        );
        registry.register(
            "hs_federation_catch_up_rooms",
            "Rooms whose latest event was sent to a destination being caught up",
            metrics.rooms_total.clone(),
        );
        registry.register(
            "hs_federation_outbound_pdus_dropped",
            "Queued PDUs dropped because their destination was caught up from the rooms instead",
            metrics.dropped_total.clone(),
        );
        metrics
    }

    /// Counts a destination put in catch-up mode for `reason`.
    pub fn record_started(&self, reason: &str) {
        self.started_total
            .get_or_create(&CatchUpStartedLabels {
                reason: reason.to_owned(),
            })
            .inc();
    }

    /// How many destinations were put in catch-up mode for `reason`.
    #[must_use]
    pub fn started(&self, reason: &str) -> u64 {
        self.started_total
            .get_or_create(&CatchUpStartedLabels {
                reason: reason.to_owned(),
            })
            .get()
    }

    /// Counts a destination caught up.
    pub fn record_completed(&self) {
        self.completed_total.inc();
    }

    /// Counts `n` rooms whose latest event was sent.
    pub fn record_rooms(&self, n: u64) {
        self.rooms_total.inc_by(n);
    }

    /// Counts `n` queued PDUs dropped.
    pub fn record_dropped(&self, n: u64) {
        self.dropped_total.inc_by(n);
    }
}

/// Labels of `hs_federation_acl_refusals_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct AclRefusalLabels {
    /// The federation endpoint refused (`send`, `make_join`, `state_ids`, ...): one of a fixed
    /// set this crate names, never caller input.
    pub endpoint: &'static str,
}

/// Labels of `hs_federation_notary_queries_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct NotaryLabels {
    /// `answered` (some key was held or fetched for the server asked about) or `none`.
    pub outcome: &'static str,
}

/// Process-wide, like the other transport-side counters: the handlers that count into them are
/// built per mount, and a counter is only an atomic. [`register_transport_metrics`] puts them in
/// a server's registry.
static ACL_REFUSALS: std::sync::LazyLock<Family<AclRefusalLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);
static NOTARY_QUERIES: std::sync::LazyLock<Family<NotaryLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);
static STATE_FALLBACKS: std::sync::LazyLock<Family<StateFallbackLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);
static PDUS_DROPPED: std::sync::LazyLock<Family<PduDroppedLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);

/// Labels of `hs_federation_pdus_dropped_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct PduDroppedLabels {
    /// `missing_ancestors` (prev events the sending server would not, or could not, supply),
    /// `missing_auth_events` (auth events that could not be fetched or judged), `not_in_room`
    /// (no user of this server is joined to the room: Synapse's "Ignoring PDU ... as we're not
    /// in the room") or `unknown_room` (a room this server has never held).
    pub reason: &'static str,
}

/// Labels of `hs_federation_key_fetch_failures_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct KeyFetchFailureLabels {
    /// `timeout` (the key server did not answer within the fetch budget), `unreachable` (the
    /// fetch failed: discovery, connection, a non-200 answer or an unreadable body),
    /// `invalid_response` (what it answered was not a validly self-signed key response for it)
    /// or `backoff` (not asked: an earlier failure is still being backed off from, see
    /// `crate::keys::RemoteKeyCache`).
    pub reason: &'static str,
}

static KEY_FETCH_FAILURES: std::sync::LazyLock<Family<KeyFetchFailureLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);

/// Labels of `hs_federation_destinations_forgotten_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct DestinationForgottenLabels {
    /// `administrator` (`federation.destinations.forget`), or a prune reason: `unused` (shares
    /// no room, nothing queued) or `failing` (failing long enough, queue only for rooms this
    /// server left), whether the administrator's prune or the background sweep decided it.
    pub reason: &'static str,
    /// `administrator` or `sweep`.
    pub by: &'static str,
}

static DESTINATIONS_FORGOTTEN: std::sync::LazyLock<Family<DestinationForgottenLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);

/// Counts one destination forgotten (decision 0042), for `reason` by `by`
/// ([`DestinationForgottenLabels`]).
pub fn record_destination_forgotten(reason: &'static str, by: &'static str) {
    DESTINATIONS_FORGOTTEN
        .get_or_create(&DestinationForgottenLabels { reason, by })
        .inc();
}

/// How many destinations were forgotten for `reason` by `by` so far (for tests).
#[must_use]
pub fn destinations_forgotten(reason: &'static str, by: &'static str) -> u64 {
    DESTINATIONS_FORGOTTEN
        .get_or_create(&DestinationForgottenLabels { reason, by })
        .get()
}

/// `hs_federation_join_verify_seconds`: how long verifying the `state` and `auth_chain` of a
/// `send_join` answer took, per join through another server
/// (`crate::outbound_join::join_room`). The buckets reach 30 minutes: a large public room's
/// snapshot cites thousands of servers, and the time is dominated by the key fetches to the
/// ones that are gone.
static JOIN_VERIFY_SECONDS: std::sync::LazyLock<Histogram> = std::sync::LazyLock::new(|| {
    Histogram::new([
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
    ])
});

/// Counts one failed (or skipped, for `backoff`) fetch of a server's keys, for `reason`
/// ([`KeyFetchFailureLabels`]).
pub fn record_key_fetch_failure(reason: &'static str) {
    KEY_FETCH_FAILURES
        .get_or_create(&KeyFetchFailureLabels { reason })
        .inc();
}

/// How many key fetches failed for `reason` so far (for tests and the admin API).
#[must_use]
pub fn key_fetch_failures(reason: &'static str) -> u64 {
    KEY_FETCH_FAILURES
        .get_or_create(&KeyFetchFailureLabels { reason })
        .get()
}

/// Records how long one join's `send_join` answer took to verify.
pub fn record_join_verify_seconds(seconds: f64) {
    JOIN_VERIFY_SECONDS.observe(seconds);
}

/// Counts one PDU received over `/send` and dropped, for `reason` ([`PduDroppedLabels`]; see
/// `crate::inbound` for what each is answered).
pub fn record_pdu_dropped(reason: &'static str) {
    PDUS_DROPPED
        .get_or_create(&PduDroppedLabels { reason })
        .inc();
}

static PDU_WAKES: std::sync::LazyLock<Family<PduWakeLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);

/// Labels of `hs_federation_pdu_wakes_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct PduWakeLabels {
    /// On the replica that queued the PDU: `sent` (the replica that sends for the destination
    /// took the wake), `failed` (it did not, or the wake could not be encoded; its rescan finds
    /// the PDU), `no_owner` (nobody owns the destination's federation shard right now; whoever
    /// takes it resumes the queue). On the replica woken: `received`, or
    /// `received_not_sent_here` (the shard moved on meanwhile).
    pub outcome: &'static str,
}

/// Counts one wake of the replica that sends for a destination, for a PDU another replica
/// queued (`hs-cli`'s `edu_forward`, `hs_federation::sender::EduForwarder::wake_sender_for`),
/// by `outcome` ([`PduWakeLabels`]).
pub fn record_pdu_wake(outcome: &'static str) {
    PDU_WAKES.get_or_create(&PduWakeLabels { outcome }).inc();
}

/// How many sender wakes ended in `outcome`, in this process ([`record_pdu_wake`]).
#[must_use]
pub fn pdu_wakes(outcome: &'static str) -> u64 {
    PDU_WAKES.get_or_create(&PduWakeLabels { outcome }).get()
}

static DEVICE_LIST_CATCH_UPS: std::sync::LazyLock<Counter> =
    std::sync::LazyLock::new(Counter::default);

/// Counts one catch-up of the device-list announcer (`hs-cli`'s `edus::DeviceListAnnouncer`):
/// federation shards this replica took on whose stored place in the device-list stream was
/// behind, so the changes since were announced to their destinations now.
pub fn record_device_list_catch_up() {
    DEVICE_LIST_CATCH_UPS.inc();
}

/// How many device-list catch-ups this process ran ([`record_device_list_catch_up`]).
#[must_use]
pub fn device_list_catch_ups() -> u64 {
    DEVICE_LIST_CATCH_UPS.get()
}

/// How many pushed PDUs were dropped for `reason`, in this process.
#[must_use]
pub fn pdus_dropped(reason: &'static str) -> u64 {
    PDUS_DROPPED
        .get_or_create(&PduDroppedLabels { reason })
        .get()
}

/// Labels of `hs_federation_state_fallbacks_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct StateFallbackLabels {
    /// One of [`STATE_FALLBACK_OUTCOMES`].
    pub outcome: &'static str,
}

/// The outcomes `hs_federation_state_fallbacks_total` is counted under, one per missing prev
/// event the `/state_ids` fallback (`crate::state_fallback`) tried: `resolved` (held with its
/// state), `rejected` (fetched and authorisation refused it; stored rejected), `no_state`
/// (neither `/state_ids` nor `/state` answered), `no_event` (the prev event itself could not be
/// fetched or did not verify), `refused` (the room could not hold it), `timed_out`.
pub const STATE_FALLBACK_OUTCOMES: [&str; 6] = [
    "resolved",
    "rejected",
    "no_state",
    "no_event",
    "refused",
    "timed_out",
];

/// Counts one missing prev event the `/state_ids` fallback tried, by outcome.
pub fn record_state_fallback(outcome: &'static str) {
    STATE_FALLBACKS
        .get_or_create(&StateFallbackLabels { outcome })
        .inc();
}

/// How many `/state_ids` fallbacks ended in `outcome`, in this process.
#[must_use]
pub fn state_fallbacks(outcome: &'static str) -> u64 {
    STATE_FALLBACKS
        .get_or_create(&StateFallbackLabels { outcome })
        .get()
}

/// Counts one federation request refused because the room's `m.room.server_acl` denies the
/// requesting server (for `/send`, one PDU), by endpoint.
pub fn record_acl_refusal(endpoint: &'static str) {
    ACL_REFUSALS
        .get_or_create(&AclRefusalLabels { endpoint })
        .inc();
}

/// How many requests to `endpoint` the server ACL refused, in this process.
#[must_use]
pub fn acl_refusals(endpoint: &'static str) -> u64 {
    ACL_REFUSALS
        .get_or_create(&AclRefusalLabels { endpoint })
        .get()
}

/// Counts one server asked about through the notary endpoints, by whether anything was answered.
pub fn record_notary_answer(answered: bool) {
    NOTARY_QUERIES
        .get_or_create(&NotaryLabels {
            outcome: if answered { "answered" } else { "none" },
        })
        .inc();
}

/// Registers the transport's counters into `registry` (the shared one, in `hs serve`):
///
/// - `hs_federation_acl_refusals_total{endpoint}`: requests (for `/send`, PDUs) from a server a
///   room's `m.room.server_acl` denies, refused with `403 M_FORBIDDEN`; and, under
///   `endpoint="typing"` and `endpoint="receipt"`, typing notices and rooms' receipts from such a
///   server dropped from a transaction (`crate::acl::filter_edu`).
/// - `hs_federation_notary_queries_total{outcome}`: servers asked about through
///   `/_matrix/key/v2/query`, `answered` or `none` (nothing held and the server unreachable).
/// - `hs_federation_state_fallbacks_total{outcome}`: missing prev events the `/state_ids`
///   fallback tried to take with the state another server answered for them, by outcome
///   ([`STATE_FALLBACK_OUTCOMES`]).
/// - `hs_federation_pdus_dropped_total{reason}`: PDUs received over `/send` and dropped
///   ([`PduDroppedLabels`]): their missing prev events (`missing_ancestors`) or auth events
///   (`missing_auth_events`) could not be obtained, no user of this server is in their room
///   (`not_in_room`), or their room is unknown here (`unknown_room`).
/// - `hs_federation_device_list_catch_ups_total`: federation shards taken on whose device-list
///   announcements were behind ([`record_device_list_catch_up`]).
/// - `hs_federation_pdu_wakes_total{outcome}`: wakes of the replica that sends for a
///   destination, for PDUs another replica queued ([`PduWakeLabels`]).
/// - `hs_federation_key_fetch_failures_total{reason}`: fetches of other servers' signing keys
///   that failed, or were skipped under backoff ([`KeyFetchFailureLabels`]).
/// - `hs_federation_join_verify_seconds`: a histogram of how long each join through another
///   server spent verifying the `send_join` answer ([`record_join_verify_seconds`]).
/// - `hs_federation_transactions_total{outcome}`: attempts at outbound `/send` transactions
///   ([`TRANSACTION_OUTCOMES`]), and `hs_federation_pdus_sent_total`, the PDUs the accepted
///   ones carried ([`record_transaction`], [`record_pdus_sent`]).
pub fn register_transport_metrics(registry: &mut Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_federation_acl_refusals",
        "Federation requests (for /send, PDUs; typing and receipt EDUs) refused because the \
         room's server ACL denies the requesting server, by endpoint",
        ACL_REFUSALS.clone(),
    );
    registry.register(
        "hs_federation_notary_queries",
        "Servers asked about through the notary key query, by outcome (answered, none)",
        NOTARY_QUERIES.clone(),
    );
    registry.register(
        "hs_federation_state_fallbacks",
        "Missing prev events the /state_ids fallback tried to take with the state another \
         server answered for them, by outcome (resolved, rejected, no_state, no_event, refused, \
         timed_out)",
        STATE_FALLBACKS.clone(),
    );
    registry.register(
        "hs_federation_pdus_dropped",
        "PDUs received over /send and dropped: their missing prev events \
         (missing_ancestors) or auth events (missing_auth_events) could not be obtained, no user \
         of this server is joined to their room (not_in_room), or their room is unknown here \
         (unknown_room)",
        PDUS_DROPPED.clone(),
    );
    registry.register(
        "hs_federation_pdu_wakes",
        "Wakes of the replica that sends for a destination, for PDUs another replica queued, \
         by outcome (sent, failed, no_owner on the replica that queued; received, \
         received_not_sent_here on the replica woken)",
        PDU_WAKES.clone(),
    );
    registry.register(
        "hs_federation_key_fetch_failures",
        "Fetches of other servers' signing keys that failed, by reason (timeout, unreachable, \
         invalid_response) or were not made because an earlier failure is still backed off \
         from (backoff)",
        KEY_FETCH_FAILURES.clone(),
    );
    registry.register(
        "hs_federation_destinations_forgotten",
        "Outbound federation destinations forgotten (queue, backoff, catch-up mark and cached \
         keys dropped), by reason (administrator, unused, failing) and by who decided \
         (administrator, sweep)",
        DESTINATIONS_FORGOTTEN.clone(),
    );
    registry.register(
        "hs_federation_join_verify_seconds",
        "Seconds spent verifying the state and auth chain a send_join answer carried, per join \
         through another server",
        JOIN_VERIFY_SECONDS.clone(),
    );
    registry.register(
        "hs_federation_device_list_catch_ups",
        "Federation shards taken on whose device-list announcements were behind, and the \
         changes since announced to their destinations",
        DEVICE_LIST_CATCH_UPS.clone(),
    );
    registry.register(
        "hs_federation_transactions",
        "Attempts at outbound /send transactions, by outcome (accepted, rejected, unresolvable, \
         failed, deferred, dropped)",
        TRANSACTIONS.clone(),
    );
    registry.register(
        "hs_federation_pdus_sent",
        "PDUs carried by outbound transactions their destination accepted",
        PDUS_SENT.clone(),
    );
    // A labelled family with nothing observed renders nothing at all: every known label is
    // created now, so each series is on /metrics at zero from the first scrape.
    for outcome in TRANSACTION_OUTCOMES {
        let _ = TRANSACTIONS.get_or_create(&TransactionLabels { outcome });
    }
    for reason in KEY_FETCH_FAILURE_REASONS {
        let _ = KEY_FETCH_FAILURES.get_or_create(&KeyFetchFailureLabels { reason });
    }
}

/// Labels of `hs_federation_transactions_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct TransactionLabels {
    /// One of [`TRANSACTION_OUTCOMES`].
    pub outcome: &'static str,
}

/// The outcomes `hs_federation_transactions_total` is counted under, one per attempt
/// `crate::sender` makes at a `PUT /send` transaction: `accepted` (the destination answered
/// 2xx), `rejected` (any other status; retried), `unresolvable` (the destination did not resolve
/// to an address; retried under the same backoff as any other failure), `failed` (the request
/// could not be made or answered: connection, TLS, timeout, an unreadable body; retried),
/// `deferred` (not attempted: the client's own backoff for the destination had not ended) and
/// `dropped` (this server's own policy forbids the destination; not retried). Every label is
/// rendered from the start, at zero, so a dashboard can tell "none" from "not wired".
pub const TRANSACTION_OUTCOMES: [&str; 6] = [
    "accepted",
    "rejected",
    "unresolvable",
    "failed",
    "deferred",
    "dropped",
];

/// The reasons `hs_federation_key_fetch_failures_total` is counted under
/// ([`KeyFetchFailureLabels`]), rendered at zero from the start like [`TRANSACTION_OUTCOMES`].
pub const KEY_FETCH_FAILURE_REASONS: [&str; 4] =
    ["timeout", "unreachable", "invalid_response", "backoff"];

static TRANSACTIONS: std::sync::LazyLock<Family<TransactionLabels, Counter>> =
    std::sync::LazyLock::new(Family::default);
static PDUS_SENT: std::sync::LazyLock<Counter> = std::sync::LazyLock::new(Counter::default);

/// Counts one attempt at an outbound transaction, by `outcome` ([`TRANSACTION_OUTCOMES`]).
pub fn record_transaction(outcome: &'static str) {
    TRANSACTIONS
        .get_or_create(&TransactionLabels { outcome })
        .inc();
}

/// How many transaction attempts ended in `outcome`, in this process ([`record_transaction`]).
#[must_use]
pub fn transactions(outcome: &'static str) -> u64 {
    TRANSACTIONS
        .get_or_create(&TransactionLabels { outcome })
        .get()
}

/// Counts `n` PDUs carried by a transaction a destination accepted
/// (`hs_federation_pdus_sent_total`).
pub fn record_pdus_sent(n: u64) {
    PDUS_SENT.inc_by(n);
}

/// How many PDUs destinations accepted in transactions from this process ([`record_pdus_sent`]).
#[must_use]
pub fn pdus_sent() -> u64 {
    PDUS_SENT.get()
}

/// The `hs_federation_sender_*` gauges, read from a [`crate::sender::FederationSender`] at every
/// scrape ([`crate::sender::FederationSender::snapshot`]):
///
/// - `hs_federation_sender_destinations`: destinations this process has a worker for.
/// - `hs_federation_sender_pdus_pending`: PDUs queued for them and not yet accepted or dropped.
/// - `hs_federation_sender_edus_queued`: in-memory EDUs (typing, receipts, presence) waiting.
/// - `hs_federation_sender_destinations_backing_off`: workers waiting out a retry right now.
/// - `hs_federation_sender_state_bytes`: an estimate of the memory the per-destination state
///   takes (queue structures, channel entries, in-memory EDUs).
///
/// Registered by `hs serve` with [`register_sender_gauges`]. What the 2026-10-10 leak hunt
/// lacked: whether the sender's own state was what grew.
pub struct SenderGauges {
    sender: std::sync::Arc<crate::sender::FederationSender>,
}

impl std::fmt::Debug for SenderGauges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderGauges").finish_non_exhaustive()
    }
}

impl prometheus_client::collector::Collector for SenderGauges {
    fn encode(
        &self,
        mut encoder: prometheus_client::encoding::DescriptorEncoder,
    ) -> Result<(), std::fmt::Error> {
        let snapshot = self.sender.snapshot();
        let gauges: [(&str, &str, usize); 5] = [
            (
                "hs_federation_sender_destinations",
                "Destinations the outbound federation sender has a worker for in this process",
                snapshot.destinations,
            ),
            (
                "hs_federation_sender_pdus_pending",
                "PDUs queued for destinations this process sends for and not yet accepted or \
                 dropped",
                snapshot.pdus_pending,
            ),
            (
                "hs_federation_sender_edus_queued",
                "In-memory EDUs (typing, receipts, presence) waiting for a transaction, across \
                 destinations",
                snapshot.edus_queued,
            ),
            (
                "hs_federation_sender_destinations_backing_off",
                "Destination workers waiting out a retry backoff right now",
                snapshot.destinations_backing_off,
            ),
            (
                "hs_federation_sender_state_bytes",
                "Estimated bytes of per-destination sender state held in memory (queues, \
                 channel entries, in-memory EDUs)",
                snapshot.state_bytes,
            ),
        ];
        for (name, help, value) in gauges {
            encoder
                .encode_descriptor(
                    name,
                    help,
                    None,
                    prometheus_client::metrics::MetricType::Gauge,
                )?
                .encode_gauge(&i64::try_from(value).unwrap_or(i64::MAX))?;
        }
        Ok(())
    }
}

/// Registers the [`SenderGauges`] of `sender` on `registry`.
pub fn register_sender_gauges(
    registry: &mut Registry,
    sender: std::sync::Arc<crate::sender::FederationSender>,
) {
    registry.register_collector(Box::new(SenderGauges { sender }));
}

/// `edu_type` if it is one of [`KNOWN_EDU_TYPES`], `other` if not.
fn bounded(edu_type: &str) -> &str {
    KNOWN_EDU_TYPES
        .iter()
        .copied()
        .find(|known| *known == edu_type)
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edus_are_counted_by_type_and_an_unknown_type_is_other() {
        let mut registry = Registry::default();
        let metrics = EduMetrics::register(&mut registry);
        metrics.record_sent("m.direct_to_device");
        metrics.record_sent("m.direct_to_device");
        metrics.record_received("m.signing_key_update", EduOutcome::Applied);
        metrics.record_received("org.example.whatever", EduOutcome::Dropped);
        assert_eq!(metrics.sent("m.direct_to_device"), 2);
        assert_eq!(
            metrics.received("m.signing_key_update", EduOutcome::Applied),
            1
        );
        assert_eq!(metrics.received("other", EduOutcome::Dropped), 1);

        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        assert!(
            text.contains(r#"hs_federation_edus_sent_total{edu_type="m.direct_to_device"} 2"#),
            "{text}"
        );
        assert!(
            text.contains(
                r#"hs_federation_edus_received_total{edu_type="other",outcome="dropped"} 1"#
            ),
            "{text}"
        );
        assert!(!text.contains("_total_total"), "{text}");
    }

    /// Every outcome of `hs_federation_transactions_total` and every reason of
    /// `hs_federation_key_fetch_failures_total` is on `/metrics` from registration, at zero,
    /// and `hs_federation_pdus_sent_total` with them: a labelled family with nothing observed
    /// would otherwise render nothing, and a dashboard could not tell "none" from "not wired"
    /// (what the 2026-10-10 demo's zeros turned out to be).
    #[test]
    fn transaction_outcomes_render_from_registration_and_count() {
        let mut registry = Registry::default();
        register_transport_metrics(&mut registry);
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        for outcome in TRANSACTION_OUTCOMES {
            let series = format!("hs_federation_transactions_total{{outcome=\"{outcome}\"}} ");
            assert!(text.contains(&series), "missing {series} in {text}");
        }
        for reason in KEY_FETCH_FAILURE_REASONS {
            let series = format!("hs_federation_key_fetch_failures_total{{reason=\"{reason}\"}} ");
            assert!(text.contains(&series), "missing {series} in {text}");
        }
        assert!(text.contains("hs_federation_pdus_sent_total "), "{text}");
        assert!(!text.contains("_total_total"), "{text}");

        let before = transactions("dropped");
        record_transaction("dropped");
        assert_eq!(transactions("dropped"), before + 1);
        let before = pdus_sent();
        record_pdus_sent(3);
        assert_eq!(pdus_sent(), before + 3);
    }
}
