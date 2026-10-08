//! Appservice delivery metrics, registered into the shared `hs_telemetry::metrics::Metrics`
//! registry by `hs-cli` (through `Metrics::with_registry`), per
//! `docs/decisions/0004-telemetry-conventions.md`:
//!
//! - `hs_appservice_transactions_total{appservice,outcome}`: transactions the scheduler
//!   attempted, by what became of each: `delivered` (the appservice answered 2xx) or `failed`
//!   (it did not; the transaction is retried, or dead-lettered once its attempts are spent).
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
//! `appservice` is the registration id: bounded by how many bridges an operator runs, and what
//! the operator looks at the numbers by.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
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

/// The delivery metric families. Cheap to clone; every clone counts into the same families.
#[derive(Clone, Default)]
pub struct AppserviceMetrics {
    /// `hs_appservice_transactions_total{appservice,outcome}`.
    pub transactions_total: Family<TransactionLabels, Counter>,
    /// `hs_appservice_delivered_items_total{appservice,kind}`.
    pub delivered_items_total: Family<DeliveredItemLabels, Counter>,
    /// `hs_admin_bridge_login_queries_total{type,outcome}`.
    pub bridge_login_queries_total: Family<BridgeLoginLabels, Counter>,
    /// `hs_appservice_key_withheld_total{appservice,code}`.
    pub key_withheld_total: Family<KeyWithheldLabels, Counter>,
    /// `hs_appservice_queries_total{appservice,kind,outcome}`.
    pub queries_total: Family<QueryLabels, Counter>,
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
        metrics
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

    /// Counts a transaction the appservice did not accept.
    pub fn record_failed(&self, appservice: &str) {
        self.transactions_total
            .get_or_create(&TransactionLabels {
                appservice: appservice.to_owned(),
                outcome: "failed".to_owned(),
            })
            .inc();
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
        metrics.record_failed("irc");
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
