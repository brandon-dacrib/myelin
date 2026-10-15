//! Appservice delivery metrics, registered into the shared `hs_telemetry::metrics::Metrics`
//! registry by `hs-cli` (through `Metrics::with_registry`), per
//! `docs/decisions/0004-telemetry-conventions.md`:
//!
//! - `hs_appservice_transactions_total{appservice,outcome}`: transactions the scheduler
//!   attempted, by what became of each: `delivered` (the appservice answered 2xx) or `failed`
//!   (it did not; the transaction is retried, or dead-lettered once its attempts are spent).
//! - `hs_appservice_transaction_seconds{appservice,outcome}`: how long each attempt's HTTP
//!   exchange took, by `delivered`, `failed` or `timed_out`. A bridge whose handler blocks
//!   until its own event channel has room (mautrix-go's does) shows here as latency climbing
//!   toward the request timeout before it shows as failures.
//! - `hs_appservice_transaction_failures_total{appservice,reason}`: failed attempts by why:
//!   `timed_out` (the bridge took the request and did not answer in time; it may still be
//!   working on it), `unreachable` (no connection), `unavailable` (5xx, 408 or 429) and
//!   `rejected` (any other status). The first three are transient: the transaction is sent
//!   again under the same id until it has failed for a day.
//! - `hs_appservice_transaction_retries_total{appservice}`: attempts that sent a transaction id
//!   already tried once, with exactly the same entries (the spec's idempotency contract).
//! - `hs_appservice_dead_lettered_total{appservice}`: queue entries set aside because their
//!   retry budget ran out.
//! - `hs_appservice_batch_item_limit{appservice}`: how many items (events, ephemeral events
//!   and to-device messages) the next new transaction may carry. It starts at 100 (Synapse's
//!   cap), halves each time the appservice times out and doubles back on each delivery: below
//!   100 means the bridge has recently been too slow to answer.
//! - `hs_appservice_delivered_items_total{appservice,kind}`: what delivered transactions
//!   carried, by kind: `events`, `typing`, `receipts`, `presence` (MSC2409 ephemeral events),
//!   `to_device` (MSC2409/MSC4203 to-device messages), `device_list_changes`,
//!   `one_time_key_counts` and `fallback_key_types` (MSC3202, counted per user or device
//!   reported). Counted from the body as sent ([`crate::transaction::body_counts`]), once the
//!   appservice has answered 2xx, so a retry is not counted twice.
//!
//! - `hs_admin_bridge_login_queries_total{type,outcome}`: `GET /api/v1/appservices/{id}/logins`
//!   answers ([`crate::provisioning`]), by catalogue bridge type (`custom` for a registration
//!   that did not come from the catalogue) and outcome: `answered` (the bridge's provisioning
//!   API answered), `cached` (an answer under 30 seconds old was reused), `unsupported` (the
//!   type has no API that reports sign-ins, or the registration lacks what asking needs),
//!   `unreachable`, `timeout`, `refused` (the bridge answered with an error) or
//!   `invalid_answer`. Named for the admin API it serves; counted here, where the asking is.
//!
//! - `hs_appservice_queries_total{appservice,kind,outcome}`: the homeserver's questions to an
//!   appservice ([`crate::query`]), by kind (`user`, `room_alias`, `protocol`,
//!   `thirdparty_user`, `thirdparty_location`) and outcome: `yes` (it answered 2xx), `no` (404, or
//!   an answer that was not what the spec asks for), `error` (unreachable, or another status), or
//!   `cached` (protocol metadata answered from the last five minutes).
//!
//! - `hs_appservice_queue_depth{appservice}`, `hs_appservice_queue_dead_lettered{appservice}`
//!   and `hs_appservice_queue_oldest_age_seconds{appservice}`: gauges read from the queue on
//!   every scrape ([`QueueCollector`]): how many queued entries wait to be sent (a backing-off
//!   one included; each is one room's page of events or one ephemeral batch, merged up to
//!   twenty to a transaction), how many ran out of attempts and wait for a replay, and how long
//!   the oldest waiting one has waited (0 when none does). A bridge that keeps up holds the depth
//!   near 0 and the age under a second; a depth that climbs while the other bridges' stay flat
//!   is that bridge falling behind. Reported by the replica that delivers to the appservice
//!   (the owner of its shard), so a cluster's series never count one queue twice.
//!
//! - `hs_appservice_ephemeral_pruned_total{stream}` and
//!   `hs_appservice_ephemeral_prune_failures_total{stream}`: entries the ephemeral pump
//!   ([`crate::ephemeral`]) deleted from the `receipts`, `presence` and `to_device` streams once
//!   every appservice had been sent them, and prunes of a stream that failed (each is retried on
//!   the next tick). Failures that keep climbing mean a stream is growing without bound.
//!
//! `appservice` is the registration id: bounded by how many bridges an operator runs, and what
//! the operator looks at the numbers by.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use std::sync::Arc;

