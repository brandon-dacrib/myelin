//! Hot reload: taking a configuration change on in a running `hs serve`.
//!
//! [`LiveConfig`] is the one place that knows both which settings are hot
//! (`hs_config::reload::HOT_SETTINGS`) and what in this process re-reads each of them. The parts
//! of the server that can take a change on register an applier for their section at startup
//! ([`LiveConfig::on_change`]); [`LiveConfig::apply`] is called with every configuration the
//! store resolves to after a write (`crate::config_source`), runs the appliers of the sections
//! whose hot settings changed, and says what it did. It is called from the store's side of a
//! write rather than from any HTTP handler, so an update, a reload and a revert all hot-apply
//! the same way.
//!
//! What is wired:
//!
//! - `rate_limits` — the room layer's [`hs_room::moderation::SendLimiter`] gets the new
//!   server-wide `message` limit ([`message_limit`]).
//! - `migration` — read when a migration starts; nothing to swap.
//! - `federation` (its allow and block lists) — the outbound client's
//!   [`hs_federation::client::DomainPolicy`] and [`hs_federation::client::IpPolicy`] are
//!   replaced in place. Only when federation is enabled.
//! - `network` (`outbound.ipv4_only`) — [`apply_network`] puts the outbound address policy in
//!   force for every client in the process (`hs_http::outbound`, read per new connection).
//! - `telemetry` (its log level) — [`hs_telemetry::LogLevelHandle::set_level`], wired in
//!   `crate::cli`'s `run_serve`, which owns the telemetry guard. Not when `RUST_LOG` set the
//!   filter: the change then waits for a restart and is reported so.
//!
//! Every section applied, failed or found unwired is logged, and counted in
//! `hs_config_reloads_total{section,outcome}` (`applied`, `failed`, `unwired`).

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use hs_config::Config;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use serde_json::Value;

/// Something in the running server that re-reads a section: given the whole new configuration,
/// it swaps in what it needs, or says why it could not.
pub type Applier = Box<dyn Fn(&Config) -> Result<(), String> + Send + Sync>;

/// The labels of `hs_config_reloads_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct ReloadLabels {
    section: String,
    outcome: &'static str,
}

/// The labels of `hs_config_settings_applied_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct SettingLabels {
    setting: &'static str,
    outcome: &'static str,
}

/// Process-wide, like the other counters registered into each server's registry: a counter is
/// only an atomic, and the appliers run far from any registry.
static RELOADS: LazyLock<Family<ReloadLabels, Counter>> = LazyLock::new(Family::default);

/// Per hot setting (`hs_config::reload::HOT_SETTINGS`, a bounded set of label values).
static SETTINGS_APPLIED: LazyLock<Family<SettingLabels, Counter>> = LazyLock::new(Family::default);

fn count(section: &str, outcome: &'static str) {
    RELOADS
        .get_or_create(&ReloadLabels {
            section: section.to_owned(),
            outcome,
        })
        .inc();
}

/// Logs and counts one changed hot setting's fate.
fn note_setting(setting: &'static str, outcome: &'static str, reason: Option<&str>) {
    SETTINGS_APPLIED
        .get_or_create(&SettingLabels { setting, outcome })
        .inc();
    match outcome {
        "applied" => tracing::info!(
            setting,
            "configuration setting applied to the running server"
        ),
        _ => tracing::warn!(
            setting,
            outcome,
            reason = reason.unwrap_or("nothing in this process re-reads it"),
            "configuration setting changed but not applied; the old value stays in force"
        ),
    }
}

/// Registers `hs_config_reloads_total{section,outcome}` into `registry`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_config_reloads",
        "Configuration sections taken on by the running server after a change, by section and \
         outcome: applied, failed (the old value stays in force), unwired (nothing in this \
         process re-reads it)",
        RELOADS.clone(),
    );
    registry.register(
        "hs_config_settings_applied",
        "Hot configuration settings that changed, by setting (a JSON Pointer from \
         hs_config::reload::SETTINGS) and outcome: applied, failed or unwired",
        SETTINGS_APPLIED.clone(),
    );
}

/// Puts `config.network.outbound` in force for every outbound client in this process
/// (`hs_http::outbound`, read per new connection) and logs the policy as `outbound: IPv4 only`
/// or `outbound: IPv4 and IPv6`. Called at boot and by the `network` section's applier.
pub fn apply_network(config: &Config) {
    hs_http::outbound::set_ipv4_only(config.network.outbound.ipv4_only);
    let policy = hs_http::outbound::describe();
    tracing::info!(
        ipv4_only = config.network.outbound.ipv4_only,
        "outbound: {policy}"
    );
}

