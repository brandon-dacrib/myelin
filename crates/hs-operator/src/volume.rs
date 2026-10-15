//! How full each bridge's volume is: the `Bridge` controller's volume sweep.
//!
//! On 2026-10-15 the demo's Slack bridge had been stopped for a day by a full 1 GiB volume
//! (its init container failing with `cp: write error: No space left on device`), and nothing had
//! said so: the kubelet's `kubelet_volume_stats_*` series showed 0.98 since the day before, but
//! nothing read them. Every [`SWEEP_INTERVAL`] the sweep asks the kubelet of each node running a
//! bridge for its volume statistics (`/stats/summary`, the source of those series) and, for each
//! bridge's claim (`<name>-data`):
//!
//! - sets `hs_operator_bridge_volume_used_ratio{bridge}` ([`crate::metrics`]);
//! - logs a `WARN` when the volume passes [`NEARLY_FULL`] or [`FULL`], once per crossing, and an
//!   `INFO` when it has room again;
//! - writes the measurement into the `Bridge`'s `status.volume`
//!   ([`crate::crds::VolumeStatus`]) when it has changed enough to matter ([`should_record`]),
//!   which is where the homeserver reads it from, to show on the bridge's page, count in
//!   `hs_bridge_volume_used_ratio` and warn about.
//!
//! A pod that is not running (a bridge whose init container cannot write to its full volume)
//! is not in the kubelet's summary, so its last measurement stays in the status: the volume that
//! stopped it still reads as full.
//!
//! [`VolumeStatsSource`] says how the kubelet is reached. The default, [`VolumeStatsSource::Kubelet`],
//! connects to each node's kubelet directly with the operator's service-account token, which
//! the kubelet authorizes as `get` on `nodes/stats` (the chart's ClusterRole grants that and
//! `get` on `nodes`, for the kubelet's address). [`VolumeStatsSource::ApiServerProxy`] goes
//! through the API server's node proxy instead, which needs `get` on `nodes/proxy`: simpler
//! networking, but `nodes/proxy` also reaches every other kubelet endpoint, so it is not the
//! default.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{ListParams, Patch, PatchParams};
use kube::{Api, Client, ResourceExt as _};
use serde_json::Value;

use crate::bridge::pvc_name;
use crate::crds::{Bridge, VolumeStatus};
use crate::metrics::OperatorMetrics;

/// How often the sweep measures. The kubelet itself refreshes volume statistics about once a
/// minute (`--volume-stats-agg-period`).
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// From this fraction used a bridge's volume is nearly full: a `WARN`, and the bridge's page
/// says so.
pub const NEARLY_FULL: f64 = 0.85;
/// From this fraction used a bridge's volume is full: SQLite and the init container's copy
/// start failing well before the last byte (filesystem reserve, temporary files).
pub const FULL: f64 = 0.95;
/// A recorded measurement is written again after this long even if nothing changed, so the
/// status says when it was last looked at.
pub const RECORD_AT_LEAST_EVERY: Duration = Duration::from_secs(15 * 60);
/// A change in the used fraction this large is written to the status straight away.
pub const RECORD_RATIO_STEP: f64 = 0.01;

/// Where the sweep reads volume statistics from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VolumeStatsSource {
    /// It does not measure.
    #[default]
    Off,
    /// Each node's kubelet, directly (`https://<InternalIP>:<kubelet port>/stats/summary`), as
    /// metrics-server does. Needs `get` on `nodes` and `nodes/stats`.
    Kubelet {
        /// Skip verifying the kubelet's serving certificate (metrics-server's
        /// `--kubelet-insecure-tls`). Most clusters' kubelets serve a self-signed certificate
        /// (kubeadm, kind and Talos do unless serving certificates are bootstrapped), which
        /// cannot be verified against the cluster CA.
        insecure_tls: bool,
    },
    /// The API server's node proxy (`/api/v1/nodes/<node>/proxy/stats/summary`). Needs `get` on
    /// `nodes/proxy`.
    ApiServerProxy,
}