use hs_kv::KvBackend;

use crate::store::QueueSummary;
use crate::transaction::BodyCounts;

/// Labels of `hs_appservice_transactions_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct TransactionLabels {
    /// The registration id.
    pub appservice: String,
    /// `delivered` or `failed`.
    pub outcome: String,
}

/// Labels of `hs_appservice_transaction_failures_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct FailureLabels {
    /// The registration id.
    pub appservice: String,
    /// `timed_out`, `unreachable`, `unavailable` or `rejected`.
    pub reason: String,
}

/// Labels of the per-appservice series that carry nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct AppserviceLabels {
    /// The registration id.
    pub appservice: String,
}

/// The buckets of `hs_appservice_transaction_seconds`, in seconds: from a bridge that answers
/// at once to one that holds the request past the 60-second timeout.
const TRANSACTION_SECONDS_BUCKETS: [f64; 13] = [
    0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0,
];

fn transaction_seconds_histogram() -> Histogram {
    Histogram::new(TRANSACTION_SECONDS_BUCKETS)
}

/// Labels of `hs_appservice_delivered_items_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct DeliveredItemLabels {
    /// The registration id.
    pub appservice: String,
    /// One of [`BodyCounts::by_kind`]'s names.
    pub kind: String,
}

/// Labels of `hs_admin_bridge_login_queries_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct BridgeLoginLabels {
    /// The catalogue bridge type, or `custom`.
    pub r#type: String,
    /// See the module docs.
    pub outcome: String,
}

/// Labels of `hs_appservice_key_withheld_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct KeyWithheldLabels {
    /// The registration id.
    pub appservice: String,
    /// The `m.room_key.withheld` event's `code` (`m.unverified`, `m.blacklisted`, ...).
    pub code: String,
}

/// Labels of `hs_appservice_queries_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct QueryLabels {
    /// The registration id.
    pub appservice: String,
    /// `user`, `room_alias`, `protocol`, `thirdparty_user` or `thirdparty_location`.
    pub kind: String,
    /// `yes`, `no`, `error` or `cached`.
    pub outcome: String,
}

/// Labels of `hs_appservice_ephemeral_pruned_total` and
/// `hs_appservice_ephemeral_prune_failures_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct EphemeralStreamLabels {
    /// `receipts`, `presence` or `to_device`.
    pub stream: String,
}

/// The delivery metric families. Cheap to clone; every clone counts into the same families.
#[derive(Clone)]
pub struct AppserviceMetrics {
    /// `hs_appservice_transactions_total{appservice,outcome}`.
    pub transactions_total: Family<TransactionLabels, Counter>,
    /// `hs_appservice_transaction_seconds{appservice,outcome}`.
    pub transaction_seconds: Family<TransactionLabels, Histogram, fn() -> Histogram>,
    /// `hs_appservice_transaction_failures_total{appservice,reason}`.
    pub transaction_failures_total: Family<FailureLabels, Counter>,
    /// `hs_appservice_transaction_retries_total{appservice}`.
    pub transaction_retries_total: Family<AppserviceLabels, Counter>,
    /// `hs_appservice_dead_lettered_total{appservice}`.
    pub dead_lettered_total: Family<AppserviceLabels, Counter>,
    /// `hs_appservice_batch_item_limit{appservice}`.
    pub batch_item_limit: Family<AppserviceLabels, Gauge>,
    /// `hs_appservice_delivered_items_total{appservice,kind}`.
    pub delivered_items_total: Family<DeliveredItemLabels, Counter>,
    /// `hs_admin_bridge_login_queries_total{type,outcome}`.
    pub bridge_login_queries_total: Family<BridgeLoginLabels, Counter>,
    /// `hs_appservice_key_withheld_total{appservice,code}`.
    pub key_withheld_total: Family<KeyWithheldLabels, Counter>,
    /// `hs_appservice_queries_total{appservice,kind,outcome}`.
    pub queries_total: Family<QueryLabels, Counter>,
    /// `hs_appservice_ephemeral_pruned_total{stream}`.
    pub ephemeral_pruned_total: Family<EphemeralStreamLabels, Counter>,
    /// `hs_appservice_ephemeral_prune_failures_total{stream}`.
    pub ephemeral_prune_failures_total: Family<EphemeralStreamLabels, Counter>,
}

