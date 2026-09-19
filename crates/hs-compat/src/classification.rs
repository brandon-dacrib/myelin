//! The Synapse configuration translation table, as data: every one of the
//! 229 documented top-level options and 51 `experimental_features` flags,
//! machine-generated from `docs/compat/synapse-config-table.md` (the
//! authoritative, human-maintained source — see that file's "Keeping this
//! current" section for the regeneration procedure and
//! `tools/synapse_inventory.py` for the upstream option list).
//!
//! `native` is empty for `Unsupported` rows. Where non-empty it names the
//! `hs-config` `Config` path(s) the translator writes to; for `Mapped` and
//! `Mapped (diff)` rows in [`OPTIONS`], `crate::translate` has a matching
//! translation function. `note` carries the reason code (see the table's
//! "Reason codes" section) and any option-specific detail.

/// Where a Synapse config key sits in the translation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Same behavior, same default, a named `hs-config` field.
    Mapped,
    /// A named `hs-config` field exists, but shape, granularity or default
    /// differs (`note` explains how).
    MappedDiff,
    /// No `hs-config` field. `note` gives the reason code.
    Unsupported,
}

/// One row of the translation table.
#[derive(Debug, Clone, Copy)]
pub struct KeyInfo {
    /// The Synapse config key, exactly as it appears in `homeserver.yaml`
    /// (top-level for [`OPTIONS`]; the flag name under
    /// `experimental_features:` for [`EXPERIMENTAL`]).
    pub key: &'static str,
    /// Mapped, mapped-with-a-difference, or unsupported.
    pub classification: Classification,
    /// The native `hs-config` path(s), or empty when unsupported.
    pub native: &'static str,
    /// Reason code and/or free-text note from the translation table.
    pub note: &'static str,
}

/// Looks up a top-level option by key.
pub fn lookup_option(key: &str) -> Option<&'static KeyInfo> {
    OPTIONS.iter().find(|k| k.key == key)
}

/// Looks up an `experimental_features` flag by name.
pub fn lookup_experimental(flag: &str) -> Option<&'static KeyInfo> {
    EXPERIMENTAL.iter().find(|k| k.key == flag)
}

