//! The bridge manager's Prometheus metrics, served by `hs serve` (registered by
//! [`register_metrics`]):
//!
//! | Metric | Type | Labels |
//! | --- | --- | --- |
//! | `hs_bridge_volume_used_ratio` | gauge | `bridge` (the appservice id), `bridge_type` |
//! | `hs_bridge_volume_capacity_bytes` | gauge | `bridge`, `bridge_type` |
//!
//! One series per instance this server deploys whose volume the operator has measured, read
//! about once a minute (`BridgeManager::watch_volumes`); a removed instance's series go. The
//! chart's PrometheusRule alerts on the ratio (`MyelinBridgeVolumeNearlyFull`, `...Full`).

use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;

use hs_admin::model::BridgeVolume;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// One deployed bridge instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct VolumeLabels {
    /// Its appservice id (`slack-alice`).
    bridge: String,
    /// Its catalogue type (`mautrix-slack`).
    bridge_type: String,
}

static USED_RATIO: LazyLock<Family<VolumeLabels, Gauge<f64, AtomicU64>>> =
    LazyLock::new(Family::default);
static CAPACITY_BYTES: LazyLock<Family<VolumeLabels, Gauge>> = LazyLock::new(Family::default);

fn labels(bridge: &str, bridge_type: &str) -> VolumeLabels {
    VolumeLabels {
        bridge: bridge.to_owned(),
        bridge_type: bridge_type.to_owned(),
    }
}

/// Sets `bridge`'s gauges from `volume`; a volume not measured yet sets nothing.
pub fn set_volume(bridge: &str, bridge_type: &str, volume: &BridgeVolume) {
    let labels = labels(bridge, bridge_type);
    if let Some(ratio) = volume.used_ratio {
        USED_RATIO.get_or_create(&labels).set(ratio);
    }
    if let Some(capacity) = volume.capacity_bytes {
        CAPACITY_BYTES
            .get_or_create(&labels)
            .set(i64::try_from(capacity).unwrap_or(i64::MAX));
    }
}

/// Drops `bridge`'s gauges (a removed instance).
pub fn remove_volume(bridge: &str, bridge_type: &str) {
    let labels = labels(bridge, bridge_type);
    USED_RATIO.remove(&labels);
    CAPACITY_BYTES.remove(&labels);
}

/// The ratio `bridge`'s gauge holds, for tests.
#[must_use]
pub fn used_ratio(bridge: &str, bridge_type: &str) -> Option<f64> {
    let labels = labels(bridge, bridge_type);
    USED_RATIO.get(&labels).map(|g| g.get())
}

/// Registers the bridge volume gauges into `registry`.
pub fn register_metrics(registry: &mut Registry) {
    registry.register(
        "hs_bridge_volume_used_ratio",
        "How full the volume of each bridge this server deploys was when the operator last \
         measured it, 0 to 1; nearly full from 0.85, full from 0.95 (a full bridge stops)",
        USED_RATIO.clone(),
    );
    registry.register(
        "hs_bridge_volume_capacity_bytes",
        "The size of the volume of each bridge this server deploys, in bytes, when last measured",
        CAPACITY_BYTES.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_measured_volume_sets_its_gauges_and_a_removed_one_drops_them() {
        let volume = BridgeVolume {
            size: "1Gi".into(),
            capacity_bytes: Some(1_020_702_720),
            used_bytes: Some(1_000_000_000),
            used_ratio: Some(0.98),
            state: "full".into(),
            ..BridgeVolume::default()
        };
        set_volume("slack-metrics-test", "mautrix-slack", &volume);
        assert_eq!(
            used_ratio("slack-metrics-test", "mautrix-slack"),
            Some(0.98)
        );
        let mut registry = Registry::default();
        register_metrics(&mut registry);
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        assert!(
            text.contains(
                "hs_bridge_volume_used_ratio{bridge=\"slack-metrics-test\",bridge_type=\"mautrix-slack\"} 0.98"
            ),
            "{text}"
        );
        assert!(text.contains("hs_bridge_volume_capacity_bytes{bridge=\"slack-metrics-test\""));
        remove_volume("slack-metrics-test", "mautrix-slack");
        assert_eq!(used_ratio("slack-metrics-test", "mautrix-slack"), None);
    }
}
