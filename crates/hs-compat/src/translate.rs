//! The `homeserver.yaml` translator: parses a Synapse configuration file,
//! writes every mapped or mapped-with-a-difference key onto a fresh
//! `hs_config::Config`, and produces a [`TranslationReport`] covering every
//! key the file sets — mapped, mapped-with-a-difference, unsupported, or
//! entirely unrecognized. See `docs/compat/synapse-config-table.md` for the
//! classification of every key and `docs/status/13-config-compat-and-migration.md`
//! for the current state of this implementation.
//!
//! Unsupported and unrecognized keys **present in the source file** fail
//! translation unless [`TranslateOptions::allow_unsupported`] is set — this
//! deliberately does not try to reproduce Synapse's own default for each
//! of the 182 unsupported options (infeasible to keep in sync release over
//! release); a key an operator bothered to write down is a decision that
//! must be acknowledged, not silently dropped, whether or not its value
//! happens to match what Synapse would have used anyway.

use std::path::PathBuf;
use std::str::FromStr;

use hs_config::auth::{MasDelegationConfig, OidcProviderConfig, PasswordConfig, PasswordPolicy};
use hs_config::listeners::{Listener, ListenerResource, TlsConfig};
use hs_config::media::{MediaStorageBackend, ThumbnailMethod, ThumbnailSize};
use hs_config::ratelimit::RateLimitBucket;
use hs_config::secret::SecretString;
use hs_config::storage::{PostgresStorageConfig, StorageConfig};
use hs_config::{ByteSize, Config, Duration};
use serde_yaml_ng::Value;

use crate::classification::{self, Classification};
use crate::report::{OutcomeClassification, TranslationReport};

/// Options controlling translation.
#[derive(Debug, Clone, Copy, Default)]
pub struct TranslateOptions {
    /// Proceed even when the source sets unsupported or unrecognized keys.
    /// Corresponds to the `--allow-unsupported-synapse-config` CLI flag
    /// (`docs/compat/cli-shims.md`).
    pub allow_unsupported: bool,
}

/// Errors from [`translate`].
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The source was not valid YAML.
    #[error("failed to parse Synapse config as YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    /// The source set one or more unsupported/unrecognized keys and
    /// `allow_unsupported` was not set.
    #[error(
        "{} unsupported or unrecognized Synapse option(s) found; re-run with \
         --allow-unsupported-synapse-config to proceed anyway:\n{}",
        keys.len(),
        keys.join("\n")
    )]
    Unsupported {
        /// One line per blocking key, e.g. `` `gc_thresholds`: R-PY. ``.
        keys: Vec<String>,
    },
    /// The translated configuration failed `hs-config`'s own validation.
    #[error("translated configuration is invalid: {0}")]
    Config(#[from] hs_config::ConfigError),
}

/// Translates a Synapse `homeserver.yaml` (as a string) into a native
/// [`Config`], returning the config alongside a full [`TranslationReport`].
///
/// On success (or when `options.allow_unsupported` masks a failure), the
/// returned `Config` has already had `resolve_secrets` and `validate`
/// applied, exactly as `Config::load` would.
pub fn translate(
    synapse_yaml: &str,
    options: TranslateOptions,
) -> Result<(Config, TranslationReport), TranslateError> {
    let doc: Value = serde_yaml_ng::from_str(synapse_yaml)?;
    let mapping = doc.as_mapping().cloned().unwrap_or_default();

    let mut config = Config::default();
    let mut report = TranslationReport::new();

    // --- Pass 1: keys later steps depend on ---
    let global_tls = TlsPaths {
        cert: get_path(&doc, "tls_certificate_path"),
        key: get_path(&doc, "tls_private_key_path"),
    };
    translate_listeners(&doc, &mut config, &global_tls);
    if let Some(enabled) = get_bool(&doc, "enable_media_repo")
        && !enabled
    {
        for l in &mut config.listeners.listeners {
            l.resources.retain(|r| *r != ListenerResource::Media);
        }
    }

    // --- Pass 2: every other top-level key, independent of order ---
    for (k, v) in &mapping {
        let Some(key) = k.as_str() else { continue };
        // `tls_certificate_path`, `tls_private_key_path`, `listeners` and
        // `enable_media_repo` were already applied in pass 1 above;
        // `translate_key`'s arm for them is a no-op, so falling through to
        // `apply_and_report` here just records their report entry once.
        if key == "experimental_features" {
            translate_experimental(v, &mut config, &mut report);
            continue;
        }
        apply_and_report(key, v, &mut config, &mut report);
    }

    if !options.allow_unsupported && report.has_blocking() {
        let keys = report
            .blocking()
            .map(|o| format!("`{}`: {}", o.key, o.note))
            .collect();
        return Err(TranslateError::Unsupported { keys });
    }

    config.resolve_secrets()?;
    config.validate()?;

    Ok((config, report))
}