pub const OPTIONS: &[KeyInfo] = &[
    KeyInfo {
        key: "modules",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MODULE.",
    },
    KeyInfo {
        key: "server_name",
        classification: Classification::Mapped,
        native: "`server.server_name`",
        note: "",
    },
    KeyInfo {
        key: "pid_file",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PROC.",
    },
    KeyInfo {
        key: "daemonize",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PROC.",
    },
    KeyInfo {
        key: "print_pidfile",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PROC.",
    },
    KeyInfo {
        key: "user_agent_suffix",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-http). Cosmetic `User-Agent` string override; fixed native string for now.",
    },
    KeyInfo {
        key: "use_frozen_dicts",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PY.",
    },
    KeyInfo {
        key: "web_client_location",
        classification: Classification::Unsupported,
        native: "",
        note: "Redirecting `/` to a bundled web client is not implemented; use a reverse-proxy redirect.",
    },
    KeyInfo {
        key: "public_baseurl",
        classification: Classification::Mapped,
        native: "`server.public_baseurl`",
        note: "",
    },
    KeyInfo {
        key: "serve_server_wellknown",
        classification: Classification::MappedDiff,
        native: "`server.well_known_server`",
        note: "Synapse takes a boolean and derives the advertised value from `server_name` itself (`host:443`, or `server_name` verbatim if it already names a port); the native field takes the advertised `host[:port]` directly instead. The translator (`crate::translate::derive_well_known_server`) reproduces Synapse's derivation exactly, so `serve_server_wellknown: true` is fully translated, not merely acknowledged. Unset means the route 404s (`crates/hs-cli/src/well_known.rs`).",
    },
    KeyInfo {
        key: "extra_well_known_client_content",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1. `/.well-known/matrix/client` is served from `server.public_baseurl` (`crates/hs-cli/src/well_known.rs`) but carries only `m.homeserver`; arbitrary extra keys have no native field yet.",
    },
    KeyInfo {
        key: "soft_file_limit",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PROC. `ulimit` is a deployment-platform concern.",
    },
    KeyInfo {
        key: "presence",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user). Presence enable/tuning is owned by the user-session actor's own config, not yet exposed in `hs-config`.",
    },
    KeyInfo {
        key: "require_auth_for_profile_requests",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user/profile).",
    },
    KeyInfo {
        key: "limit_profile_requests_to_users_who_share_rooms",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user/profile).",
    },
    KeyInfo {
        key: "include_profile_data_on_invite",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user/profile).",
    },
    KeyInfo {
        key: "include_profile_updates_in_sync",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user).",
    },
    KeyInfo {
        key: "allow_public_rooms_without_auth",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room directory).",
    },
    KeyInfo {
        key: "allow_public_rooms_over_federation",
        classification: Classification::Mapped,
        native: "`federation.allow_public_rooms_over_federation`",
        note: "",
    },
    KeyInfo {
        key: "default_room_version",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-state/hs-room). Room-version default selection is not yet exposed at the `hs-config` layer.",
    },
    KeyInfo {
        key: "gc_thresholds",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PY. Rust has no cyclic tracing GC to tune.",
    },
    KeyInfo {
        key: "gc_min_interval",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PY.",
    },
    KeyInfo {
        key: "filter_timeline_limit",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user, sync filters).",
    },
    KeyInfo {
        key: "block_non_admin_invites",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room policy).",
    },
    KeyInfo {
        key: "enable_search",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-search).",
    },
    KeyInfo {
        key: "ip_range_blacklist",
        classification: Classification::MappedDiff,
        native: "`federation.ip_range_blocklist`, `media.url_preview_ip_range_blocklist`",
        note: "Synapse applies one list to federation, URL previews, push and identity-server lookups at once; the native schema splits it per subsystem so each can be tuned independently. Federation and media both default to the same private-range list Synapse ships.",
    },
    KeyInfo {
        key: "ip_range_whitelist",
        classification: Classification::MappedDiff,
        native: "`federation.ip_range_allowlist`",
        note: "Same split as above; only the federation half has a native allowlist override today (media preview fetch does not yet have one — see `url_preview_ip_range_whitelist` below).",
    },
    KeyInfo {
        key: "listeners",
        classification: Classification::MappedDiff,
        native: "`listeners.listeners[]`",
        note: "Same shape (bind addresses, port, TLS, resource names, `x_forwarded`) minus the `replication` resource (R-WORKER: no workers) and Synapse's per-resource `additional_resources`.",
    },
    KeyInfo {
        key: "manhole",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PROC. No Python-REPL-style debug port.",
    },
    KeyInfo {
        key: "manhole_settings",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PROC.",
    },
    KeyInfo {
        key: "http_proxy",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-http). Outbound HTTP proxying is planned to honor the standard `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment variables rather than dedicated config keys; not yet implemented either way.",
    },
    KeyInfo {
        key: "https_proxy",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-http), paired with `http_proxy`.",
    },
    KeyInfo {
        key: "no_proxy_hosts",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-http), paired with `http_proxy`.",
    },
    KeyInfo {
        key: "matrix_authentication_service",
        classification: Classification::MappedDiff,
        native: "`auth.mas_delegation`",
        note: "Field names differ (`secret`/`secret_path` → `shared_secret`/`shared_secret_file`); `enabled` is implicit in the block being present rather than a separate boolean; `force_http2` (H2C to MAS) has no native equivalent.",
    },
    KeyInfo {
        key: "dummy_events_threshold",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room, forward-extremity maintenance).",
    },
    KeyInfo {
        key: "delete_stale_devices_after",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-e2e, device hygiene).",
    },
    KeyInfo {
        key: "email",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth/hs-push). Outbound email delivery (SMTP settings, templates, notification emails) is not implemented in Phase 0; password reset and email notifications are deferred to Phase 1.",
    },
    KeyInfo {
        key: "max_event_delay_duration",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room). MSC4140 delayed events is on the required MSC list (`PLAN.md` 10.4) but its config surface is not yet in `hs-config`.",
    },
    KeyInfo {
        key: "user_types",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth, custom user-type registry).",
    },
    KeyInfo {
        key: "admin_contact",
        classification: Classification::Mapped,
        native: "`server.admin_contact`",
        note: "",
    },
    KeyInfo {
        key: "hs_disabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-admin). A global kill switch is a plausible admin-API action, not a static config key, in this design; not yet implemented either way.",
    },
    KeyInfo {
        key: "hs_disabled_message",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-admin), paired with `hs_disabled`.",
    },
    KeyInfo {
        key: "limit_usage_by_mau",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "max_mau_value",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "mau_trial_days",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "mau_appservice_trial_days",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "mau_limit_alerting",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "mau_stats_only",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "mau_limit_reserved_threepids",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU.",
    },
    KeyInfo {
        key: "server_context",
        classification: Classification::Unsupported,
        native: "",
        note: "R-MAU. Only used in the MAU-limit-exceeded error message.",
    },
    KeyInfo {
        key: "limit_remote_rooms",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room, join-size policy).",
    },
    KeyInfo {
        key: "require_membership_for_aliases",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room directory policy).",
    },
    KeyInfo {
        key: "allow_per_room_profiles",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room profile policy).",
    },
    KeyInfo {
        key: "max_avatar_size",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media/profile). Not distinguished from `media.max_upload_size` yet.",
    },
    KeyInfo {
        key: "allowed_avatar_mimetypes",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media/profile).",
    },
    KeyInfo {
        key: "redaction_retention_period",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room retention/purge).",
    },
    KeyInfo {
        key: "redaction_allowed_period",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room retention/purge).",
    },
    KeyInfo {
        key: "forgotten_room_retention_period",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room retention/purge).",
    },
    KeyInfo {
        key: "user_ips_max_age",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth, login-IP retention).",
    },
    KeyInfo {
        key: "request_token_inhibit_3pid_errors",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth, 3PID enumeration hardening).",
    },
    KeyInfo {
        key: "next_link_domain_whitelist",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth, password-reset redirect allowlist).",
    },
    KeyInfo {
        key: "templates",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-compat/hs-auth). Custom Jinja template overrides for email/SSO pages; the native templating mechanism is not yet designed.",
    },
    KeyInfo {
        key: "retention",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room retention/purge scheduler).",
    },
    KeyInfo {
        key: "tls_certificate_path",
        classification: Classification::MappedDiff,
        native: "`listeners.listeners[].tls.certificate_path`",
        note: "Synapse has one global cert/key pair shared by every `tls: true` listener; the native schema configures TLS per listener. A single-cert Synapse deployment translates to the same path repeated on every listener that had `tls: true`.",
    },
    KeyInfo {
        key: "tls_private_key_path",
        classification: Classification::MappedDiff,
        native: "`listeners.listeners[].tls.private_key_path`",
        note: "Same as above.",
    },
    KeyInfo {
        key: "federation_verify_certificates",
        classification: Classification::Mapped,
        native: "`federation.verify_certificates`",
        note: "",
    },
    KeyInfo {
        key: "federation_client_minimum_tls_version",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation). `rustls`'s default TLS 1.2+ floor is used; not yet configurable.",
    },
    KeyInfo {
        key: "federation_certificate_verification_whitelist",
        classification: Classification::Unsupported,
        native: "",
        note: "R-SECURITY. A per-domain certificate-verification bypass list is not offered; use `federation.verify_certificates = false` globally (test/private-federation use) or a reverse proxy.",
    },
    KeyInfo {
        key: "federation_custom_ca_list",
        classification: Classification::MappedDiff,
        native: "`federation.custom_ca_certificates`",
        note: "Checked against `crates/hs-federation/src/client.rs`, which loads these PEM files into the outbound federation TLS trust store, and `crates/hs-config/src/federation.rs`'s doc comment. The shapes match (both a flat list of PEM file paths) but the trust semantics differ: Synapse's `trustRootFromCertificates` *replaces* the platform trust store with exactly this list (`refs/synapse/synapse/config/tls.py`, `refs/synapse/synapse/crypto/context_factory.py`), while the native field adds these CAs *on top of* the ~140 bundled public roots. An operator relying on Synapse's replace semantics to federate only with a closed set of privately-CA'd servers gets a looser trust boundary here unless they also set `federation.verify_certificates = false` and rely on the custom CA alone being sufficient, which is not equivalent either — flagged for that operator to review, not silently translated as identical.",
    },
    KeyInfo {
        key: "federation_domain_whitelist",
        classification: Classification::Mapped,
        native: "`federation.domain_allowlist`",
        note: "",
    },
    KeyInfo {
        key: "federation_whitelist_endpoint_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-compat). The `GET /_synapse/client/v1/config/federation_whitelist` diagnostic endpoint is not yet implemented.",
    },
    KeyInfo {
        key: "federation_metrics_domains",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-telemetry). Per-domain federation metric breakdown is not yet in the telemetry schema.",
    },
    KeyInfo {
        key: "allow_profile_lookup_over_federation",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation, profile-query policy).",
    },
    KeyInfo {
        key: "allow_device_name_lookup_over_federation",
        classification: Classification::Mapped,
        native: "`federation.allow_device_name_lookup_over_federation`",
        note: "",
    },
    KeyInfo {
        key: "federation",
        classification: Classification::MappedDiff,
        native: "`federation.client_timeout`, `federation.max_retry_backoff`",
        note: "Synapse tunes the short-retry (interactive) and long-retry (background) algorithms independently (`max_short_retry_delay`, `max_long_retry_delay`); the native schema exposes one backoff ceiling for both.",
    },
    KeyInfo {
        key: "event_cache_size",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room). Cache sizing is expected to be automatic (working-set based) rather than a fixed entry count; not yet implemented either way.",
    },
    KeyInfo {
        key: "caches",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1. Global/per-cache factor tuning is not yet exposed; lands with the perf-tuning section of `hs-config` once cache implementations exist across tracks.",
    },
    KeyInfo {
        key: "database",
        classification: Classification::MappedDiff,
        native: "`storage` (`Postgres` variant)",
        note: "Synapse's `name: sqlite3` has no equivalent (the embedded backend is Fjall, not SQLite); `txn_limit` and `allow_unsafe_locale` are not modeled; `args.cp_max` maps to `storage.postgres.pool_size`.",
    },
    KeyInfo {
        key: "databases",
        classification: Classification::Unsupported,
        native: "",
        note: "Database sharding across multiple Postgres hosts by table is superseded by the store's own shard mechanism (`storage.slatedb.shard_count`, `PLAN.md` section 6.5); the Postgres backend uses one database.",
    },
    KeyInfo {
        key: "log_config",
        classification: Classification::MappedDiff,
        native: "`telemetry.logging`",
        note: "Synapse points at an external Python `dictConfig` YAML file with arbitrary handlers and filters; the native schema has a fixed structured-logging output (level plus a JSON/text toggle, `telemetry.logging.level`/`telemetry.logging.json`) rather than arbitrary handler composition.",
    },
    KeyInfo {
        key: "rc_message",
        classification: Classification::Mapped,
        native: "`rate_limits.message`",
        note: "",
    },
    KeyInfo {
        key: "rc_registration",
        classification: Classification::Mapped,
        native: "`rate_limits.registration`",
        note: "",
    },
    KeyInfo {
        key: "rc_registration_token_validity",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). Registration tokens are not yet modeled.",
    },
    KeyInfo {
        key: "rc_login",
        classification: Classification::MappedDiff,
        native: "`rate_limits.login`",
        note: "Synapse has three independent buckets (`address`, `account`, `failed_attempts`); the native schema has one. A per-mechanism split is a plausible Phase 1 addition if abuse patterns require it.",
    },
    KeyInfo {
        key: "rc_admin_redaction",
        classification: Classification::Mapped,
        native: "`rate_limits.admin_redaction`",
        note: "",
    },
    KeyInfo {
        key: "rc_joins",
        classification: Classification::Mapped,
        native: "`rate_limits.joins_local`, `rate_limits.joins_remote`",
        note: "Synapse's `local`/`remote` split maps 1:1.",
    },
    KeyInfo {
        key: "rc_joins_per_room",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room). Only a per-user join rate is modeled, not a per-room ceiling.",
    },
    KeyInfo {
        key: "rc_3pid_validation",
        classification: Classification::Mapped,
        native: "`rate_limits.third_party_id_validation`",
        note: "",
    },
    KeyInfo {
        key: "rc_invites",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room). Invite-specific rate limiting is not modeled separately from `rate_limits.message`.",
    },
    KeyInfo {
        key: "rc_third_party_invite",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room).",
    },
    KeyInfo {
        key: "rc_media_create",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media). Upload-specific rate limiting is not modeled separately.",
    },
    KeyInfo {
        key: "rc_federation",
        classification: Classification::MappedDiff,
        native: "`rate_limits.federation`",
        note: "Synapse's five-field sliding-window limiter (`window_size`, `sleep_limit`, `sleep_delay`, `reject_limit`, `concurrent`) is represented as one token bucket.",
    },
    KeyInfo {
        key: "rc_presence",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user).",
    },
    KeyInfo {
        key: "rc_delayed_event_mgmt",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room, MSC4140).",
    },
    KeyInfo {
        key: "rc_reports",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room, content reports).",
    },
    KeyInfo {
        key: "rc_room_creation",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room).",
    },
    KeyInfo {
        key: "rc_user_directory",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-search).",
    },
    KeyInfo {
        key: "federation_rr_transactions_per_room_per_second",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation). Per-room read-receipt relay throttling; `rate_limits.federation` is the closest, coarser analog.",
    },
    KeyInfo {
        key: "enable_authenticated_media",
        classification: Classification::MappedDiff,
        native: "`media.allow_legacy_unauthenticated_media`",
        note: "Inverted: authenticated media is unconditional here (`PLAN.md` D6); the native flag controls whether the *legacy* unauthenticated endpoints are also served, the mirror image of Synapse's opt-in-to-the-new-thing flag.",
    },
    KeyInfo {
        key: "enable_media_repo",
        classification: Classification::MappedDiff,
        native: "`listeners.listeners[].resources`",
        note: "Disabling the whole media repo is done by omitting the `media` resource from every listener rather than a dedicated flag.",
    },
    KeyInfo {
        key: "enable_local_media_storage",
        classification: Classification::Unsupported,
        native: "",
        note: "No equivalent: `media.storage` backend selection is exclusive (local, S3, GCS or Azure), not layered with a separate on/off switch for the local tier.",
    },
    KeyInfo {
        key: "media_store_path",
        classification: Classification::Mapped,
        native: "`media.storage` (`Local` variant `path`)",
        note: "",
    },
    KeyInfo {
        key: "max_pending_media_uploads",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media, MSC2246 async uploads).",
    },
    KeyInfo {
        key: "unused_expiration_time",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media, MSC2246 async uploads).",
    },
    KeyInfo {
        key: "media_storage_providers",
        classification: Classification::MappedDiff,
        native: "`media.storage`",
        note: "Synapse's pluggable provider-module list (including the S3 storage provider) collapses to one native backend selection; `object_store` already covers S3/GCS/Azure/local (`PLAN.md` D6). Multiple simultaneous providers or write-through caching between them are not supported.",
    },
    KeyInfo {
        key: "max_upload_size",
        classification: Classification::Mapped,
        native: "`media.max_upload_size`",
        note: "",
    },
    KeyInfo {
        key: "media_upload_limits",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media). Per-mimetype upload size limits are not modeled; only the global `media.max_upload_size`.",
    },
    KeyInfo {
        key: "max_image_pixels",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media, decompression-bomb protection).",
    },
    KeyInfo {
        key: "remote_media_download_burst_count",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media).",
    },
    KeyInfo {
        key: "remote_media_download_per_second",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media).",
    },
    KeyInfo {
        key: "prevent_media_downloads_from",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media, per-server remote-media block list).",
    },
    KeyInfo {
        key: "dynamic_thumbnails",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media). Only the fixed `media.thumbnail_sizes` list is served; arbitrary on-the-fly sizes are not.",
    },
    KeyInfo {
        key: "thumbnail_sizes",
        classification: Classification::Mapped,
        native: "`media.thumbnail_sizes`",
        note: "",
    },
    KeyInfo {
        key: "media_retention",
        classification: Classification::MappedDiff,
        native: "`media.remote_media_retention`",
        note: "Synapse also has `local_media_lifetime`; the native schema only models remote-media retention so far.",
    },
    KeyInfo {
        key: "url_preview_enabled",
        classification: Classification::Mapped,
        native: "`media.url_preview_enabled`",
        note: "",
    },
    KeyInfo {
        key: "url_preview_ip_range_blacklist",
        classification: Classification::Mapped,
        native: "`media.url_preview_ip_range_blocklist`",
        note: "",
    },
    KeyInfo {
        key: "url_preview_ip_range_whitelist",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media). No allowlist override for preview fetches yet (unlike `federation.ip_range_allowlist`).",
    },
    KeyInfo {
        key: "url_preview_url_blacklist",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media). URL/domain-pattern blocklist, distinct from the IP-range blocklist.",
    },
    KeyInfo {
        key: "max_spider_size",
        classification: Classification::Mapped,
        native: "`media.url_preview_max_fetch_size`",
        note: "Checked against `crates/hs-media/src/preview.rs`'s `FetchLimits::max_body_bytes` (still a hardcoded 10 MiB constant as of this check — see that crate's status file for the one-line consumer change needed to read this field instead) and `refs/synapse/synapse/config/repository.py`'s `max_spider_size` default (`\"10M\"`), which the native field's own default matches.",
    },
    KeyInfo {
        key: "url_preview_accept_language",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media).",
    },
    KeyInfo {
        key: "oembed",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-media, oEmbed provider list).",
    },
    KeyInfo {
        key: "recaptcha_public_key",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). CAPTCHA-gated registration is not in the Phase 0 schema; tracked alongside registration tokens as a registration-hardening addition.",
    },
    KeyInfo {
        key: "recaptcha_public_key_path",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth), paired with above.",
    },
    KeyInfo {
        key: "recaptcha_private_key",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "recaptcha_private_key_path",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "enable_registration_captcha",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "recaptcha_siteverify_api",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "turn_uris",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1. TURN/MatrixRTC credential issuance (MSC4143 is on the required MSC list, `PLAN.md` 10.4) does not yet have an `hs-config` surface.",
    },
    KeyInfo {
        key: "turn_shared_secret",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `turn_uris`.",
    },
    KeyInfo {
        key: "turn_shared_secret_path",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `turn_uris`.",
    },
    KeyInfo {
        key: "turn_username",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `turn_uris`.",
    },
    KeyInfo {
        key: "turn_password",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `turn_uris`.",
    },
    KeyInfo {
        key: "turn_user_lifetime",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `turn_uris`.",
    },
    KeyInfo {
        key: "turn_allow_guests",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `turn_uris`.",
    },
    KeyInfo {
        key: "matrix_rtc",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1. MatrixRTC transport/SFU configuration for MSC4143; not yet in `hs-config`.",
    },
    KeyInfo {
        key: "enable_registration",
        classification: Classification::Mapped,
        native: "`auth.enable_registration`",
        note: "",
    },
    KeyInfo {
        key: "enable_registration_without_verification",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "registrations_require_3pid",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "disable_msisdn_registration",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "allowed_local_3pids",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "enable_3pid_lookup",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "registration_requires_token",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth, registration tokens).",
    },
    KeyInfo {
        key: "registration_shared_secret",
        classification: Classification::Mapped,
        native: "`auth.registration_shared_secret`",
        note: "Also see `docs/compat/synapse-admin-routes.md` and the shared-secret registration protocol in `hs-compat`.",
    },
    KeyInfo {
        key: "registration_shared_secret_path",
        classification: Classification::Mapped,
        native: "`auth.registration_shared_secret_file`",
        note: "",
    },
    KeyInfo {
        key: "bcrypt_rounds",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). Password hashing cost factor is not yet tunable; a fixed, at-least-as-strong default is used.",
    },
    KeyInfo {
        key: "allow_guest_access",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "default_identity_server",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth/hs-identity).",
    },
    KeyInfo {
        key: "account_threepid_delegates",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "enable_set_displayname",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user/profile).",
    },
    KeyInfo {
        key: "enable_set_avatar_url",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user/profile).",
    },
    KeyInfo {
        key: "enable_3pid_changes",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "auto_join_rooms",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room/hs-auth).",
    },
    KeyInfo {
        key: "autocreate_auto_join_rooms",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `auto_join_rooms`.",
    },
    KeyInfo {
        key: "autocreate_auto_join_rooms_federated",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `auto_join_rooms`.",
    },
    KeyInfo {
        key: "autocreate_auto_join_room_preset",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `auto_join_rooms`.",
    },
    KeyInfo {
        key: "auto_join_mxid_localpart",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `auto_join_rooms`.",
    },
    KeyInfo {
        key: "auto_join_rooms_for_guests",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `auto_join_rooms`.",
    },
    KeyInfo {
        key: "inhibit_user_in_use_error",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "allow_underscore_prefixed_registration",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "session_lifetime",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). No equivalent absolute session ceiling independent of token lifetimes.",
    },
    KeyInfo {
        key: "refreshable_access_token_lifetime",
        classification: Classification::MappedDiff,
        native: "`auth.access_token_lifetime`",
        note: "Synapse distinguishes refreshable vs. non-refreshable token lifetimes; the native schema has one `access_token_lifetime` for native OAuth tokens.",
    },
    KeyInfo {
        key: "refresh_token_lifetime",
        classification: Classification::Mapped,
        native: "`auth.refresh_token_lifetime`",
        note: "",
    },
    KeyInfo {
        key: "nonrefreshable_access_token_lifetime",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). Legacy non-refreshing logins reuse `auth.access_token_lifetime`.",
    },
    KeyInfo {
        key: "ui_auth",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth, UIA session timeout).",
    },
    KeyInfo {
        key: "login_via_existing_session",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth).",
    },
    KeyInfo {
        key: "enable_metrics",
        classification: Classification::Mapped,
        native: "`telemetry.metrics.enabled`",
        note: "",
    },
    KeyInfo {
        key: "sentry",
        classification: Classification::MappedDiff,
        native: "`telemetry.sentry`",
        note: "`dsn`/`dsn_path` → `dsn`/`dsn_file`; arbitrary extra Sentry SDK kwargs Synapse passes through are not supported.",
    },
    KeyInfo {
        key: "metrics_flags",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-telemetry). Per-metric detail toggles (e.g. `known_servers`) not yet exposed.",
    },
    KeyInfo {
        key: "report_stats",
        classification: Classification::Mapped,
        native: "`server.report_stats`",
        note: "",
    },
    KeyInfo {
        key: "report_stats_endpoint",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-telemetry). Custom stats-reporting URL override.",
    },
    KeyInfo {
        key: "room_prejoin_state",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room).",
    },
    KeyInfo {
        key: "track_puppeted_user_ips",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-appservice/hs-auth).",
    },
    KeyInfo {
        key: "app_service_config_files",
        classification: Classification::Mapped,
        native: "`appservices.registration_files`",
        note: "",
    },
    KeyInfo {
        key: "track_appservice_user_ips",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-appservice).",
    },
    KeyInfo {
        key: "use_appservice_legacy_authorization",
        classification: Classification::Unsupported,
        native: "",
        note: "R-SECURITY. Only the `Authorization: Bearer` header form of appservice auth is supported; the insecure legacy `access_token` query-parameter form is not offered, matching Synapse's own recommendation against it.",
    },
    KeyInfo {
        key: "macaroon_secret_key",
        classification: Classification::MappedDiff,
        native: "`auth.session_secret`",
        note: "Synapse's macaroon key signs guest tokens, SSO short-term login tokens and email-unsubscribe tokens using the macaroon caveat scheme specifically; the native session secret signs native OAuth-issued tokens with a different (non-macaroon) scheme. Same operational role (rotate and every session in flight is invalidated), different format.",
    },
    KeyInfo {
        key: "macaroon_secret_key_path",
        classification: Classification::Mapped,
        native: "`auth.session_secret_file`",
        note: "",
    },
    KeyInfo {
        key: "form_secret",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). CSRF-form-signing secret for the SSO fallback login page; the native SSO page implementation does not exist yet.",
    },
    KeyInfo {
        key: "form_secret_path",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `form_secret`.",
    },
    KeyInfo {
        key: "signing_key_path",
        classification: Classification::MappedDiff,
        native: "`server.signing_key_path`",
        note: "Synapse points at one file containing one active signing key; the native field is a directory, since key rotation needs more than one active key at once (see the doc comment on `ServerConfig::signing_key_path`).",
    },
    KeyInfo {
        key: "old_signing_keys",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation). Publishing retired keys so others can still verify old signatures is not yet modeled; tracked with key-rotation tooling.",
    },
    KeyInfo {
        key: "key_refresh_interval",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation, remote key cache TTL).",
    },
    KeyInfo {
        key: "trusted_key_servers",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation, notary/perspective key server list).",
    },
    KeyInfo {
        key: "suppress_key_server_warning",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1, paired with `trusted_key_servers`; not applicable until that lands.",
    },
    KeyInfo {
        key: "key_server_signing_keys_path",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-federation, acting as a notary server for others).",
    },
    KeyInfo {
        key: "saml2_config",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). SAML 2.0 SP support is not implemented in the Phase 0 auth schema, which covers OIDC upstream IdPs only (`PLAN.md` D5); tracked for Phase 1.",
    },
    KeyInfo {
        key: "oidc_providers",
        classification: Classification::MappedDiff,
        native: "`auth.oidc_providers`",
        note: "Fewer sub-options than Synapse's (no per-provider claim-mapping template, no `user_mapping_provider` Python module hook — see `modules`/R-MODULE); `idp_id`, `issuer`, `client_id`, `client_secret`/`client_secret_path` and `scopes` map directly.",
    },
    KeyInfo {
        key: "cas_config",
        classification: Classification::Unsupported,
        native: "",
        note: "R-SSO-LEGACY.",
    },
    KeyInfo {
        key: "sso",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). SSO landing-page customization (`client_whitelist`, template overrides, `update_profile_information`) is not yet in the schema; tracked with `templates`.",
    },
    KeyInfo {
        key: "jwt_config",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). `m.login.jwt` is not implemented in the Phase 0 auth schema.",
    },
    KeyInfo {
        key: "password_config",
        classification: Classification::MappedDiff,
        native: "`auth.password`",
        note: "Synapse's `localdb_enabled` (disable the local password DB while keeping password login via a custom Python provider) has no equivalent — see `modules`/R-MODULE. `enabled`, `pepper`/`pepper_path` and `policy` map directly.",
    },
    KeyInfo {
        key: "push",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-push). Delivery tuning (`include_content`, `group_unread_count_by_room`, jitter) not yet in `hs-config`.",
    },
    KeyInfo {
        key: "push_rules",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-push). Server-set default push-rule overrides.",
    },
    KeyInfo {
        key: "encryption_enabled_by_default_for_room_type",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room).",
    },
    KeyInfo {
        key: "user_directory",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-search).",
    },
    KeyInfo {
        key: "user_consent",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). Template-driven consent-tracking flow; low priority.",
    },
    KeyInfo {
        key: "stats",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room, room/user stats collection toggle).",
    },
    KeyInfo {
        key: "server_notices",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room, server-notices bot).",
    },
    KeyInfo {
        key: "enable_room_list_search",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room directory).",
    },
    KeyInfo {
        key: "alias_creation_rules",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room directory policy).",
    },
    KeyInfo {
        key: "room_list_publication_rules",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room directory policy).",
    },
    KeyInfo {
        key: "default_power_level_content_override",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room).",
    },
    KeyInfo {
        key: "forget_rooms_on_leave",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room).",
    },
    KeyInfo {
        key: "exclude_rooms_from_sync",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user/hs-room).",
    },
    KeyInfo {
        key: "exclude_rooms_from_presence",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-user).",
    },
    KeyInfo {
        key: "opentracing",
        classification: Classification::MappedDiff,
        native: "`telemetry.tracing`",
        note: "Synapse's block has per-homeserver allow lists and user-keyed sampling policy tied to its Jaeger-specific integration; the native schema has one OTLP endpoint plus a global `sample_ratio`, exported through standard OpenTelemetry (`PLAN.md` 5.5, `hs-telemetry`) so any OTLP-compatible collector works, not just Jaeger.",
    },
    KeyInfo {
        key: "worker_replication_secret",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "worker_replication_secret_path",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "start_pushers",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "pusher_instances",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "send_federation",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "federation_sender_instances",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "instance_map",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "stream_writers",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "outbound_federation_restricted_to",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "run_background_tasks_on",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "update_user_directory_from_worker",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "notify_appservices_from_worker",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "media_instance_running_background_jobs",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "redis",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER. The internal mesh (`cluster.mesh`) replaces Redis pub/sub as the replica-to-replica transport, but is not a like-for-like config mapping (different protocol, different purpose — ownership/forwarding, not a generic bus).",
    },
    KeyInfo {
        key: "worker_app",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "worker_name",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "worker_listeners",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "worker_manhole",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER, R-PROC.",
    },
    KeyInfo {
        key: "worker_daemonize",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER, R-PROC.",
    },
    KeyInfo {
        key: "worker_pid_file",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER, R-PROC.",
    },
    KeyInfo {
        key: "worker_log_config",
        classification: Classification::Unsupported,
        native: "",
        note: "R-WORKER.",
    },
    KeyInfo {
        key: "background_updates",
        classification: Classification::Unsupported,
        native: "",
        note: "Schema and data migrations run as bounded, transactional `hs-tables` migrations (`PLAN.md` section 5.5), not Python-style throttled background updates that run for hours after an upgrade; there is no equivalent throttle knob to translate.",
    },
    KeyInfo {
        key: "auto_accept_invites",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-room/hs-appservice). Useful for bridges; a plausible near-term addition, not yet in `hs-config`.",
    },
];