/// How full a volume is, in the words the logs, the status and the homeserver use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Below [`NEARLY_FULL`].
    Ok,
    /// [`NEARLY_FULL`] or more.
    NearlyFull,
    /// [`FULL`] or more.
    Full,
}

impl Level {
    /// The level of a volume `ratio` used.
    #[must_use]
    pub fn of(ratio: f64) -> Self {
        if ratio >= FULL {
            Self::Full
        } else if ratio >= NEARLY_FULL {
            Self::NearlyFull
        } else {
            Self::Ok
        }
    }

    /// `ok`, `nearly_full` or `full`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NearlyFull => "nearly_full",
            Self::Full => "full",
        }
    }
}

/// One claim's statistics from a kubelet summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimStats {
    /// The claim's namespace.
    pub namespace: String,
    /// The claim's name.
    pub claim: String,
    /// The measurement.
    pub volume: VolumeStatus,
}

/// Every claim-backed volume in a kubelet `/stats/summary` document (`pods[].volume[]` entries
/// with a `pvcRef` and byte counts). Anything else, or anything malformed, is skipped.
#[must_use]
pub fn parse_summary(summary: &Value) -> Vec<ClaimStats> {
    let mut out = Vec::new();
    let pods = summary["pods"].as_array().map_or(&[][..], Vec::as_slice);
    for pod in pods {
        let volumes = pod["volume"].as_array().map_or(&[][..], Vec::as_slice);
        for v in volumes {
            let (Some(namespace), Some(claim)) = (
                v["pvcRef"]["namespace"].as_str(),
                v["pvcRef"]["name"].as_str(),
            ) else {
                continue;
            };
            let (Some(capacity_bytes), Some(used_bytes)) =
                (v["capacityBytes"].as_i64(), v["usedBytes"].as_i64())
            else {
                continue;
            };
            out.push(ClaimStats {
                namespace: namespace.to_owned(),
                claim: claim.to_owned(),
                volume: VolumeStatus {
                    capacity_bytes,
                    used_bytes,
                    observed_at: v["time"].as_str().unwrap_or_default().to_owned(),
                },
            });
        }
    }
    out
}

/// Whether a new measurement `now` should be written over the recorded `previous` one: there
/// is none, the size changed, the used fraction moved by [`RECORD_RATIO_STEP`] or crossed a
/// [`Level`], or the recorded one is [`RECORD_AT_LEAST_EVERY`] old. `age` is how old
/// `previous` is.
#[must_use]
pub fn should_record(previous: Option<&VolumeStatus>, now: &VolumeStatus, age: Duration) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.capacity_bytes != now.capacity_bytes
        || (previous.used_ratio() - now.used_ratio()).abs() >= RECORD_RATIO_STEP
        || Level::of(previous.used_ratio()) != Level::of(now.used_ratio())
        || age >= RECORD_AT_LEAST_EVERY
}

/// What to log when a bridge's volume goes from `previous` to `now` (`None` for the first
/// measurement since the operator started): a `WARN` sentence when it rises to nearly full or
/// full (or is first seen so), an `INFO` one when it falls back below nearly full, else nothing.
#[must_use]
pub fn crossing(previous: Option<Level>, now: Level) -> Option<(tracing::Level, &'static str)> {
    let rose = match previous {
        None => now > Level::Ok,
        Some(previous) => now > previous,
    };
    if rose {
        return Some((
            tracing::Level::WARN,
            match now {
                Level::Full => {
                    "a bridge's volume is full: the bridge cannot write its database and will \
                     stop (its init container fails with \"No space left on device\"); raise \
                     its volume size on the bridge's page in the admin interface"
                }
                _ => {
                    "a bridge's volume is nearly full: raise its volume size on the bridge's \
                     page in the admin interface before the bridge stops"
                }
            },
        ));
    }
    match (previous, now) {
        (Some(previous), Level::Ok) if previous > Level::Ok => {
            Some((tracing::Level::INFO, "a bridge's volume has room again"))
        }
        _ => None,
    }
}