impl Default for AppserviceMetrics {
    fn default() -> Self {
        Self {
            transactions_total: Family::default(),
            transaction_seconds: Family::new_with_constructor(transaction_seconds_histogram),
            transaction_failures_total: Family::default(),
            transaction_retries_total: Family::default(),
            dead_lettered_total: Family::default(),
            batch_item_limit: Family::default(),
            delivered_items_total: Family::default(),
            bridge_login_queries_total: Family::default(),
            key_withheld_total: Family::default(),
            queries_total: Family::default(),
            ephemeral_pruned_total: Family::default(),
            ephemeral_prune_failures_total: Family::default(),
        }
    }
}

impl AppserviceMetrics {
    /// Registers the families into `registry` (the shared one, in `hs serve`).
    #[must_use]
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self::default();
        // Registered without `_total`: the text encoder appends it to every counter
        // (`hs_telemetry::metrics`' module docs).
        registry.register(
            "hs_appservice_transactions",
            "Appservice transactions attempted, by appservice and outcome (delivered, failed)",
            metrics.transactions_total.clone(),
        );
        registry.register(
            "hs_appservice_transaction_seconds",
            "How long each appservice transaction's HTTP exchange took, by appservice and \
             outcome (delivered, failed, timed_out)",
            metrics.transaction_seconds.clone(),
        );
        registry.register(
            "hs_appservice_transaction_failures",
            "Failed appservice transaction attempts, by appservice and reason (timed_out, \
             unreachable, unavailable, rejected)",
            metrics.transaction_failures_total.clone(),
        );
        registry.register(
            "hs_appservice_transaction_retries",
            "Appservice transaction attempts that sent an already-tried transaction id again, \
             with the same entries",
            metrics.transaction_retries_total.clone(),
        );
        registry.register(
            "hs_appservice_dead_lettered",
            "Appservice queue entries dead-lettered because their retry budget ran out",
            metrics.dead_lettered_total.clone(),
        );
        registry.register(
            "hs_appservice_batch_item_limit",
            "How many items the next new transaction to an appservice may carry; below 100 the \
             appservice has recently timed out",
            metrics.batch_item_limit.clone(),
        );
        registry.register(
            "hs_appservice_delivered_items",
            "What delivered appservice transactions carried, by appservice and kind (events, \
             typing, receipts, presence, to_device, device_list_changes, one_time_key_counts, \
             fallback_key_types)",
            metrics.delivered_items_total.clone(),
        );
        registry.register(
            "hs_admin_bridge_login_queries",
            "Admin API questions to a bridge's provisioning API about who has signed in, by \
             bridge type and outcome (answered, cached, unsupported, unreachable, timeout, \
             refused, invalid_answer)",
            metrics.bridge_login_queries_total.clone(),
        );
        registry.register(
            "hs_appservice_key_withheld",
            "m.room_key.withheld events delivered to an appservice's users (a client refused \
             to share a room's keys with the bridge), by appservice and the event's code",
            metrics.key_withheld_total.clone(),
        );
        registry.register(
            "hs_appservice_queries",
            "The homeserver's questions to appservices (does this user or alias exist, \
             third-party protocols and lookups), by appservice, kind and outcome (yes, no, \
             error, cached)",
            metrics.queries_total.clone(),
        );
        registry.register(
            "hs_appservice_ephemeral_pruned",
            "Entries the appservice ephemeral pump deleted from a stream (receipts, presence, \
             to_device) once every appservice had been sent them",
            metrics.ephemeral_pruned_total.clone(),
        );
        registry.register(
            "hs_appservice_ephemeral_prune_failures",
            "Prunes of an appservice ephemeral stream (receipts, presence, to_device) that \
             failed; the next tick tries again",
            metrics.ephemeral_prune_failures_total.clone(),
        );
        metrics
    }

    /// Counts `pruned` entries deleted from `stream`.
    pub fn record_ephemeral_pruned(&self, stream: &str, pruned: usize) {
        self.ephemeral_pruned_total
            .get_or_create(&EphemeralStreamLabels {
                stream: stream.to_owned(),
            })
            .inc_by(u64::try_from(pruned).unwrap_or(u64::MAX));
    }

    /// Counts one failed prune of `stream`.
    pub fn record_ephemeral_prune_failed(&self, stream: &str) {
        self.ephemeral_prune_failures_total
            .get_or_create(&EphemeralStreamLabels {
                stream: stream.to_owned(),
            })
            .inc();
    }

    /// Counts one question to `appservice`.
    pub fn record_query(&self, appservice: &str, kind: &str, outcome: &str) {
        self.queries_total
            .get_or_create(&QueryLabels {
                appservice: appservice.to_owned(),
                kind: kind.to_owned(),
                outcome: outcome.to_owned(),
            })
            .inc();
    }

    /// Counts one `m.room_key.withheld` delivered to `appservice`.
    pub fn record_key_withheld(&self, appservice: &str, code: &str) {
        self.key_withheld_total
            .get_or_create(&KeyWithheldLabels {
                appservice: appservice.to_owned(),
                code: code.to_owned(),
            })
            .inc();
    }

    /// Counts one `appservices.logins` answer.
    pub fn record_login_query(&self, bridge_type: &str, outcome: &str) {
        self.bridge_login_queries_total
            .get_or_create(&BridgeLoginLabels {
                r#type: bridge_type.to_owned(),
                outcome: outcome.to_owned(),
            })
            .inc();
    }

    /// Counts a transaction the appservice accepted, and what it carried.
    pub fn record_delivered(&self, appservice: &str, counts: &BodyCounts) {
        self.transactions_total
            .get_or_create(&TransactionLabels {
                appservice: appservice.to_owned(),
                outcome: "delivered".to_owned(),
            })
            .inc();
        for (kind, count) in counts.by_kind() {
            if count > 0 {
                self.delivered_items_total
                    .get_or_create(&DeliveredItemLabels {
                        appservice: appservice.to_owned(),
                        kind: kind.to_owned(),
                    })
                    .inc_by(count as u64);
            }
        }
    }

    /// Counts a transaction the appservice did not accept, and why (`reason`, one of
    /// [`crate::scheduler::SendErrorKind::as_str`]'s words).
    pub fn record_failed(&self, appservice: &str, reason: &str) {
        self.transactions_total
            .get_or_create(&TransactionLabels {
                appservice: appservice.to_owned(),
                outcome: "failed".to_owned(),
            })
            .inc();
        self.transaction_failures_total
            .get_or_create(&FailureLabels {
                appservice: appservice.to_owned(),
                reason: reason.to_owned(),
            })
            .inc();
    }

    /// Times one attempt's HTTP exchange: `outcome` is `delivered`, `failed` or `timed_out`.
    pub fn record_attempt_seconds(&self, appservice: &str, outcome: &str, seconds: f64) {
        self.transaction_seconds
            .get_or_create(&TransactionLabels {
                appservice: appservice.to_owned(),
                outcome: outcome.to_owned(),
            })
            .observe(seconds);
    }

    /// Counts one attempt that sent an already-tried transaction id again.
    pub fn record_retry(&self, appservice: &str) {
        self.transaction_retries_total
            .get_or_create(&AppserviceLabels {
                appservice: appservice.to_owned(),
            })
            .inc();
    }

    /// Counts `entries` queue entries dead-lettered.
    pub fn record_dead_lettered(&self, appservice: &str, entries: usize) {
        if entries == 0 {
            return;
        }
        self.dead_lettered_total
            .get_or_create(&AppserviceLabels {
                appservice: appservice.to_owned(),
            })
            .inc_by(u64::try_from(entries).unwrap_or(u64::MAX));
    }

    /// Sets the item limit of `appservice`'s next new transaction.
    pub fn set_batch_item_limit(&self, appservice: &str, limit: usize) {
        self.batch_item_limit
            .get_or_create(&AppserviceLabels {
                appservice: appservice.to_owned(),
            })
            .set(i64::try_from(limit).unwrap_or(i64::MAX));
    }
}