fn apply_and_report(key: &str, value: &Value, config: &mut Config, report: &mut TranslationReport) {
    let Some(info) = classification::lookup_option(key) else {
        report.record(
            key,
            OutcomeClassification::Unrecognized,
            "",
            "not in docs/synapse-inventory.md as of the pinned Synapse release",
        );
        return;
    };
    if info.classification != Classification::Unsupported {
        translate_key(key, value, config);
    }
    report.record(key, info.classification.into(), info.native, info.note);
}

fn translate_experimental(value: &Value, config: &mut Config, report: &mut TranslationReport) {
    let Some(mapping) = value.as_mapping() else {
        return;
    };
    for (k, v) in mapping {
        let Some(flag) = k.as_str() else { continue };
        let dotted = format!("experimental_features.{flag}");
        let Some(info) = classification::lookup_experimental(flag) else {
            report.record(
                dotted,
                OutcomeClassification::Unrecognized,
                "",
                "not in docs/synapse-inventory.md as of the pinned Synapse release",
            );
            continue;
        };
        if info.classification != Classification::Unsupported {
            translate_experimental_key(flag, v, config);
        }
        report.record(dotted, info.classification.into(), info.native, info.note);
    }
}

// ---------------------------------------------------------------------
// Value extraction helpers
// ---------------------------------------------------------------------

fn get<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    doc.as_mapping()?.get(Value::String(key.to_owned()))
}

fn get_str(doc: &Value, key: &str) -> Option<String> {
    get(doc, key)?.as_str().map(str::to_owned)
}

fn get_bool(doc: &Value, key: &str) -> Option<bool> {
    get(doc, key)?.as_bool()
}

fn get_u64(doc: &Value, key: &str) -> Option<u64> {
    get(doc, key)?.as_u64()
}

fn get_u32(doc: &Value, key: &str) -> Option<u32> {
    get_u64(doc, key).and_then(|v| u32::try_from(v).ok())
}

fn get_f64(doc: &Value, key: &str) -> Option<f64> {
    get(doc, key)
        .and_then(Value::as_f64)
        .or_else(|| get_u64(doc, key).map(|v| v as f64))
}

fn get_path(doc: &Value, key: &str) -> Option<PathBuf> {
    get_str(doc, key).map(PathBuf::from)
}

fn get_str_list(doc: &Value, key: &str) -> Option<Vec<String>> {
    let seq = get(doc, key)?.as_sequence()?;
    Some(
        seq.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
    )
}

fn get_duration(doc: &Value, key: &str) -> Option<Duration> {
    let v = get(doc, key)?;
    if let Some(s) = v.as_str() {
        Duration::from_str(s).ok()
    } else {
        v.as_u64().map(Duration::from_millis)
    }
}

fn rate_limit_bucket(doc: &Value) -> Option<RateLimitBucket> {
    let per_second = get_f64(doc, "per_second")?;
    let burst_count = get_u32(doc, "burst_count")?;
    Some(RateLimitBucket {
        per_second,
        burst_count,
    })
}

// ---------------------------------------------------------------------
// Listeners (and the TLS/media-repo keys that interact with them)
// ---------------------------------------------------------------------

struct TlsPaths {
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
}