/// The kubelet base URL of `node` from its status: its first `InternalIP` (else any address),
/// at the port it reports (`daemonEndpoints.kubeletEndpoint.Port`, else 10250).
#[must_use]
pub fn kubelet_url(node: &Node) -> Option<String> {
    let status = node.status.as_ref()?;
    let addresses = status.addresses.as_deref().unwrap_or_default();
    let address = addresses
        .iter()
        .find(|a| a.type_ == "InternalIP")
        .or_else(|| addresses.iter().find(|a| a.type_ == "ExternalIP"))
        .or_else(|| addresses.first())?
        .address
        .clone();
    let port = status
        .daemon_endpoints
        .as_ref()
        .and_then(|d| d.kubelet_endpoint.as_ref())
        .map(|e| e.port)
        .filter(|p| *p > 0)
        .unwrap_or(10250);
    let host = if address.contains(':') {
        format!("[{address}]")
    } else {
        address
    };
    Some(format!("https://{host}:{port}"))
}

/// The bridges' claims on each node, from the bridge pods: node name to the bridges (by
/// `app.kubernetes.io/instance`) with a pod there.
fn bridges_by_node(pods: &[Pod]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for pod in pods {
        let (Some(node), Some(bridge)) = (
            pod.spec.as_ref().and_then(|s| s.node_name.clone()),
            pod.labels().get("app.kubernetes.io/instance").cloned(),
        ) else {
            continue;
        };
        out.entry(node).or_default().insert(bridge);
    }
    out
}

/// What the sweep remembers between rounds.
#[derive(Debug, Default)]
struct SweepState {
    /// Each bridge's level at its last measurement, for logging crossings once.
    levels: BTreeMap<String, Level>,
    /// The bridges whose gauge is set, to drop the gauge of one that is deleted.
    gauged: BTreeSet<String>,
    /// The last error each node's summary failed with, to log a failure once rather than every
    /// round.
    node_errors: BTreeMap<String, String>,
}

/// Runs the volume sweep over the `Bridge`s in `namespace` every [`SWEEP_INTERVAL`], forever
/// (the controller drops it on shutdown). Does nothing for [`VolumeStatsSource::Off`].
pub async fn run(
    client: Client,
    namespace: String,
    source: VolumeStatsSource,
    metrics: OperatorMetrics,
) {
    if source == VolumeStatsSource::Off {
        tracing::info!("bridge volume statistics are off: volumes are not measured");
        return std::future::pending().await;
    }
    tracing::info!(namespace = %namespace, source = ?source, "measuring bridge volumes");
    let mut state = SweepState::default();
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(e) = sweep(&client, &namespace, source, &metrics, &mut state).await {
            tracing::warn!(error = %e, "could not measure bridge volumes this round");
        }
    }
}