/// Which appservices' queues this replica reports, and their numbers: what [`QueueCollector`]
/// reads on every scrape.
pub type QueueReader = Arc<dyn Fn() -> Vec<(String, QueueSummary)> + Send + Sync>;

/// Renders the queue gauges (see the module docs) on every scrape. Register it with
/// `registry.register_collector(Box::new(collector))`.
#[derive(Clone)]
pub struct QueueCollector {
    read: QueueReader,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for QueueCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueCollector").finish_non_exhaustive()
    }
}

impl QueueCollector {
    /// Reports the queue of every appservice in `registry` that `delivers_here` says this
    /// replica delivers to.
    #[must_use]
    pub fn over_registry<B: KvBackend + 'static>(
        registry: Arc<crate::registry::Registry<B>>,
        delivers_here: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        let clock = registry.clone();
        let read: QueueReader = Arc::new(move || {
            let rows = match registry.list() {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(%error, "could not list appservices for the queue gauges");
                    return Vec::new();
                }
            };
            rows.into_iter()
                .filter(|row| row.url.is_some() && delivers_here(&row.id))
                .filter_map(|row| match registry.store().queue_summary(&row.id) {
                    Ok(summary) => Some((row.id, summary)),
                    Err(error) => {
                        tracing::warn!(appservice = %row.id, %error, "could not read an appservice's queue for the queue gauges");
                        None
                    }
                })
                .collect()
        });
        Self {
            read,
            now_ms: Arc::new(move || clock.now_ms()),
        }
    }
}