fn translate_listeners(doc: &Value, config: &mut Config, global_tls: &TlsPaths) {
    let Some(seq) = get(doc, "listeners").and_then(Value::as_sequence) else {
        return;
    };
    let mut listeners = Vec::new();
    for entry in seq {
        if get_str(entry, "type")
            .as_deref()
            .is_some_and(|t| t != "http")
        {
            // manhole/metrics-legacy listener types have no native
            // equivalent (R-PROC / folded into the `metrics` resource);
            // skip rather than mistranslate.
            continue;
        }
        let port = get_u64(entry, "port")
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(8008);
        let bind_addresses =
            get_str_list(entry, "bind_addresses").unwrap_or_else(|| vec!["::".to_owned()]);
        let x_forwarded = get_bool(entry, "x_forwarded").unwrap_or(false);
        let tls = if get_bool(entry, "tls").unwrap_or(false) {
            match (&global_tls.cert, &global_tls.key) {
                (Some(c), Some(k)) => Some(TlsConfig {
                    certificate_path: c.clone(),
                    private_key_path: k.clone(),
                }),
                _ => None,
            }
        } else {
            None
        };
        let mut resources = Vec::new();
        if let Some(res_seq) = get(entry, "resources").and_then(Value::as_sequence) {
            for res in res_seq {
                if let Some(names) = get_str_list(res, "names") {
                    for name in names {
                        if let Some(r) = listener_resource(&name) {
                            resources.push(r);
                        }
                    }
                }
            }
        }
        if resources.is_empty() {
            continue;
        }
        listeners.push(Listener {
            bind_addresses,
            port,
            tls,
            resources,
            x_forwarded,
        });
    }
    if !listeners.is_empty() {
        config.listeners.listeners = listeners;
    }
}

fn listener_resource(name: &str) -> Option<ListenerResource> {
    Some(match name {
        "client" => ListenerResource::Client,
        "federation" => ListenerResource::Federation,
        "media" => ListenerResource::Media,
        "metrics" => ListenerResource::Metrics,
        // "replication" (R-WORKER) and anything else: no native resource.
        _ => return None,
    })
}

// ---------------------------------------------------------------------
// Per-key translation
// ---------------------------------------------------------------------