async fn sweep(
    client: &Client,
    namespace: &str,
    source: VolumeStatsSource,
    metrics: &OperatorMetrics,
    state: &mut SweepState,
) -> Result<(), kube::Error> {
    let bridges: Api<Bridge> = Api::namespaced(client.clone(), namespace);
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let all = bridges.list(&ListParams::default()).await?.items;
    let pod_list = pods
        .list(&ListParams::default().labels(&crate::controller::watch_selector()))
        .await?
        .items;

    let mut measured: BTreeMap<String, VolumeStatus> = BTreeMap::new();
    for (node, on_node) in bridges_by_node(&pod_list) {
        let summary = match summary(client, &node, source).await {
            Ok(summary) => {
                if state.node_errors.remove(&node).is_some() {
                    tracing::info!(node = %node, "the kubelet's volume statistics can be read again");
                }
                summary
            }
            Err(e) => {
                if state.node_errors.get(&node) != Some(&e) {
                    tracing::warn!(
                        node = %node,
                        error = %e,
                        source = ?source,
                        "cannot read the kubelet's volume statistics, so the bridges on this \
                         node are not measured (the chart's bridges.operator.volumeStats \
                         settings choose how the kubelet is reached)"
                    );
                    state.node_errors.insert(node.clone(), e);
                }
                continue;
            }
        };
        for stats in parse_summary(&summary) {
            if stats.namespace != namespace {
                continue;
            }
            if let Some(bridge) = on_node.iter().find(|b| pvc_name(b) == stats.claim) {
                measured.insert(bridge.clone(), stats.volume);
            }
        }
    }

    let now = k8s_openapi::chrono::Utc::now();
    let mut present = BTreeSet::new();
    for bridge in &all {
        let name = bridge.name_any();
        present.insert(name.clone());
        let recorded = bridge.status.as_ref().and_then(|s| s.volume.as_ref());
        let Some(volume) = measured.remove(&name).or_else(|| recorded.cloned()) else {
            continue;
        };
        let ratio = volume.used_ratio();
        metrics.set_bridge_volume(&name, ratio);
        state.gauged.insert(name.clone());
        let level = Level::of(ratio);
        let previous = state.levels.insert(name.clone(), level);
        if let Some((log_level, what)) = crossing(previous, level) {
            let percent = (ratio * 100.0).round();
            if log_level == tracing::Level::WARN {
                tracing::warn!(
                    bridge = %name,
                    used_percent = percent,
                    used_bytes = volume.used_bytes,
                    capacity_bytes = volume.capacity_bytes,
                    size = %bridge.spec.storage.size,
                    "{what}"
                );
            } else {
                tracing::info!(bridge = %name, used_percent = percent, "{what}");
            }
        }
        let age = recorded
            .and_then(|r| k8s_openapi::chrono::DateTime::parse_from_rfc3339(&r.observed_at).ok())
            .and_then(|t| (now - t.to_utc()).to_std().ok())
            .unwrap_or(Duration::MAX);
        if recorded != Some(&volume) && should_record(recorded, &volume, age) {
            bridges
                .patch_status(
                    &name,
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({ "status": { "volume": volume } })),
                )
                .await?;
        }
    }
    for gone in state
        .gauged
        .difference(&present)
        .cloned()
        .collect::<Vec<_>>()
    {
        metrics.remove_bridge_volume(&gone);
        state.gauged.remove(&gone);
        state.levels.remove(&gone);
    }
    Ok(())
}

/// `node`'s kubelet summary, read as `source` says. The error is a sentence, compared between
/// rounds to log each new failure once.
async fn summary(client: &Client, node: &str, source: VolumeStatsSource) -> Result<Value, String> {
    let (client, path) = match source {
        VolumeStatsSource::Off => return Err("volume statistics are off".to_owned()),
        VolumeStatsSource::ApiServerProxy => (
            client.clone(),
            format!("/api/v1/nodes/{node}/proxy/stats/summary"),
        ),
        VolumeStatsSource::Kubelet { insecure_tls } => {
            let nodes: Api<Node> = Api::all(client.clone());
            let found = nodes
                .get(node)
                .await
                .map_err(|e| format!("reading the node: {e}"))?;
            let url = kubelet_url(&found).ok_or("the node reports no address")?;
            let mut config = kube::Config::infer()
                .await
                .map_err(|e| format!("no Kubernetes configuration: {e}"))?;
            config.cluster_url = url
                .parse()
                .map_err(|e| format!("the kubelet address {url} is not a URL: {e}"))?;
            config.accept_invalid_certs = insecure_tls;
            let kubelet =
                Client::try_from(config).map_err(|e| format!("a client for {url}: {e}"))?;
            (kubelet, "/stats/summary".to_owned())
        }
    };
    let request = http::Request::get(&path)
        .body(Vec::new())
        .map_err(|e| e.to_string())?;
    let text = client
        .request_text(request)
        .await
        .map_err(|e| format!("GET {path}: {}", with_causes(&e)))?;
    serde_json::from_str(&text).map_err(|e| format!("the summary is not JSON: {e}"))
}

