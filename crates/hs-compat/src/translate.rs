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

use hs_config::auth::{
    CasConfig, MasDelegationConfig, OidcProviderConfig, PasswordConfig, PasswordPolicy,
};
use hs_config::listeners::{Listener, ListenerResource, TlsConfig};
use hs_config::media::{MediaStorageBackend, ThumbnailMethod, ThumbnailSize};
use hs_config::ratelimit::RateLimitBucket;
use hs_config::secret::SecretString;
use hs_config::storage::{PostgresSslMode, PostgresStorageConfig, StorageConfig};
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
    // `server_name` is extracted first (rather than left to pass 2's file-order-dependent loop)
    // because `serve_server_wellknown`'s translation below derives its value from it, and a YAML
    // mapping's key order should not decide whether that derivation sees the right server name.
    if let Some(s) = get_str(&doc, "server_name") {
        config.server.server_name = s;
    }
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
        if key == "email" {
            translate_email(v, &mut config, &mut report);
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
// The `email` block
// ---------------------------------------------------------------------

/// Synapse's `email` sub-keys this translator writes onto the native `email` section.
const EMAIL_MAPPED: &[&str] = &[
    "smtp_host",
    "smtp_port",
    "smtp_user",
    "smtp_pass",
    "force_tls",
    "require_transport_security",
    "enable_tls",
    "tlsname",
    "notif_from",
    "app_name",
    "enable_notifs",
    "client_base_url",
    "riot_base_url",
    "notif_delay_before_mail",
    "subjects",
];

/// Synapse's email-validation settings (password reset, registration and adding an address by
/// email), which go with a feature this server does not have.
const EMAIL_VALIDATION: &[&str] = &[
    "validation_token_lifetime",
    "invite_client_location",
    "password_reset_template_html",
    "password_reset_template_text",
    "registration_template_html",
    "registration_template_text",
    "already_in_use_template_html",
    "already_in_use_template_text",
    "add_threepid_template_html",
    "add_threepid_template_text",
    "password_reset_template_failure_html",
    "registration_template_failure_html",
    "add_threepid_template_failure_html",
    "password_reset_template_success_html",
    "registration_template_success_html",
    "add_threepid_template_success_html",
];

/// Synapse's template overrides for the notification and account-expiry emails.
const EMAIL_TEMPLATES: &[&str] = &[
    "template_dir",
    "notif_template_html",
    "notif_template_text",
    "expiry_template_html",
    "expiry_template_text",
];

/// Synapse's `email.subjects` keys, which the native `email.notifications.subjects` keeps.
const EMAIL_SUBJECTS: &[&str] = &[
    "message_from_person_in_room",
    "message_from_person",
    "messages_from_person",
    "messages_in_room",
    "messages_in_room_and_others",
    "messages_from_person_and_others",
    "invite_from_person",
    "invite_from_person_to_room",
];

/// Translates Synapse's `email` block onto the native `email` section, recording `email`
/// itself and, for each sub-key that cannot be honoured, an `email.<key>` entry of its own
/// (unsupported, so it blocks without `--allow-unsupported-synapse-config`, as a top-level key
/// would).
///
/// Synapse's own defaults are carried over where the native ones differ, so a translated
/// server mails as the Synapse one did: `smtp_host` defaults to `localhost` and `smtp_port` to
/// 25 (465 with `force_tls`); notification emails are off unless `enable_notifs` is set; the
/// first email waits `notif_delay_before_mail`, ten minutes by default. `%(app)s` in
/// `notif_from` is replaced by `app_name`. The TLS booleans become one `smtp.security`:
/// `force_tls` is `tls`, `enable_tls: false` is `none`, and anything else `starttls`, which
/// here always requires the upgrade where Synapse, without `require_transport_security`, would
/// send in the clear to a server that does not offer it.
fn translate_email(value: &Value, config: &mut Config, report: &mut TranslationReport) {
    use hs_config::email::SmtpSecurity;

    let note = classification::lookup_option("email").map_or("", |info| info.note);
    report.record("email", OutcomeClassification::MappedDiff, "email", note);
    let Some(mapping) = value.as_mapping() else {
        return;
    };
    for (k, v) in mapping {
        let Some(sub) = k.as_str() else { continue };
        let dotted = format!("email.{sub}");
        if EMAIL_MAPPED.contains(&sub) {
            continue;
        }
        if sub == "notif_for_new_users" {
            // Synapse adds an email pusher for each new user who registers with an address;
            // nothing here does, which is what `false` asks for.
            if v.as_bool() != Some(false) {
                report.record(
                    dotted,
                    OutcomeClassification::Unsupported,
                    "",
                    "R-PHASE1 (hs-auth/hs-push). New users get no email pusher automatically:                      a user sets one (or an administrator binds their address and they do).",
                );
            }
            continue;
        }
        let (classification, note) = if EMAIL_VALIDATION.contains(&sub) {
            (
                OutcomeClassification::Unsupported,
                "R-PHASE1 (hs-auth). Email validation (password reset, registration and adding                  an address by email) is not implemented; the `email` section sends                  notification emails only.",
            )
        } else if EMAIL_TEMPLATES.contains(&sub) {
            (
                OutcomeClassification::Unsupported,
                "R-PHASE1 (hs-push). The notification email is built by the server, not from                  templates; only its subjects (`email.notifications.subjects`) are configurable.",
            )
        } else {
            (
                OutcomeClassification::Unrecognized,
                "not an `email` option of the pinned Synapse release",
            )
        };
        report.record(dotted, classification, "", note);
    }

    let email = &mut config.email;
    let force_tls = get_bool(value, "force_tls").unwrap_or(false);
    let enable_tls = get_bool(value, "enable_tls").unwrap_or(true);
    email.smtp.host = Some(get_str(value, "smtp_host").unwrap_or_else(|| "localhost".to_owned()));
    email.smtp.port = get_u64(value, "smtp_port")
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(if force_tls { 465 } else { 25 });
    email.smtp.security = if force_tls {
        SmtpSecurity::Tls
    } else if enable_tls {
        SmtpSecurity::Starttls
    } else {
        SmtpSecurity::None
    };
    // Synapse authenticates only with both; a password alone is ignored there, and refused here.
    if let (Some(user), Some(pass)) = (get_str(value, "smtp_user"), get_str(value, "smtp_pass")) {
        email.smtp.username = Some(user);
        email.smtp.password = SecretString::from(pass);
    }
    email.smtp.tls_name = get_str(value, "tlsname");
    if let Some(app_name) = get_str(value, "app_name") {
        email.app_name = app_name;
    }
    email.from = get_str(value, "notif_from").map(|from| from.replace("%(app)s", &email.app_name));
    email.client_base_url =
        get_str(value, "client_base_url").or_else(|| get_str(value, "riot_base_url"));
    let notifications = &mut email.notifications;
    notifications.enabled = get_bool(value, "enable_notifs").unwrap_or(false);
    notifications.delay_before_mail =
        get_duration(value, "notif_delay_before_mail").unwrap_or(Duration::from_mins(10));
    if let Some(subjects) = get(value, "subjects") {
        let target = &mut notifications.subjects;
        for key in EMAIL_SUBJECTS {
            let Some(subject) = get_str(subjects, key) else {
                continue;
            };
            let field = match *key {
                "message_from_person_in_room" => &mut target.message_from_person_in_room,
                "message_from_person" => &mut target.message_from_person,
                "messages_from_person" => &mut target.messages_from_person,
                "messages_in_room" => &mut target.messages_in_room,
                "messages_in_room_and_others" => &mut target.messages_in_room_and_others,
                "messages_from_person_and_others" => &mut target.messages_from_person_and_others,
                "invite_from_person" => &mut target.invite_from_person,
                _ => &mut target.invite_from_person_to_room,
            };
            *field = subject;
        }
        if let Some(map) = subjects.as_mapping() {
            for k in map.keys().filter_map(Value::as_str) {
                if !EMAIL_SUBJECTS.contains(&k) {
                    report.record(
                        format!("email.subjects.{k}"),
                        OutcomeClassification::Unrecognized,
                        "",
                        "not an `email.subjects` key of the pinned Synapse release",
                    );
                }
            }
        }
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

/// Derives the value `serve_server_wellknown: true` should advertise, matching Synapse's own
/// `parse_server_name` + `ServerWellKnownResource` behavior
/// (`refs/synapse/synapse/rest/well_known.py`, `refs/synapse/synapse/util/__init__.py`'s
/// `parse_server_name`): `server_name` already carrying an explicit `host:port` is used verbatim,
/// otherwise `:443` (Synapse's federation default port, not `:8448`) is appended.
fn derive_well_known_server(server_name: &str) -> String {
    let has_explicit_port = server_name
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if has_explicit_port {
        server_name.to_owned()
    } else {
        format!("{server_name}:443")
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
        // Synapse derives the advertised `.well-known/matrix/server` value from `server_name`
        // itself (`refs/synapse/synapse/rest/well_known.py`'s `ServerWellKnownResource.__init__`:
        // `host, port = parse_server_name(server_name)`, `port` defaulting to 443), rather than
        // taking it as a separate setting the way the native `server.well_known_server` field
        // does. `server_name` is guaranteed already translated by the time this arm runs (see
        // pass 1's comment in `translate`), so the derivation below matches Synapse's own
        // behavior exactly rather than leaving the field unset.
        "serve_server_wellknown" => {
            if v.as_bool() == Some(true) {
                config.server.well_known_server =
                    Some(derive_well_known_server(&config.server.server_name));
            }
        }
        "public_baseurl" => config.server.public_baseurl = v.as_str().map(str::to_owned),
        "admin_contact" => config.server.admin_contact = v.as_str().map(str::to_owned),
        // Nothing to carry over (`hs_config::retired`): this server sends no usage statistics,
        // and keeps sessions and tokens in its store, so nothing is signed with a macaroon key.
        "report_stats" | "macaroon_secret_key" | "macaroon_secret_key_path" => {}
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
        "federation_custom_ca_list" => {
            if let Some(seq) = v.as_sequence() {
                config.federation.custom_ca_certificates = seq
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect();
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
        "max_image_pixels" => {
            // A Synapse "byte size" (`32M` is 32 x 1024 x 1024), counting pixels here.
            if let Some(sz) = get_bytesize_value(v) {
                config.media.max_image_pixels = sz.as_u64();
            }
        }
        "max_spider_size" => {
            if let Some(sz) = get_bytesize_value(v) {
                config.media.url_preview_max_fetch_size = sz;
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
        "allow_guest_access" => {
            if let Some(b) = v.as_bool() {
                config.auth.allow_guest_access = b;
            }
        }
        "registration_shared_secret" => {
            if let Some(s) = v.as_str() {
                config.auth.registration_shared_secret = SecretString::from(s);
            }
        }
        "enable_registration_captcha" => {
            if let Some(b) = v.as_bool() {
                config.auth.recaptcha.required = b;
            }
        }
        "recaptcha_public_key" => {
            if let Some(s) = v.as_str() {
                config.auth.recaptcha.public_key = Some(s.to_owned());
            }
        }
        "recaptcha_private_key" => {
            if let Some(s) = v.as_str() {
                config.auth.recaptcha.private_key = SecretString::from(s);
            }
        }
        "recaptcha_private_key_path" => {
            if let Some(s) = v.as_str() {
                config.auth.recaptcha.private_key_file = Some(PathBuf::from(s));
            }
        }
        "recaptcha_siteverify_api" => {
            if let Some(s) = v.as_str() {
                config.auth.recaptcha.siteverify_api = s.to_owned();
            }
        }
        "registration_shared_secret_path" => {
            if let Some(s) = v.as_str() {
                config.auth.registration_shared_secret_file = Some(PathBuf::from(s));
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
            // `only_for_reauth` is what the native `false` does: no password login, and a
            // password still confirms a sensitive change.
            if let Some(b) = get_bool(v, "enabled") {
                password.enabled = b;
            } else if get_str(v, "enabled").as_deref() == Some("only_for_reauth") {
                password.enabled = false;
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
        "cas_config" => apply_cas(v, config),
        "sso" => {
            if let Some(clients) = get_str_list(v, "client_whitelist") {
                config.auth.sso.client_whitelist = clients;
            }
            if let Some(update) = get_bool(v, "update_profile_information") {
                config.auth.sso.update_profile_information = update;
            }
        }
        "next_link_domain_whitelist" => {
            config.auth.next_link_domain_whitelist = get_str_list_value(v);
        }

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
    // libpq's own `sslmode` and `sslrootcert`, which Synapse passes through to psycopg2
    // untouched; the native names are the same five, and libpq's `allow` (TLS only if the server
    // insists) has no native equivalent nearer than `prefer`.
    let ssl_mode = match get_str(&args, "sslmode").as_deref() {
        Some("disable") => PostgresSslMode::Disable,
        Some("require") => PostgresSslMode::Require,
        Some("verify-ca") => PostgresSslMode::VerifyCa,
        Some("verify-full") => PostgresSslMode::VerifyFull,
        Some("prefer" | "allow") | None => PostgresSslMode::Prefer,
        Some(_) => PostgresSslMode::Prefer,
    };
    let ssl_root_cert = get_str(&args, "sslrootcert")
        .filter(|_| {
            matches!(
                ssl_mode,
                PostgresSslMode::VerifyCa | PostgresSslMode::VerifyFull
            )
        })
        .map(PathBuf::from);
    config.storage = StorageConfig::Postgres(PostgresStorageConfig {
        host,
        port,
        database,
        user,
        password,
        password_file: None,
        pool_size,
        schema: "public".to_owned(),
        ssl_mode,
        ssl_root_cert,
    });
}

/// Synapse's `cas_config` onto `auth.cas`. `enabled: false` (or no `server_url`) leaves CAS
/// off. `protocol_version`, `enable_registration`, `allow_numeric_ids` and `numeric_ids_prefix`
/// carry over with Synapse's defaults; `idp_icon` and `idp_brand` have no native counterpart
/// (the classification row says so).
fn apply_cas(v: &Value, config: &mut Config) {
    if get_bool(v, "enabled") == Some(false) {
        return;
    }
    let Some(server_url) = get_str(v, "server_url") else {
        return;
    };
    let required_attributes = get(v, "required_attributes")
        .and_then(Value::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| {
                    Some((k.as_str()?.to_owned(), val.as_str().map(str::to_owned)))
                })
                .collect()
        })
        .unwrap_or_default();
    config.auth.cas = Some(CasConfig {
        server_url,
        service_url: get_str(v, "service_url"),
        displayname_attribute: get_str(v, "displayname_attribute"),
        required_attributes,
        idp_name: get_str(v, "idp_name").unwrap_or_else(|| "CAS".to_owned()),
        protocol_version: get_u64(v, "protocol_version").and_then(|n| u8::try_from(n).ok()),
        enable_registration: get_bool(v, "enable_registration").unwrap_or(true),
        allow_numeric_ids: get_bool(v, "allow_numeric_ids").unwrap_or(false),
        numeric_ids_prefix: get_str(v, "numeric_ids_prefix").unwrap_or_else(|| "u".to_owned()),
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

    /// A Synapse `email` block as its documentation shows one, with notifications on.
    const SYNAPSE_EMAIL: &str = "server_name: example.org
email:
  smtp_host: mail.example.org
  smtp_port: 587
  smtp_user: exampleusername
  smtp_pass: examplepassword
  require_transport_security: true
  tlsname: smtp.example.com
  notif_from: \"Your Friendly %(app)s homeserver <noreply@example.com>\"
  app_name: my_branded_matrix_server
  enable_notifs: true
  notif_for_new_users: false
  client_base_url: \"http://localhost/riot\"
  notif_delay_before_mail: 5m
  subjects:
    message_from_person_in_room: \"[%(app)s] You have a message from %(person)s...\"
";

    #[test]
    fn the_email_block_becomes_the_email_section() {
        use hs_config::email::SmtpSecurity;
        let (config, report) = translate(SYNAPSE_EMAIL, TranslateOptions::default()).unwrap();
        let email = &config.email;
        assert_eq!(email.smtp.host.as_deref(), Some("mail.example.org"));
        assert_eq!(email.smtp.port, 587);
        assert_eq!(email.smtp.security, SmtpSecurity::Starttls);
        assert_eq!(email.smtp.username.as_deref(), Some("exampleusername"));
        assert_eq!(email.smtp.password.as_str(), Some("examplepassword"));
        assert_eq!(email.smtp.tls_name.as_deref(), Some("smtp.example.com"));
        assert_eq!(
            email.from.as_deref(),
            Some("Your Friendly my_branded_matrix_server homeserver <noreply@example.com>"),
            "%(app)s is filled in"
        );
        assert_eq!(email.app_name, "my_branded_matrix_server");
        assert_eq!(
            email.client_base_url.as_deref(),
            Some("http://localhost/riot")
        );
        assert!(email.notifications.enabled);
        assert_eq!(
            email.notifications.delay_before_mail,
            Duration::from_mins(5)
        );
        assert_eq!(
            email.notifications.subjects.message_from_person_in_room,
            "[%(app)s] You have a message from %(person)s..."
        );
        assert_eq!(
            email.notifications.subjects.messages_in_room,
            hs_config::email::SubjectsConfig::default().messages_in_room,
            "an unset subject keeps the default"
        );
        assert!(!report.has_blocking(), "{}", report.to_markdown());
        assert!(
            report
                .outcomes
                .iter()
                .any(|o| o.key == "email" && o.classification == OutcomeClassification::MappedDiff)
        );
    }

    #[test]
    fn synapses_email_defaults_carry_over() {
        use hs_config::email::SmtpSecurity;
        let yaml = "server_name: example.org\nemail:\n  notif_from: noreply@example.org\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        let email = &config.email;
        assert_eq!(email.smtp.host.as_deref(), Some("localhost"));
        assert_eq!(email.smtp.port, 25);
        assert_eq!(email.smtp.security, SmtpSecurity::Starttls);
        assert!(
            !email.notifications.enabled,
            "Synapse's enable_notifs is off by default"
        );
        assert_eq!(
            email.notifications.delay_before_mail,
            Duration::from_mins(10)
        );
        let yaml =
            "server_name: example.org\nemail:\n  notif_from: n@example.org\n  force_tls: true\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.email.smtp.port, 465);
        assert_eq!(config.email.smtp.security, SmtpSecurity::Tls);
        let yaml =
            "server_name: example.org\nemail:\n  notif_from: n@example.org\n  enable_tls: false\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.email.smtp.security, SmtpSecurity::None);
        // No email block: no SMTP server, as before.
        let (config, _) =
            translate("server_name: example.org\n", TranslateOptions::default()).unwrap();
        assert!(config.email.smtp.host.is_none());
    }

    #[test]
    fn email_settings_with_no_counterpart_block_by_default() {
        for (sub, value) in [
            ("validation_token_lifetime", "15m"),
            ("template_dir", "/templates"),
            ("notif_for_new_users", "true"),
            ("not_an_email_option", "1"),
        ] {
            let yaml = format!(
                "server_name: example.org\nemail:\n  notif_from: n@example.org\n  {sub}: {value}\n"
            );
            let err = translate(&yaml, TranslateOptions::default()).unwrap_err();
            let TranslateError::Unsupported { keys } = err else {
                panic!("{sub}: expected a blocking report");
            };
            assert!(
                keys.iter().any(|k| k.contains(&format!("email.{sub}"))),
                "{sub}: {keys:?}"
            );
            let (config, _) = translate(
                &yaml,
                TranslateOptions {
                    allow_unsupported: true,
                },
            )
            .unwrap();
            assert_eq!(config.email.from.as_deref(), Some("n@example.org"));
        }
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
    sslmode: verify-full
    sslrootcert: /etc/synapse/pg-ca.pem
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        match config.storage {
            StorageConfig::Postgres(p) => {
                assert_eq!(p.host, "db.internal");
                assert_eq!(p.pool_size, 20);
                assert_eq!(p.password.as_str(), Some("hunter2"));
                assert_eq!(p.ssl_mode, PostgresSslMode::VerifyFull);
                assert_eq!(
                    p.ssl_root_cert.as_deref(),
                    Some(std::path::Path::new("/etc/synapse/pg-ca.pem"))
                );
                assert_eq!(p.schema, "public");
            }
            other => panic!("expected Postgres, got {other:?}"),
        }
    }

    #[test]
    fn translates_postgres_sslmode_defaults_and_drops_an_unused_root_cert() {
        let yaml = r#"
server_name: example.org
database:
  name: psycopg2
  args:
    host: db.internal
    sslmode: require
    sslrootcert: /etc/synapse/pg-ca.pem
"#;
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        let StorageConfig::Postgres(p) = config.storage else {
            panic!("expected Postgres");
        };
        assert_eq!(p.ssl_mode, PostgresSslMode::Require);
        // `require` does not read a root certificate natively (no libpq-style promotion to
        // verify-ca), and the native validator refuses the pair, so the path is dropped.
        assert_eq!(p.ssl_root_cert, None);

        let yaml =
            "server_name: example.org\ndatabase:\n  name: psycopg2\n  args:\n    sslmode: allow\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        let StorageConfig::Postgres(p) = config.storage else {
            panic!("expected Postgres");
        };
        assert_eq!(p.ssl_mode, PostgresSslMode::Prefer);
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
    fn translates_the_sso_client_whitelist_and_next_link_domains() {
        let yaml = "server_name: example.org\n\
                    sso:\n  client_whitelist: [\"https://app.example/\"]\n  \
                    update_profile_information: true\n\
                    next_link_domain_whitelist: [app.example]\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        assert!(!report.has_blocking());
        assert_eq!(config.auth.sso.client_whitelist, ["https://app.example/"]);
        assert!(config.auth.sso.update_profile_information);
        assert_eq!(
            config.auth.next_link_domain_whitelist,
            Some(vec!["app.example".to_owned()])
        );
        let (unset, _) = translate(
            "server_name: example.org\nnext_link_domain_whitelist: null\n",
            TranslateOptions::default(),
        )
        .unwrap();
        assert_eq!(unset.auth.next_link_domain_whitelist, None);
    }

    #[test]
    fn translates_the_recaptcha_settings() {
        let yaml = "server_name: example.org\nenable_registration_captcha: true\n\
                    recaptcha_public_key: site-key\nrecaptcha_private_key: secret-key\n\
                    recaptcha_siteverify_api: https://captcha.example/siteverify\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        assert!(!report.has_blocking());
        let captcha = &config.auth.recaptcha;
        assert!(captcha.required);
        assert_eq!(captcha.public_key.as_deref(), Some("site-key"));
        assert_eq!(captcha.private_key.as_str(), Some("secret-key"));
        assert_eq!(captcha.siteverify_api, "https://captcha.example/siteverify");
    }

    #[test]
    fn translates_cas_config() {
        let yaml = "server_name: example.org\npublic_baseurl: https://example.org/\ncas_config:\n  enabled: true\n  server_url: https://cas.example.edu/cas\n  displayname_attribute: name\n  required_attributes:\n    userGroup: staff\n    department: ~\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        let cas = config.auth.cas.expect("cas_config maps to auth.cas");
        assert_eq!(cas.server_url, "https://cas.example.edu/cas");
        assert_eq!(cas.displayname_attribute.as_deref(), Some("name"));
        assert_eq!(cas.idp_name, "CAS");
        assert_eq!(
            cas.required_attributes.get("userGroup"),
            Some(&Some("staff".to_owned()))
        );
        assert_eq!(cas.required_attributes.get("department"), Some(&None));
        // Synapse's defaults for what the file does not say.
        assert_eq!(cas.protocol_version, None);
        assert!(cas.enable_registration);
        assert!(!cas.allow_numeric_ids);
        assert_eq!(cas.numeric_ids_prefix, "u");
        let _ = report;

        let full = "server_name: example.org
cas_config:
  server_url: https://cas.example.edu/cas
  protocol_version: 3
  enable_registration: false
  allow_numeric_ids: true
  numeric_ids_prefix: numericuser
";
        let (config, _) = translate(full, TranslateOptions::default()).unwrap();
        let cas = config.auth.cas.unwrap();
        assert_eq!(cas.protocol_version, Some(3));
        assert!(!cas.enable_registration);
        assert!(cas.allow_numeric_ids);
        assert_eq!(cas.numeric_ids_prefix, "numericuser");

        let disabled = "server_name: example.org\ncas_config:\n  enabled: false\n  server_url: https://cas.example.edu/cas\n";
        let (config, _) = translate(disabled, TranslateOptions::default()).unwrap();
        assert!(config.auth.cas.is_none());
    }

    #[test]
    fn the_keys_this_server_does_not_need_translate_to_nothing_and_say_so() {
        // `report_stats` and the macaroon key are in nearly every Synapse config; neither has a
        // native setting any more, and neither blocks a translation.
        let yaml = "server_name: example.org\nreport_stats: true\nmacaroon_secret_key: inline-macaroon\npassword_config:\n  enabled: only_for_reauth\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        assert!(!report.has_blocking());
        for key in ["report_stats", "macaroon_secret_key"] {
            let outcome = report.outcomes.iter().find(|o| o.key == key).unwrap();
            assert!(
                outcome.note.starts_with("Not needed"),
                "{key}: {}",
                outcome.note
            );
        }
        assert!(
            !config.auth.password.enabled,
            "only_for_reauth is the native false"
        );
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

    #[test]
    fn serve_server_wellknown_derives_host_and_default_port() {
        let yaml = "server_name: example.org\nserve_server_wellknown: true\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(
            config.server.well_known_server.as_deref(),
            Some("example.org:443")
        );
        assert!(!report.has_blocking());
    }

    #[test]
    fn serve_server_wellknown_keeps_an_explicit_port_in_server_name() {
        let yaml = "server_name: example.org:8448\nserve_server_wellknown: true\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(
            config.server.well_known_server.as_deref(),
            Some("example.org:8448")
        );
    }

    #[test]
    fn serve_server_wellknown_false_leaves_the_route_unset() {
        let yaml = "server_name: example.org\nserve_server_wellknown: false\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.server.well_known_server, None);
    }

    #[test]
    fn serve_server_wellknown_sees_server_name_regardless_of_key_order() {
        // `serve_server_wellknown` appears before `server_name` in the source file; pass 1's
        // early extraction of `server_name` (see `translate`'s doc comment on that line) must
        // make this independent of file order.
        let yaml = "serve_server_wellknown: true\nserver_name: example.org\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(
            config.server.well_known_server.as_deref(),
            Some("example.org:443")
        );
    }

    #[test]
    fn translates_federation_custom_ca_list() {
        let yaml =
            "server_name: example.org\nfederation_custom_ca_list:\n  - myCA1.pem\n  - myCA2.pem\n";
        let (config, report) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(
            config.federation.custom_ca_certificates,
            vec!["myCA1.pem".to_string(), "myCA2.pem".to_string()]
        );
        assert!(!report.has_blocking());
    }

    #[test]
    fn translates_max_image_pixels() {
        let yaml = "server_name: example.org\nmax_image_pixels: 35M\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(config.media.max_image_pixels, 35 * 1024 * 1024);
    }

    #[test]
    fn translates_max_spider_size() {
        let yaml = "server_name: example.org\nmax_spider_size: \"20M\"\n";
        let (config, _) = translate(yaml, TranslateOptions::default()).unwrap();
        assert_eq!(
            config.media.url_preview_max_fetch_size,
            hs_config::ByteSize::mib(20)
        );
    }
}