fn translate_key(key: &str, v: &Value, config: &mut Config) {
    match key {
        "server_name" => {
            if let Some(s) = v.as_str() {
                config.server.server_name = s.to_owned();
            }
        }
        "public_baseurl" => config.server.public_baseurl = v.as_str().map(str::to_owned),
        "admin_contact" => config.server.admin_contact = v.as_str().map(str::to_owned),
        "report_stats" => {
            if let Some(b) = v.as_bool() {
                config.server.report_stats = b;
            }
        }
        "signing_key_path" => {
            if let Some(s) = v.as_str() {
                config.server.signing_key_path = PathBuf::from(s);
            }
        }

        "tls_certificate_path" | "tls_private_key_path" | "listeners" | "enable_media_repo" => {
            // Handled in pass 1.
        }

        "federation_domain_whitelist" => {
            if let Some(seq) = v.as_sequence() {
                config.federation.domain_allowlist = Some(
                    seq.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect(),
                );
            }
        }
        "federation_verify_certificates" => {
            if let Some(b) = v.as_bool() {
                config.federation.verify_certificates = b;
            }
        }
        "allow_public_rooms_over_federation" => {
            if let Some(b) = v.as_bool() {
                config.federation.allow_public_rooms_over_federation = b;
            }
        }
        "allow_device_name_lookup_over_federation" => {
            if let Some(b) = v.as_bool() {
                config.federation.allow_device_name_lookup_over_federation = b;
            }
        }
        "ip_range_blacklist" => {
            if let Some(seq) = v.as_sequence() {
                let list: Vec<String> = seq
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect();
                config.federation.ip_range_blocklist = list.clone();
                config.media.url_preview_ip_range_blocklist = list;
            }
        }
        "ip_range_whitelist" => {
            if let Some(seq) = v.as_sequence() {
                config.federation.ip_range_allowlist = seq
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect();
            }
        }
        "federation" => {
            if let Some(d) = get_duration(v, "client_timeout") {
                config.federation.client_timeout = d;
            }
            let long = get_duration(v, "max_long_retry_delay");
            let short = get_duration(v, "max_short_retry_delay");
            if let Some(d) = long.or(short) {
                config.federation.max_retry_backoff = d;
            }
        }

        "rc_message" => {
            if let Some(b) = rate_limit_bucket(v) {
                config.rate_limits.message = b;
            }
        }
        "rc_registration" => {
            if let Some(b) = rate_limit_bucket(v) {
                config.rate_limits.registration = b;
            }
        }
        "rc_login" => {
            if let Some(sub) = get(v, "address").or(Some(v))
                && let Some(b) = rate_limit_bucket(sub)
            {
                config.rate_limits.login = b;
            }
        }
        "rc_admin_redaction" => {
            if let Some(b) = rate_limit_bucket(v) {
                config.rate_limits.admin_redaction = b;
            }
        }
        "rc_joins" => {
            if let Some(local) = get(v, "local").and_then(rate_limit_bucket) {
                config.rate_limits.joins_local = local;
            }
            if let Some(remote) = get(v, "remote").and_then(rate_limit_bucket) {
                config.rate_limits.joins_remote = remote;
            }
        }
        "rc_3pid_validation" => {
            if let Some(b) = rate_limit_bucket(v) {
                config.rate_limits.third_party_id_validation = b;
            }
        }
        "rc_federation" => {
            // Synapse's five-field sliding window has no 1:1 mapping;
            // approximate with reject_limit as the burst and window_size
            // (converted to a per-second rate) as the steady rate.
            let window_ms = get_u64(v, "window_size").unwrap_or(1000).max(1);
            let sleep_limit = get_u64(v, "sleep_limit").unwrap_or(10);
            let reject_limit = get_u32(v, "reject_limit").unwrap_or(50);
            config.rate_limits.federation = RateLimitBucket {
                per_second: sleep_limit as f64 * 1000.0 / window_ms as f64,
                burst_count: reject_limit.max(1),
            };
        }

        "media_store_path" => {
            if let Some(s) = v.as_str() {
                config.media.storage = MediaStorageBackend::Local {
                    path: PathBuf::from(s),
                };
            }
        }
        "media_storage_providers" => {
            // Provider-module configuration is not auto-translated (see
            // docs/compat/synapse-config-table.md); recorded for manual
            // review only.
        }
        "max_upload_size" => {
            if let Some(sz) = get_bytesize_value(v) {
                config.media.max_upload_size = sz;
            }
        }
        "thumbnail_sizes" => {
            if let Some(seq) = v.as_sequence() {
                let sizes: Vec<ThumbnailSize> = seq
                    .iter()
                    .filter_map(|t| {
                        Some(ThumbnailSize {
                            width: get_u32(t, "width")?,
                            height: get_u32(t, "height")?,
                            method: match get_str(t, "method").as_deref() {
                                Some("scale") => ThumbnailMethod::Scale,
                                _ => ThumbnailMethod::Crop,
                            },
                        })
                    })
                    .collect();
                if !sizes.is_empty() {
                    config.media.thumbnail_sizes = sizes;
                }
            }
        }
        "url_preview_enabled" => {
            if let Some(b) = v.as_bool() {
                config.media.url_preview_enabled = b;
            }
        }
        "url_preview_ip_range_blacklist" => {
            if let Some(seq) = v.as_sequence() {
                config.media.url_preview_ip_range_blocklist = seq
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect();
            }
        }
        "media_retention" => {
            if let Some(d) = get_duration(v, "remote_media_lifetime") {
                config.media.remote_media_retention = Some(d);
            }
        }
        "enable_authenticated_media" => {
            if let Some(b) = v.as_bool() {
                // Inverted: see docs/compat/synapse-config-table.md.
                config.media.allow_legacy_unauthenticated_media = !b;
            }
        }

        "enable_registration" => {
            if let Some(b) = v.as_bool() {
                config.auth.enable_registration = b;
            }
        }
        "registration_shared_secret" => {
            if let Some(s) = v.as_str() {
                config.auth.registration_shared_secret = SecretString::from(s);
            }
        }
        "registration_shared_secret_path" => {
            if let Some(s) = v.as_str() {
                config.auth.registration_shared_secret_file = Some(PathBuf::from(s));
            }
        }
        "macaroon_secret_key" => {
            if let Some(s) = v.as_str() {
                config.auth.session_secret = SecretString::from(s);
            }
        }
        "macaroon_secret_key_path" => {
            if let Some(s) = v.as_str() {
                config.auth.session_secret_file = Some(PathBuf::from(s));
            }
        }
        "refresh_token_lifetime" => {
            if let Some(d) = get_duration_value(v) {
                config.auth.refresh_token_lifetime = Some(d);
            }
        }
        "refreshable_access_token_lifetime" => {
            if let Some(d) = get_duration_value(v) {
                config.auth.access_token_lifetime = d;
            }
        }
        "password_config" => {
            let mut password = PasswordConfig::default();
            if let Some(b) = get_bool(v, "enabled") {
                password.enabled = b;
            }
            if let Some(s) = get_str(v, "pepper") {
                password.pepper = SecretString::from(s);
            }
            if let Some(policy) = get(v, "policy") {
                let mut p = PasswordPolicy::default();
                if let Some(n) = get_u32(policy, "minimum_length") {
                    p.minimum_length = n;
                }
                p.require_digit = get_bool(policy, "require_digit").unwrap_or(false);
                p.require_symbol = get_bool(policy, "require_symbol").unwrap_or(false);
                p.require_uppercase = get_bool(policy, "require_uppercase").unwrap_or(false);
                p.require_lowercase = get_bool(policy, "require_lowercase").unwrap_or(false);
                password.policy = p;
            }
            config.auth.password = password;
        }
        "oidc_providers" => {
            if let Some(seq) = v.as_sequence() {
                let providers: Vec<OidcProviderConfig> = seq
                    .iter()
                    .filter_map(|p| {
                        Some(OidcProviderConfig {
                            idp_id: get_str(p, "idp_id")?,
                            idp_name: get_str(p, "idp_name"),
                            issuer: get_str(p, "issuer")?,
                            client_id: get_str(p, "client_id")?,
                            client_secret: get_str(p, "client_secret")
                                .map(SecretString::from)
                                .unwrap_or_default(),
                            client_secret_file: get_path(p, "client_secret_path"),
                            scopes: get_str_list(p, "scopes")
                                .unwrap_or_else(|| vec!["openid".into(), "profile".into()]),
                        })
                    })
                    .collect();
                config.auth.oidc_providers = providers;
            }
        }
        "matrix_authentication_service" => apply_mas(v, config),

        "app_service_config_files" => {
            if let Some(list) = get_str_list_value(v) {
                config.appservices.registration_files =
                    list.into_iter().map(PathBuf::from).collect();
            }
        }

        "enable_metrics" => {
            if let Some(b) = v.as_bool() {
                config.telemetry.metrics.enabled = b;
            }
        }
        "sentry" => {
            if let Some(dsn) = get_str(v, "dsn") {
                config.telemetry.sentry = Some(hs_config::telemetry::SentryConfig {
                    dsn: SecretString::from(dsn),
                    dsn_file: None,
                    environment: "production".to_owned(),
                });
            }
        }
        "opentracing" => {
            // Synapse's block configures a Jaeger agent host/port, not an
            // OTLP endpoint; there is nothing to translate `enabled: true`
            // into without also inventing a destination (and hs-config
            // rejects `tracing.enabled: true` with no `otlp_endpoint`).
            // Recorded in the report for manual follow-up only.
        }
        "log_config" => {
            // Points at an external Python dictConfig file; not parsed.
            // Native logging stays at its own defaults for manual review.
        }
        "database" => apply_database(v, config),

        _ => {}
    }
}