/// `error` and each of its causes, `: `-separated: a connection failure's top error says only
/// "client error (Connect)", and the cause (refused, a TLS failure) is what an operator needs.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(c) = cause {
        let next = c.to_string();
        if !text.ends_with(&next) {
            text.push_str(": ");
            text.push_str(&next);
        }
        cause = c.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        DaemonEndpoint, NodeAddress, NodeDaemonEndpoints, NodeStatus, PodSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    /// The shape of what the demo's kubelet answered on 2026-10-15 (trimmed).
    fn demo_summary() -> Value {
        serde_json::json!({
            "node": { "nodeName": "tp0n1" },
            "pods": [
                {
                    "podRef": { "name": "myelin-hs-0", "namespace": "myelin" },
                    "volume": [
                        { "name": "data", "time": "2026-10-15T20:08:25Z",
                          "capacityBytes": 10_464_022_528_u64, "usedBytes": 910_262_272_u64,
                          "pvcRef": { "name": "data-myelin-hs-0", "namespace": "myelin" } },
                        { "name": "kube-api-access", "time": "2026-10-15T20:08:25Z",
                          "capacityBytes": 100, "usedBytes": 1 }
                    ]
                },
                {
                    "podRef": { "name": "bridge-signal-brandon-bb489c9f7-kbjrm", "namespace": "myelin" },
                    "volume": [
                        { "name": "data", "time": "2026-10-15T20:07:24Z",
                          "capacityBytes": 1_020_702_720_u64, "usedBytes": 119_447_552_u64,
                          "pvcRef": { "name": "bridge-signal-brandon-data", "namespace": "myelin" } }
                    ]
                },
                { "podRef": { "name": "no-volumes", "namespace": "myelin" } }
            ]
        })
    }

    #[test]
    fn a_summary_yields_each_claims_measurement() {
        let stats = parse_summary(&demo_summary());
        assert_eq!(stats.len(), 2, "{stats:?}");
        let signal = stats
            .iter()
            .find(|s| s.claim == "bridge-signal-brandon-data")
            .unwrap();
        assert_eq!(signal.namespace, "myelin");
        assert_eq!(signal.volume.used_bytes, 119_447_552);
        assert_eq!(signal.volume.capacity_bytes, 1_020_702_720);
        assert_eq!(signal.volume.observed_at, "2026-10-15T20:07:24Z");
        assert!(parse_summary(&serde_json::json!({})).is_empty());
        assert!(parse_summary(&serde_json::json!({ "pods": "nope" })).is_empty());
    }

    #[test]
    fn levels_are_ok_nearly_full_and_full() {
        assert_eq!(Level::of(0.12), Level::Ok);
        assert_eq!(Level::of(0.849), Level::Ok);
        assert_eq!(Level::of(0.85), Level::NearlyFull);
        assert_eq!(Level::of(0.94), Level::NearlyFull);
        assert_eq!(Level::of(0.95), Level::Full);
        // The demo's Slack bridge from 2026-10-14 08:00.
        assert_eq!(Level::of(0.98), Level::Full);
        assert_eq!(Level::Full.as_str(), "full");
        assert_eq!(Level::NearlyFull.as_str(), "nearly_full");
    }

    #[test]
    fn a_crossing_is_logged_once_each_way() {
        use tracing::Level as L;
        // The demo's Slack bridge: 0.09, 0.48, then 0.98.
        assert_eq!(crossing(None, Level::Ok), None);
        assert_eq!(crossing(Some(Level::Ok), Level::Ok), None);
        let (level, text) = crossing(Some(Level::Ok), Level::Full).unwrap();
        assert_eq!(level, L::WARN);
        assert!(text.contains("volume is full"), "{text}");
        assert!(text.contains("raise its volume size"), "{text}");
        // Still full on the next round: nothing new to say.
        assert_eq!(crossing(Some(Level::Full), Level::Full), None);
        let (level, text) = crossing(Some(Level::Ok), Level::NearlyFull).unwrap();
        assert_eq!(level, L::WARN);
        assert!(text.contains("nearly full"), "{text}");
        assert_eq!(
            crossing(Some(Level::NearlyFull), Level::Full).unwrap().0,
            L::WARN
        );
        // Falling from full to nearly full is not news; to below it is.
        assert_eq!(crossing(Some(Level::Full), Level::NearlyFull), None);
        assert_eq!(crossing(Some(Level::Full), Level::Ok).unwrap().0, L::INFO);
        // The operator restarted while a volume was full: it says so again, once.
        assert_eq!(crossing(None, Level::Full).unwrap().0, L::WARN);
    }

    fn volume(used: i64, capacity: i64) -> VolumeStatus {
        VolumeStatus {
            capacity_bytes: capacity,
            used_bytes: used,
            observed_at: "2026-10-15T20:00:00Z".to_owned(),
        }
    }

    #[test]
    fn a_measurement_is_written_when_it_has_changed_enough() {
        let minute = Duration::from_secs(60);
        assert!(should_record(None, &volume(1, 100), minute));
        assert!(!should_record(
            Some(&volume(500, 1000)),
            &volume(505, 1000),
            minute
        ));
        assert!(should_record(
            Some(&volume(500, 1000)),
            &volume(510, 1000),
            minute
        ));
        assert!(should_record(
            Some(&volume(500, 1000)),
            &volume(500, 2000),
            minute
        ));
        // 0.845 to 0.85 is under a step but crosses into nearly full.
        assert!(should_record(
            Some(&volume(845, 1000)),
            &volume(850, 1000),
            minute
        ));
        assert!(should_record(
            Some(&volume(500, 1000)),
            &volume(500, 1000),
            RECORD_AT_LEAST_EVERY
        ));
    }

    fn node(addresses: &[(&str, &str)], port: Option<i32>) -> Node {
        Node {
            metadata: ObjectMeta::default(),
            spec: None,
            status: Some(NodeStatus {
                addresses: Some(
                    addresses
                        .iter()
                        .map(|(t, a)| NodeAddress {
                            type_: (*t).to_owned(),
                            address: (*a).to_owned(),
                        })
                        .collect(),
                ),
                daemon_endpoints: port.map(|p| NodeDaemonEndpoints {
                    kubelet_endpoint: Some(DaemonEndpoint { port: p }),
                }),
                ..NodeStatus::default()
            }),
        }
    }

    #[test]
    fn the_kubelet_is_reached_at_the_nodes_internal_address_and_port() {
        assert_eq!(
            kubelet_url(&node(
                &[("Hostname", "tp0n1"), ("InternalIP", "10.0.0.11")],
                Some(10250)
            ))
            .as_deref(),
            Some("https://10.0.0.11:10250")
        );
        assert_eq!(
            kubelet_url(&node(&[("InternalIP", "fd00::11")], None)).as_deref(),
            Some("https://[fd00::11]:10250")
        );
        assert_eq!(
            kubelet_url(&node(&[("Hostname", "tp0n1")], Some(10255))).as_deref(),
            Some("https://tp0n1:10255")
        );
        assert_eq!(kubelet_url(&node(&[], None)), None);
    }

    #[test]
    fn bridge_pods_are_grouped_by_node() {
        let pod = |name: &str, bridge: &str, node: Option<&str>| Pod {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/instance".to_owned(),
                    bridge.to_owned(),
                )])),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                node_name: node.map(str::to_owned),
                ..PodSpec::default()
            }),
            status: None,
        };
        let grouped = bridges_by_node(&[
            pod("a-1", "bridge-a", Some("n1")),
            pod("b-1", "bridge-b", Some("n1")),
            pod("c-1", "bridge-c", Some("n2")),
            pod("d-1", "bridge-d", None),
        ]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped["n1"].len(), 2);
        assert!(grouped["n2"].contains("bridge-c"));
    }
}