impl prometheus_client::collector::Collector for QueueCollector {
    fn encode(
        &self,
        mut encoder: prometheus_client::encoding::DescriptorEncoder,
    ) -> Result<(), std::fmt::Error> {
        use prometheus_client::metrics::MetricType;
        let mut queues = (self.read)();
        queues.sort_by(|a, b| a.0.cmp(&b.0));
        let now = (self.now_ms)();

        let mut family = encoder.encode_descriptor(
            "hs_appservice_queue_depth",
            "Queued entries waiting to be sent to an appservice (a backing-off one included), by \
             appservice",
            None,
            MetricType::Gauge,
        )?;
        for (id, summary) in &queues {
            family
                .encode_family(&[("appservice", id.as_str())])?
                .encode_gauge(&summary.pending)?;
        }
        let mut family = encoder.encode_descriptor(
            "hs_appservice_queue_dead_lettered",
            "Queued entries that ran out of delivery attempts and wait for an operator's replay, \
             by appservice",
            None,
            MetricType::Gauge,
        )?;
        for (id, summary) in &queues {
            family
                .encode_family(&[("appservice", id.as_str())])?
                .encode_gauge(&summary.dead_lettered)?;
        }
        let mut family = encoder.encode_descriptor(
            "hs_appservice_queue_oldest_age_seconds",
            "How long the oldest entry waiting to be sent to an appservice has waited (0 when \
             none waits), by appservice",
            None,
            MetricType::Gauge,
        )?;
        for (id, summary) in &queues {
            #[allow(
                clippy::cast_precision_loss,
                reason = "milliseconds of waiting fit an f64"
            )]
            let age = summary
                .oldest_pending_at_ms
                .map_or(0.0, |at| now.saturating_sub(at) as f64 / 1000.0);
            family
                .encode_family(&[("appservice", id.as_str())])?
                .encode_gauge(&age)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_and_failed_transactions_are_counted_by_kind_and_outcome() {
        let mut registry = Registry::default();
        let metrics = AppserviceMetrics::register(&mut registry);
        metrics.record_delivered(
            "irc",
            &BodyCounts {
                events: 2,
                typing: 1,
                ..BodyCounts::default()
            },
        );
        metrics.record_delivered(
            "irc",
            &BodyCounts {
                to_device: 3,
                ..BodyCounts::default()
            },
        );
        metrics.record_failed("irc", "timed_out");
        metrics.record_attempt_seconds("irc", "timed_out", 60.0);
        metrics.record_retry("irc");
        metrics.record_dead_lettered("irc", 20);
        metrics.set_batch_item_limit("irc", 50);
        metrics.record_login_query("mautrix-whatsapp", "answered");
        metrics.record_query("irc", "user", "yes");
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        assert!(
            text.contains(
                "hs_appservice_transactions_total{appservice=\"irc\",outcome=\"delivered\"} 2"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "hs_appservice_transactions_total{appservice=\"irc\",outcome=\"failed\"} 1"
            ),
            "{text}"
        );
        for line in [
            "hs_appservice_transaction_failures_total{appservice=\"irc\",reason=\"timed_out\"} 1",
            "hs_appservice_transaction_seconds_count{appservice=\"irc\",outcome=\"timed_out\"} 1",
            "hs_appservice_transaction_retries_total{appservice=\"irc\"} 1",
            "hs_appservice_dead_lettered_total{appservice=\"irc\"} 20",
            "hs_appservice_batch_item_limit{appservice=\"irc\"} 50",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
        assert!(
            text.contains(
                "hs_appservice_delivered_items_total{appservice=\"irc\",kind=\"events\"} 2"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "hs_appservice_delivered_items_total{appservice=\"irc\",kind=\"to_device\"} 3"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "hs_admin_bridge_login_queries_total{type=\"mautrix-whatsapp\",outcome=\"answered\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "hs_appservice_queries_total{appservice=\"irc\",kind=\"user\",outcome=\"yes\"} 1"
            ),
            "{text}"
        );
        assert!(
            !text.contains("kind=\"receipts\""),
            "a zero is not a series: {text}"
        );
    }

    /// The queue gauges read the queue as it is at the scrape, for the appservices this replica
    /// delivers to.
    #[test]
    fn the_queue_gauges_say_what_waits_for_each_appservice_this_replica_delivers_to() {
        use crate::registration::Registration;
        use hs_kv::memory::MemoryBackend;
        let store = Arc::new(
            crate::registry::Registry::open(
                MemoryBackend::new(),
                ruma::server_name!("example.org"),
            )
            .unwrap(),
        );
        for id in ["irc", "slack", "elsewhere"] {
            store
                .add(
                    &Registration::parse_yaml(&format!(
                        "id: {id}\nurl: 'http://{id}'\nas_token: as_{id}\nhs_token: hs_{id}\n\
                         sender_localpart: {id}bot\nnamespaces: {{}}\n"
                    ))
                    .unwrap(),
                )
                .unwrap();
        }
        let now = store.now_ms();
        for _ in 0..3 {
            store
                .store()
                .enqueue(
                    "irc",
                    serde_json::json!({"events": []}),
                    now.saturating_sub(4_000),
                )
                .unwrap();
        }
        let seq = store
            .store()
            .enqueue("irc", serde_json::json!({"events": []}), now)
            .unwrap();
        let mut entry = store
            .store()
            .queue_for("irc")
            .unwrap()
            .into_iter()
            .find(|e| e.seq == seq)
            .unwrap();
        entry.status = crate::store::QueueStatus::DeadLettered;
        store.store().put_queue_entry("irc", &entry).unwrap();
        store
            .store()
            .enqueue("elsewhere", serde_json::json!({"events": []}), now)
            .unwrap();

        let mut registry = Registry::default();
        registry.register_collector(Box::new(QueueCollector::over_registry(
            store,
            Arc::new(|id: &str| id != "elsewhere"),
        )));
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        assert!(
            text.contains("hs_appservice_queue_depth{appservice=\"irc\"} 3"),
            "{text}"
        );
        assert!(
            text.contains("hs_appservice_queue_depth{appservice=\"slack\"} 0"),
            "{text}"
        );
        assert!(
            text.contains("hs_appservice_queue_dead_lettered{appservice=\"irc\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("hs_appservice_queue_oldest_age_seconds{appservice=\"slack\"} 0"),
            "{text}"
        );
        let age: f64 = text
            .lines()
            .find(|l| l.starts_with("hs_appservice_queue_oldest_age_seconds{appservice=\"irc\"}"))
            .and_then(|l| l.rsplit(' ').next())
            .unwrap()
            .parse()
            .unwrap();
        assert!((4.0..60.0).contains(&age), "{text}");
        assert!(!text.contains("elsewhere"), "another replica's: {text}");
    }
}