fn get_bytesize_value(v: &Value) -> Option<ByteSize> {
    if let Some(s) = v.as_str() {
        ByteSize::from_str(s).ok()
    } else {
        v.as_u64().map(ByteSize::bytes)
    }
}

fn get_duration_value(v: &Value) -> Option<Duration> {
    if let Some(s) = v.as_str() {
        Duration::from_str(s).ok()
    } else {
        v.as_u64().map(Duration::from_millis)
    }
}

fn get_str_list_value(v: &Value) -> Option<Vec<String>> {
    v.as_sequence().map(|seq| {
        seq.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

fn apply_mas(v: &Value, config: &mut Config) {
    if !get_bool(v, "enabled").unwrap_or(false) {
        return;
    }
    let Some(endpoint) = get_str(v, "endpoint") else {
        return;
    };
    let secret = get_str(v, "secret")
        .map(SecretString::from)
        .unwrap_or_default();
    let secret_file = get_path(v, "secret_path");
    config.auth.mas_delegation = Some(MasDelegationConfig {
        endpoint,
        shared_secret: secret,
        shared_secret_file: secret_file,
    });
}

fn apply_database(v: &Value, config: &mut Config) {
    if get_str(v, "name").as_deref() != Some("psycopg2") {
        // sqlite3 (or unset): no native equivalent for the file-based
        // engine; leave the embedded backend at its default.
        return;
    }
    let args = get(v, "args").cloned().unwrap_or(Value::Null);
    let host = get_str(&args, "host").unwrap_or_else(|| "localhost".to_owned());
    let port = get_u64(&args, "port")
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(5432);
    let database = get_str(&args, "database")
        .or_else(|| get_str(&args, "dbname"))
        .unwrap_or_else(|| "synapse".to_owned());
    let user = get_str(&args, "user").unwrap_or_else(|| "synapse".to_owned());
    let password = get_str(&args, "password")
        .map(SecretString::from)
        .unwrap_or_default();
    let pool_size = get_u32(&args, "cp_max").unwrap_or(10);
    config.storage = StorageConfig::Postgres(PostgresStorageConfig {
        host,
        port,
        database,
        user,
        password,
        password_file: None,
        pool_size,
        tls: false,
    });
}

fn translate_experimental_key(flag: &str, v: &Value, config: &mut Config) {
    if flag == "msc3861" {
        if !get_bool(v, "enabled").unwrap_or(false) {
            return;
        }
        // Already configured via the stable `matrix_authentication_service`
        // block: that one wins.
        if config.auth.mas_delegation.is_some() {
            return;
        }
        let Some(endpoint) = get_str(v, "issuer") else {
            return;
        };
        let secret = get_str(v, "client_secret")
            .or_else(|| get_str(v, "admin_token"))
            .map(SecretString::from)
            .unwrap_or_default();
        config.auth.mas_delegation = Some(MasDelegationConfig {
            endpoint,
            shared_secret: secret,
            shared_secret_file: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_a_minimal_config() {
        let yaml = "server_name: example.org\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.server.server_name, "example.org");
        assert!(!report.has_blocking());
    }

    #[test]
    fn unsupported_key_blocks_by_default() {
        let yaml = "server_name: example.org\ngc_thresholds: [100, 10, 10]\n";
        let err = translate(yaml, TranslateOptions::default()).unwrap_err();
        assert!(matches!(err, TranslateError::Unsupported { .. }));
    }

    #[test]
    fn unsupported_key_is_allowed_with_the_override() {
        let yaml = "server_name: example.org\ngc_thresholds: [100, 10, 10]\n";
        let (config, report) = translate(
            yaml,
            TranslateOptions {
                allow_unsupported: true,
            },
        )
        .unwrap();
        assert_eq!(config.server.server_name, "example.org");
        assert!(report.has_blocking());
    }

    #[test]
    fn unrecognized_key_blocks_by_default() {
        let yaml = "server_name: example.org\nthis_is_not_a_real_synapse_option: true\n";
        let err = translate(yaml, TranslateOptions::default()).unwrap_err();
        assert!(matches!(err, TranslateError::Unsupported { .. }));
    }

    #[test]
    fn translates_rate_limits() {
        let yaml = "server_name: example.org\nrc_login:\n  address:\n    per_second: 0.5\n    burst_count: 6\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.rate_limits.login.per_second, 0.5);
        assert_eq!(config.rate_limits.login.burst_count, 6);
    }

    #[test]
    fn translates_listeners_with_tls() {
        let yaml = r#"
server_name: example.org
tls_certificate_path: /etc/hs/cert.pem
tls_private_key_path: /etc/hs/key.pem
listeners:
  - port: 8448
    type: http
    tls: true
    resources:
      - names: [client, federation]
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.listeners.listeners.len(), 1);
        let l = &config.listeners.listeners[0];
        assert_eq!(l.port, 8448);
        assert!(l.resources.contains(&ListenerResource::Client));
        assert!(l.resources.contains(&ListenerResource::Federation));
        assert_eq!(
            l.tls.as_ref().unwrap().certificate_path,
            PathBuf::from("/etc/hs/cert.pem")
        );
    }

    #[test]
    fn enable_media_repo_false_strips_media_resource() {
        let yaml = r#"
server_name: example.org
enable_media_repo: false
listeners:
  - port: 8008
    type: http
    resources:
      - names: [client, media]
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert!(
            !config.listeners.listeners[0]
                .resources
                .contains(&ListenerResource::Media)
        );
        assert!(
            config.listeners.listeners[0]
                .resources
                .contains(&ListenerResource::Client)
        );
    }

    #[test]
    fn translates_postgres_database() {
        let yaml = r#"
server_name: example.org
database:
  name: psycopg2
  args:
    host: db.internal
    database: synapse
    user: synapse
    password: hunter2
    cp_max: 20
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        match config.storage {
            StorageConfig::Postgres(p) => {
                assert_eq!(p.host, "db.internal");
                assert_eq!(p.pool_size, 20);
                assert_eq!(p.password.as_str(), Some("hunter2"));
            }
            other => panic!("expected Postgres, got {other:?}"),
        }
    }

    #[test]
    fn translates_registration_shared_secret_inline() {
        // The inline form; the kitchen-sink corpus fixture uses the
        // `_path` form instead (the two cannot coexist in one file).
        let yaml = "server_name: example.org\nregistration_shared_secret: inline-secret\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(
            config.auth.registration_shared_secret.as_str(),
            Some("inline-secret")
        );
    }

    #[test]
    fn translates_macaroon_secret_key_inline() {
        // The inline form; the kitchen-sink corpus fixture uses the
        // `_path` form instead (the two cannot coexist in one file).
        let yaml = "server_name: example.org\nmacaroon_secret_key: inline-macaroon\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.auth.session_secret.as_str(), Some("inline-macaroon"));
    }

    #[test]
    fn translates_matrix_authentication_service() {
        // Not exercised by the kitchen-sink fixture: hs-config refuses to
        // combine `mas_delegation` with `oidc_providers`, which that
        // fixture uses instead.
        let yaml = r#"
server_name: example.org
matrix_authentication_service:
  enabled: true
  endpoint: "http://mas.internal:8080"
  secret: mas-shared-secret
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        let mas = config.auth.mas_delegation.unwrap();
        assert_eq!(mas.endpoint, "http://mas.internal:8080");
        assert_eq!(mas.shared_secret.as_str(), Some("mas-shared-secret"));
    }

    #[test]
    fn matrix_authentication_service_disabled_is_a_no_op() {
        let yaml = "server_name: example.org\nmatrix_authentication_service:\n  enabled: false\n  endpoint: \"http://mas.internal:8080\"\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert!(config.auth.mas_delegation.is_none());
    }

    #[test]
    fn translates_msc3861_experimental_flag() {
        let yaml = r#"
server_name: example.org
experimental_features:
  msc3861:
    enabled: true
    issuer: "https://mas.internal/"
    client_secret: legacy-msc3861-secret
"#;
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        let mas = config.auth.mas_delegation.unwrap();
        assert_eq!(mas.endpoint, "https://mas.internal/");
        assert_eq!(mas.shared_secret.as_str(), Some("legacy-msc3861-secret"));
        assert!(
            report
                .outcomes
                .iter()
                .any(|o| o.key == "experimental_features.msc3861")
        );
    }

    #[test]
    fn matrix_authentication_service_wins_over_msc3861_when_both_present() {
        let yaml = r#"
server_name: example.org
matrix_authentication_service:
  enabled: true
  endpoint: "http://stable.internal:8080"
  secret: stable-secret
experimental_features:
  msc3861:
    enabled: true
    issuer: "https://legacy.internal/"
    client_secret: legacy-secret
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        let mas = config.auth.mas_delegation.unwrap();
        assert_eq!(mas.endpoint, "http://stable.internal:8080");
    }

    #[test]
    fn unrecognized_experimental_flag_blocks_by_default() {
        let yaml = "server_name: example.org\nexperimental_features:\n  msc9999999_enabled: true\n";
        let err = translate(yaml, TranslateOptions::default()).unwrap_err();
        assert!(matches!(err, TranslateError::Unsupported { .. }));
    }

    #[test]
    fn report_covers_every_source_key() {
        let yaml = "server_name: example.org\nreport_stats: true\nenable_metrics: true\n";
        let (_, report) = translate(yaml, TranslateOptions::default()).unwrap();
        let keys: Vec<&str> = report.outcomes.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"server_name"));
        assert!(keys.contains(&"report_stats"));
        assert!(keys.contains(&"enable_metrics"));
    }
}