/// What one [`LiveConfig::apply`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Sections whose changed hot settings are now in force.
    pub reloaded: Vec<String>,
    /// Sections whose change could not be applied, with why. The old value stays in force, and
    /// the next apply tries again.
    pub failed: Vec<(String, String)>,
    /// Sections holding a setting that differs from what this process booted on and is only
    /// read at startup.
    pub requires_restart: Vec<String>,
}

struct Inner {
    /// The hot settings in force, as the whole configuration's JSON: what the next change is
    /// compared against.
    running: Value,
    appliers: BTreeMap<&'static str, Vec<Applier>>,
}

/// The configuration a running server is on, and what re-reads each hot section of it. See the
/// module docs.
pub struct LiveConfig {
    booted: Config,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveConfig")
    }
}

impl LiveConfig {
    /// A server that booted on `booted`, with nothing wired yet.
    #[must_use]
    pub fn new(booted: Config) -> Self {
        let running = serde_json::to_value(&booted).unwrap_or(Value::Null);
        Self {
            booted,
            inner: Mutex::new(Inner {
                running,
                appliers: BTreeMap::new(),
            }),
        }
    }

    /// The configuration this process booted on.
    #[must_use]
    pub fn booted(&self) -> &Config {
        &self.booted
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wires `applier` to `section`: from now on it runs whenever a hot setting in `section`
    /// changes. A section may have several.
    pub fn on_change(
        &self,
        section: &'static str,
        applier: impl Fn(&Config) -> Result<(), String> + Send + Sync + 'static,
    ) {
        self.lock()
            .appliers
            .entry(section)
            .or_default()
            .push(Box::new(applier));
    }

    /// Whether a hot setting still differs from what is running, including changes whose
    /// appliers failed or were not registered yet. Store followers retry these changes even
    /// when the stored revision has not moved.
    #[must_use]
    pub fn has_pending(&self, config: &Config) -> bool {
        let Ok(next) = serde_json::to_value(config) else {
            return false;
        };
        let inner = self.lock();
        !hs_config::reload::hot_settings_changed(&inner.running, &next).is_empty()
    }

    /// Takes `new` on: every section in which a hot setting differs from what is in force has
    /// its appliers run. Reports those sections, and every section that differs from what the
    /// process booted on in a setting that only a restart reads.
    pub fn apply(&self, new: &Config) -> Applied {
        let requires_restart: Vec<String> =
            hs_config::reload::sections_requiring_restart(&self.booted, new)
                .into_iter()
                .map(str::to_owned)
                .collect();
        let Ok(mut next) = serde_json::to_value(new) else {
            return Applied {
                requires_restart,
                ..Applied::default()
            };
        };

        let mut inner = self.lock();
        let changed_settings = hs_config::reload::hot_settings_changed(&inner.running, &next);
        let mut changed: Vec<&'static str> = Vec::new();
        for pointer in &changed_settings {
            if let Some(section) = hs_config::reload::section_name(pointer)
                && !changed.contains(&section)
            {
                changed.push(section);
            }
        }
        let settings_of = |section: &'static str| -> Vec<&'static str> {
            changed_settings
                .iter()
                .copied()
                .filter(|pointer| hs_config::reload::section_name(pointer) == Some(section))
                .collect()
        };

        let mut applied = Applied {
            requires_restart,
            ..Applied::default()
        };
        for section in changed {
            let outcome = match inner.appliers.get(section) {
                None => Err(None),
                Some(appliers) => appliers
                    .iter()
                    .try_for_each(|applier| applier(new))
                    .map_err(Some),
            };
            match outcome {
                Ok(()) => {
                    tracing::info!(section, "configuration section reloaded");
                    count(section, "applied");
                    settings_of(section)
                        .into_iter()
                        .for_each(|setting| note_setting(setting, "applied", None));
                    applied.reloaded.push(section.to_owned());
                }
                Err(Some(reason)) => {
                    tracing::warn!(section, %reason, "configuration section could not be reloaded; the old value stays in force");
                    count(section, "failed");
                    settings_of(section)
                        .into_iter()
                        .for_each(|setting| note_setting(setting, "failed", Some(&reason)));
                    keep_running(&inner.running, &mut next, section);
                    applied.failed.push((section.to_owned(), reason));
                }
                Err(None) => {
                    tracing::warn!(
                        section,
                        "configuration section changed, but nothing in this process re-reads it; it takes effect at the next restart"
                    );
                    count(section, "unwired");
                    settings_of(section)
                        .into_iter()
                        .for_each(|setting| note_setting(setting, "unwired", None));
                    keep_running(&inner.running, &mut next, section);
                    if !applied.requires_restart.iter().any(|s| s == section) {
                        applied.requires_restart.push(section.to_owned());
                    }
                }
            }
        }
        inner.running = next;
        applied
    }
}

/// Puts `section`'s hot settings in `next` back to what `running` has, so a change that did not
/// take is seen as a change again next time.
fn keep_running(running: &Value, next: &mut Value, section: &str) {
    for pointer in hs_config::reload::HOT_SETTINGS.iter() {
        if hs_config::document::section_of(pointer) != Some(section) {
            continue;
        }
        let old = running.pointer(pointer).cloned().unwrap_or(Value::Null);
        if let Some(slot) = next.pointer_mut(pointer) {
            *slot = old;
        }
    }
}

/// The server-wide send limit `rate_limits` says is in force: `rate_limits.message` while
/// `rate_limits.enabled`, and nothing -- nobody without an override limited -- otherwise, or
/// when its rate is `0`.
#[must_use]
pub fn message_limit(
    config: &hs_config::RateLimitConfig,
) -> Option<hs_auth::store::RateLimitOverrideRecord> {
    (config.enabled && config.message.per_second > 0.0).then_some(
        hs_auth::store::RateLimitOverrideRecord {
            per_second: config.message.per_second,
            burst_count: config.message.burst_count,
        },
    )
}

/// The limit one `rate_limits` bucket says is in force: `bucket` while `rate_limits.enabled`, and
/// none otherwise (a rate of `0` is none too, which [`hs_http::buckets::TokenBuckets`] decides).
#[must_use]
pub fn bucket_limit(
    config: &hs_config::RateLimitConfig,
    bucket: &hs_config::ratelimit::RateLimitBucket,
) -> Option<hs_http::buckets::BucketLimit> {
    config.enabled.then_some(hs_http::buckets::BucketLimit {
        per_second: bucket.per_second,
        burst_count: bucket.burst_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn with_message(per_second: f64, burst_count: u32) -> Config {
        let mut config = Config::default();
        config.rate_limits.message.per_second = per_second;
        config.rate_limits.message.burst_count = burst_count;
        config
    }

    #[test]
    fn a_hot_change_runs_its_appliers_once_and_is_reported() {
        let live = LiveConfig::new(Config::default());
        let seen = Arc::new(AtomicU32::new(0));
        let counter = seen.clone();
        live.on_change("rate_limits", move |config| {
            counter.store(config.rate_limits.message.burst_count, Ordering::SeqCst);
            Ok(())
        });

        let new = with_message(0.5, 3);
        let applied = live.apply(&new);
        assert_eq!(applied.reloaded, vec!["rate_limits"]);
        assert!(applied.requires_restart.is_empty());
        assert!(applied.failed.is_empty());
        assert_eq!(seen.load(Ordering::SeqCst), 3);

        // The same configuration again changes nothing, so nothing is re-applied.
        seen.store(0, Ordering::SeqCst);
        assert_eq!(live.apply(&new), Applied::default());
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_cold_change_is_reported_against_what_the_process_booted_on() {
        let live = LiveConfig::new(Config::default());
        live.on_change("rate_limits", |_| Ok(()));
        let mut new = with_message(0.5, 3);
        new.server.public_baseurl = Some("https://matrix.example.org".to_owned());
        let applied = live.apply(&new);
        assert_eq!(applied.reloaded, vec!["rate_limits"]);
        assert_eq!(applied.requires_restart, vec!["server"]);
        // Still pending on the next apply, because the process still runs on the old value.
        assert_eq!(live.apply(&new).requires_restart, vec!["server"]);
    }

    #[test]
    fn a_failed_or_unwired_change_stays_pending() {
        let live = LiveConfig::new(Config::default());
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failing = fail.clone();
        live.on_change("rate_limits", move |_| {
            if failing.load(Ordering::SeqCst) {
                Err("no".to_owned())
            } else {
                Ok(())
            }
        });
        let new = with_message(0.5, 3);
        let applied = live.apply(&new);
        assert_eq!(
            applied.failed,
            vec![("rate_limits".to_owned(), "no".to_owned())]
        );
        assert!(applied.reloaded.is_empty());
        // Tried again next time, and taken once it works.
        fail.store(false, Ordering::SeqCst);
        assert_eq!(live.apply(&new).reloaded, vec!["rate_limits"]);

        // A hot section nothing in the process registered for waits for a restart.
        let unwired = LiveConfig::new(Config::default());
        let applied = unwired.apply(&new);
        assert_eq!(applied.requires_restart, vec!["rate_limits"]);
        assert!(applied.reloaded.is_empty());
    }

    #[test]
    fn the_message_limit_follows_the_master_switch() {
        let mut config = hs_config::RateLimitConfig::default();
        let limit = message_limit(&config).unwrap();
        assert_eq!(limit.burst_count, config.message.burst_count);
        config.enabled = false;
        assert!(message_limit(&config).is_none());
        config.enabled = true;
        config.message.per_second = 0.0;
        assert!(message_limit(&config).is_none());
    }
}
