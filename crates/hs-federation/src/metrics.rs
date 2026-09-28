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
//!
//! `edu_type` is bounded: an EDU type that is not one of [`KNOWN_EDU_TYPES`] is counted as
//! `other`, since the type is whatever a remote server wrote.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
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
        metrics
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
}