pub const EXPERIMENTAL: &[KeyInfo] = &[
    KeyInfo {
        key: "msc1763_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Room retention policies; see `retention` above (R-PHASE1).",
    },
    KeyInfo {
        key: "msc1767_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Extensible events.",
    },
    KeyInfo {
        key: "msc2409_to_device_messages_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Appservice family, required (`PLAN.md` 10.4).",
    },
    KeyInfo {
        key: "msc2654_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Unread notification counts are unconditional once `/sync` ships.",
    },
    KeyInfo {
        key: "msc2815_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. View-redacted-content, required (`PLAN.md` 10.4).",
    },
    KeyInfo {
        key: "msc3026_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Busy presence state.",
    },
    KeyInfo {
        key: "msc3202_transaction_extensions",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Appservice family, required.",
    },
    KeyInfo {
        key: "msc3381_polls_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Polls are primarily a client/event-type feature; no server gate planned.",
    },
    KeyInfo {
        key: "msc3391_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Account-data deletion, required.",
    },
    KeyInfo {
        key: "msc3575_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Legacy (non-simplified) sliding sync; superseded in the spec by MSC4186, which is what `PLAN.md` 10.4 commits to.",
    },
    KeyInfo {
        key: "msc3664_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Push rule for `m.in_reply_to`.",
    },
    KeyInfo {
        key: "msc3720_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Account status lookup.",
    },
    KeyInfo {
        key: "msc3773_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Thread-unread notifications are a stable spec feature, served unconditionally once sync/threads ship.",
    },
    KeyInfo {
        key: "msc3814_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Dehydrated devices, required.",
    },
    KeyInfo {
        key: "msc3848_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Additional standard error codes are used unconditionally.",
    },
    KeyInfo {
        key: "msc3861",
        classification: Classification::MappedDiff,
        native: "`auth.mas_delegation`, native OAuth issuer",
        note: "Superseded by the native OAuth 2.0 authorization server (`PLAN.md` D5) and `auth.mas_delegation` for delegation mode; msc3861's own sub-keys (introspection endpoint, client ID, etc.) are subsumed by those two mechanisms rather than translated field-for-field.",
    },
    KeyInfo {
        key: "msc3866",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-auth). Registration-token gating is not yet in the Phase 0 schema (see `registration_requires_token`).",
    },
    KeyInfo {
        key: "msc3874_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Filtering `/messages` by relation type.",
    },
    KeyInfo {
        key: "msc3881_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Pusher enable/disable, required.",
    },
    KeyInfo {
        key: "msc3890_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Remote push toggle on logout.",
    },
    KeyInfo {
        key: "msc3912_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Relation-based redactions.",
    },
    KeyInfo {
        key: "msc3983_appservice_otk_claims",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Appservice family, required.",
    },
    KeyInfo {
        key: "msc3984_appservice_key_query",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Appservice family, required.",
    },
    KeyInfo {
        key: "msc4028_push_encrypted_events",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Push for encrypted events, required.",
    },
    KeyInfo {
        key: "msc4069_profile_inhibit_propagation",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4076_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. `invite_room_state` on knocks.",
    },
    KeyInfo {
        key: "msc4108_delegation_endpoint",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. QR login rendezvous, required.",
    },
    KeyInfo {
        key: "msc4108_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. QR login rendezvous, required.",
    },
    KeyInfo {
        key: "msc4133_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Extended profiles, required.",
    },
    KeyInfo {
        key: "msc4143_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. MatrixRTC, required.",
    },
    KeyInfo {
        key: "msc4155_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Invite filtering, required.",
    },
    KeyInfo {
        key: "msc4169_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4210_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Removal of legacy `m.room.aliases` from state.",
    },
    KeyInfo {
        key: "msc4222_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. `state_after`, required.",
    },
    KeyInfo {
        key: "msc4235_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. `via` field on membership events.",
    },
    KeyInfo {
        key: "msc4242_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-PHASE1 (hs-state), uniquely expected to gain a real opt-in flag: `PLAN.md` 10.4 names MSC4242 state DAGs as the one feature that ships \"experimental\" here too, unlike every other MSC in this table. Not yet in `hs-config`; when added it will not use `experimental_features`-style nesting, just its own key (e.g. under a future `federation` or `rooms` section).",
    },
    KeyInfo {
        key: "msc4263_limit_key_queries_to_users_who_share_rooms",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4267_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4277_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4293_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED. Redact-on-kick/ban.",
    },
    KeyInfo {
        key: "msc4306_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Thread subscriptions, required.",
    },
    KeyInfo {
        key: "msc4354_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOFLAG. Sticky events, required.",
    },
    KeyInfo {
        key: "msc4370_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4388_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4388_mode",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED, paired with `msc4388_enabled`.",
    },
    KeyInfo {
        key: "msc4446_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4450_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4452_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4491_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4502_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
    KeyInfo {
        key: "msc4512_enabled",
        classification: Classification::Unsupported,
        native: "",
        note: "R-NOTPLANNED.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_synapse_option_and_flag_is_present() {
        assert_eq!(
            OPTIONS.len(),
            229,
            "docs/synapse-inventory.md documents 229 top-level options"
        );
        assert_eq!(
            EXPERIMENTAL.len(),
            51,
            "docs/synapse-inventory.md documents 51 experimental_features flags"
        );
    }

    #[test]
    fn no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for k in OPTIONS.iter().chain(EXPERIMENTAL.iter()) {
            assert!(seen.insert(k.key), "duplicate key {}", k.key);
        }
    }

    #[test]
    fn mapped_rows_always_name_a_native_path() {
        for k in OPTIONS.iter().chain(EXPERIMENTAL.iter()) {
            if matches!(
                k.classification,
                Classification::Mapped | Classification::MappedDiff
            ) {
                assert!(
                    !k.native.is_empty(),
                    "{} is mapped but has no native path",
                    k.key
                );
            }
        }
    }

    #[test]
    fn unsupported_rows_never_name_a_native_path() {
        for k in OPTIONS.iter().chain(EXPERIMENTAL.iter()) {
            if k.classification == Classification::Unsupported {
                assert!(
                    k.native.is_empty(),
                    "{} is unsupported but names a native path",
                    k.key
                );
            }
        }
    }

    #[test]
    fn lookup_finds_known_keys() {
        assert!(lookup_option("server_name").is_some());
        assert!(lookup_option("not_a_real_key").is_none());
        assert!(lookup_experimental("msc4242_enabled").is_some());
    }
}
